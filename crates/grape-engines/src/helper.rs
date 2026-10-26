//! Helper processes: the plumbing an engine shares when it runs one of
//! Grape's own helper programs (the HEIC helper, DECISIONS D33). Lane A's
//! module, taken from the PDF worker's runner (`pdf/run.rs`).
//!
//! - [`WorkDir`]: a new, private folder under the OS temp folder (0700 on
//!   Unix), where the engine and its helper exchange files. It's removed
//!   with whatever is in it when dropped, so large intermediate files never
//!   land in the user's folder or on a network share.
//! - [`start`]: spawns the helper with stdin and stdout piped and stderr
//!   discarded, no console at all on Windows, the named variables removed
//!   from its environment and its working folder set. It puts the helper
//!   into the app's container ([`set_contain`]) before the helper has its
//!   request, and fails closed: a helper that can't be contained is killed.
//!   The request is written on a thread, so a helper that doesn't read
//!   can't block the job, and stdout is read as JSON lines on another.
//! - [`Running`]: the engine polls it for messages and the exit status. It
//!   kills and reaps the helper when dropped, then ends its containment.
//!
//! The containment hook is process-wide, not an [`crate::EngineCtx`] field
//! (heic-spec J15): the app sets it once at startup, and the tests of an
//! engine inject theirs through [`Launch::contain`].

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::archive::folder::remove_tree;
use crate::output::unique_suffix;

/// `DETACHED_PROCESS`: a console-subsystem helper gets no console at all,
/// so Windows starts no `conhost.exe` for it. Its stdio are pipes, so it
/// needs none, and a console host would be a second process in a job that
/// allows one (D33). `CREATE_NO_WINDOW` would still make a hidden console.
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// How many fresh names [`WorkDir::create`] tries.
const UNIQUE_ATTEMPTS: usize = 8;

/// The longest [`Running::poll`] sleeps while it waits for the exit status
/// of a helper whose stdout has closed.
const EXIT_POLL: Duration = Duration::from_millis(10);

/// Puts a helper process Grape just started into an OS container (D33):
/// a Job Object on Windows, set by the app ([`set_contain`]).
pub trait Contain: Send + Sync + fmt::Debug {
    /// Contains `child`, which hasn't been given any work yet. `max_memory`
    /// caps its committed memory, where the OS can. An `Err` says why, for
    /// the log; the engine then kills the helper and fails the job.
    fn contain(&self, child: &Child, max_memory: Option<u64>)
        -> Result<Box<dyn Contained>, String>;
}

/// A contained helper. Dropping it ends the containment (and the helper,
/// on Windows, if it's still running).
pub trait Contained: Send {
    /// The most memory the helper has committed, in bytes, if the OS
    /// reports it. The engine reads it after a crash, to tell "needs too
    /// much memory" from other failures.
    fn peak_memory(&self) -> Option<u64>;
}

/// The app's hook, set once.
static CONTAIN: OnceLock<Arc<dyn Contain>> = OnceLock::new();

/// Sets the containment hook every helper process goes through (D33). The
/// app calls it once at startup, before any job runs; later calls change
/// nothing and return false.
pub fn set_contain(hook: Arc<dyn Contain>) -> bool {
    CONTAIN.set(hook).is_ok()
}

/// The hook the app set, if any.
pub(crate) fn contain_hook() -> Option<Arc<dyn Contain>> {
    CONTAIN.get().cloned()
}

/// How to start a helper.
#[derive(Debug, Clone)]
pub(crate) struct Launch {
    /// The executable.
    pub(crate) program: PathBuf,
    /// Its arguments.
    pub(crate) args: Vec<OsString>,
    /// Variables added to the environment it inherits (tests). Removed
    /// variables ([`Start::env_remove`]) stay removed even if listed here.
    pub(crate) envs: Vec<(OsString, OsString)>,
    /// Where it's contained: the app's hook ([`contain_hook`]) for a real
    /// job, a fake in tests, `None` for no container.
    pub(crate) contain: Option<Arc<dyn Contain>>,
}

/// The rest of what [`start`] needs besides the [`Launch`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Start<'a> {
    /// The helper's working folder: its [`WorkDir`].
    pub(crate) cwd: &'a Path,
    /// Variables removed from the environment it inherits.
    pub(crate) env_remove: &'a [&'a str],
    /// The memory cap passed to [`Contain::contain`].
    pub(crate) max_memory: Option<u64>,
    /// The longest stdout line read; a longer one is skipped.
    pub(crate) max_line: usize,
    /// The prefix of the reader and writer threads' names.
    pub(crate) name: &'a str,
}

/// Why [`start`] failed. The helper, if it was spawned, is already killed.
#[derive(Debug)]
pub(crate) enum StartError {
    /// The OS couldn't start it.
    Spawn(io::ErrorKind),
    /// It couldn't be contained; the hook's reason.
    Contain(String),
    /// A pipe or a thread for it couldn't be set up.
    Plumbing(&'static str),
}

/// Spawns `launch` as [`start`]'s doc says, contains it, then hands it
/// `request` on stdin (which is closed after it) and reads its stdout as
/// one `M` per line. Lines that aren't an `M`, and lines over
/// [`Start::max_line`], are skipped.
pub(crate) fn start<M>(
    launch: &Launch,
    options: Start<'_>,
    request: Vec<u8>,
) -> Result<Running<M>, StartError>
where
    M: DeserializeOwned + Send + 'static,
{
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .envs(launch.envs.iter().map(|(k, v)| (k, v)))
        .current_dir(options.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // After `envs`, so a removed variable stays removed.
    for name in options.env_remove {
        command.env_remove(name);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(DETACHED_PROCESS);
    }
    let child = command.spawn().map_err(|e| StartError::Spawn(e.kind()))?;
    let (tx, rx) = mpsc::channel();
    // From here on, dropping `running` kills and reaps the helper.
    let mut running = Running {
        child,
        contained: None,
        rx,
        open: true,
    };
    if let Some(hook) = &launch.contain {
        // Before the request: the helper waits for it, so it has done
        // nothing yet.
        let contained = hook
            .contain(&running.child, options.max_memory)
            .map_err(StartError::Contain)?;
        running.contained = Some(contained);
    }
    let stdout = running
        .child
        .stdout
        .take()
        .ok_or(StartError::Plumbing("no stdout"))?;
    let max_line = options.max_line;
    thread::Builder::new()
        .name(format!("{}-stdout", options.name))
        .spawn(move || read_messages(stdout, max_line, tx))
        .map_err(|_| StartError::Plumbing("no reader thread"))?;
    let mut stdin = running
        .child
        .stdin
        .take()
        .ok_or(StartError::Plumbing("no stdin"))?;
    // A helper that died at once shows up when it's polled; a killed one
    // ends the write. Dropping stdin closes it: the helper reads to the end.
    thread::Builder::new()
        .name(format!("{}-stdin", options.name))
        .spawn(move || {
            let _ = stdin.write_all(&request);
        })
        .map_err(|_| StartError::Plumbing("no writer thread"))?;
    Ok(running)
}

/// A helper that [`start`] started.
pub(crate) struct Running<M> {
    /// Killed (if still running) and reaped when this is dropped.
    child: Child,
    /// Dropped after the child is reaped.
    contained: Option<Box<dyn Contained>>,
    rx: Receiver<Event<M>>,
    /// False once stdout has ended.
    open: bool,
}

/// What the stdout thread passes on.
enum Event<M> {
    Message(M),
    End,
}

/// What [`Running::poll`] saw.
#[derive(Debug)]
pub(crate) enum Polled<M> {
    /// The next message.
    Message(M),
    /// Nothing yet.
    Idle,
    /// Its stdout ended and it exited.
    Exited(ExitStatus),
}

impl<M> Running<M> {
    /// Waits up to `wait` for the next message, then for the exit once
    /// stdout has ended.
    pub(crate) fn poll(&mut self, wait: Duration) -> io::Result<Polled<M>> {
        if self.open {
            match self.rx.recv_timeout(wait) {
                Ok(Event::Message(message)) => return Ok(Polled::Message(message)),
                Ok(Event::End) | Err(RecvTimeoutError::Disconnected) => self.open = false,
                Err(RecvTimeoutError::Timeout) => return Ok(Polled::Idle),
            }
        }
        match self.child.try_wait()? {
            Some(status) => Ok(Polled::Exited(status)),
            None => {
                thread::sleep(wait.min(EXIT_POLL));
                Ok(Polled::Idle)
            }
        }
    }

    /// The container's [`Contained::peak_memory`], if it's contained.
    pub(crate) fn peak_memory(&self) -> Option<u64> {
        self.contained.as_ref().and_then(|c| c.peak_memory())
    }
}

impl<M> Drop for Running<M> {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The stdout thread: one [`Event::Message`] per line that is an `M`, then
/// [`Event::End`].
fn read_messages<M: DeserializeOwned>(stdout: ChildStdout, max_line: usize, tx: Sender<Event<M>>) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    while let Ok(Some(())) = read_line(&mut reader, &mut line, max_line) {
        if let Ok(message) = serde_json::from_slice::<M>(&line) {
            if tx.send(Event::Message(message)).is_err() {
                return;
            }
        }
    }
    let _ = tx.send(Event::End);
}

/// Reads the next line into `line` (without its `\n`), keeping at most
/// `max` bytes: a longer line comes back empty. `None` at the end.
pub(crate) fn read_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    max: usize,
) -> io::Result<Option<()>> {
    line.clear();
    let mut seen_any = false;
    let mut overlong = false;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(seen_any.then_some(()));
        }
        seen_any = true;
        let (take, end) = match buf.iter().position(|&b| b == b'\n') {
            Some(at) => (at + 1, true),
            None => (buf.len(), false),
        };
        let content = if end { &buf[..take - 1] } else { &buf[..take] };
        if !overlong {
            if line.len() + content.len() > max {
                overlong = true;
                line.clear();
            } else {
                line.extend_from_slice(content);
            }
        }
        reader.consume(take);
        if end {
            return Ok(Some(()));
        }
    }
}

/// A helper's working folder: new, empty, private (0700 on Unix), and
/// removed with whatever is in it when dropped.
#[derive(Debug)]
pub(crate) struct WorkDir(PathBuf);

impl WorkDir {
    /// Creates `{parent}/{prefix}{pid}-{nanos}-{n}`.
    pub(crate) fn create(parent: &Path, prefix: &str) -> io::Result<WorkDir> {
        let mut last = None;
        for _ in 0..UNIQUE_ATTEMPTS {
            let path = parent.join(format!("{prefix}{}", unique_suffix()));
            match make_private_dir(&path) {
                Ok(()) => return Ok(WorkDir(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::from(io::ErrorKind::AlreadyExists)))
    }

    /// The folder.
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = remove_tree(&self.0);
    }
}

/// Creates the folder `path` (never an existing one), readable only by
/// this user on Unix. On Windows the OS temp folder is already the user's.
fn make_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Cursor;

    use super::*;

    #[test]
    fn lines_are_bounded() {
        let mut input = Cursor::new(b"short\n0123456789\nlast".to_vec());
        let mut line = Vec::new();
        assert_eq!(read_line(&mut input, &mut line, 8).unwrap(), Some(()));
        assert_eq!(line, b"short");
        assert_eq!(read_line(&mut input, &mut line, 8).unwrap(), Some(()));
        assert!(line.is_empty(), "an overlong line comes back empty");
        assert_eq!(read_line(&mut input, &mut line, 8).unwrap(), Some(()));
        assert_eq!(line, b"last");
        assert_eq!(read_line(&mut input, &mut line, 8).unwrap(), None);
        // Exactly at the limit is kept.
        let mut input = Cursor::new(b"12345678\n".to_vec());
        assert_eq!(read_line(&mut input, &mut line, 8).unwrap(), Some(()));
        assert_eq!(line, b"12345678");
    }

    #[test]
    fn a_work_dir_is_new_private_and_removed_with_its_contents() {
        let parent = std::env::temp_dir().join(format!("grape-helper-unit-{}", unique_suffix()));
        fs::create_dir_all(&parent).unwrap();
        let first = WorkDir::create(&parent, "grape-test-").unwrap();
        let second = WorkDir::create(&parent, "grape-test-").unwrap();
        assert_ne!(first.path(), second.path());
        assert!(first
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("grape-test-"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(first.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        fs::create_dir(first.path().join("inner")).unwrap();
        fs::write(first.path().join("inner").join("x"), b"x").unwrap();
        let path = first.path().to_path_buf();
        drop(first);
        drop(second);
        assert!(!path.exists());
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
        fs::remove_dir(&parent).unwrap();
    }

    #[test]
    fn a_missing_program_is_a_spawn_error() {
        let dir = std::env::temp_dir();
        let launch = Launch {
            program: dir.join(format!("grape-no-such-helper-{}", unique_suffix())),
            args: Vec::new(),
            envs: Vec::new(),
            contain: None,
        };
        let options = Start {
            cwd: &dir,
            env_remove: &[],
            max_memory: None,
            max_line: 1024,
            name: "grape-test",
        };
        match start::<serde_json::Value>(&launch, options, Vec::new()) {
            Err(StartError::Spawn(kind)) => assert_eq!(kind, io::ErrorKind::NotFound),
            other => panic!("{:?}", other.err()),
        }
    }

    #[test]
    fn the_hook_is_set_once() {
        /// Contains nothing, so a test that did reach the global hook
        /// would still run its helper.
        #[derive(Debug)]
        struct Nothing;
        struct Uncontained;
        impl Contained for Uncontained {
            fn peak_memory(&self) -> Option<u64> {
                None
            }
        }
        impl Contain for Nothing {
            fn contain(&self, _: &Child, _: Option<u64>) -> Result<Box<dyn Contained>, String> {
                Ok(Box::new(Uncontained))
            }
        }
        // This is the only test that sets the global hook: the engines'
        // tests inject theirs through `Launch::contain`.
        assert!(contain_hook().is_none());
        assert!(set_contain(Arc::new(Nothing)));
        assert!(!set_contain(Arc::new(Nothing)));
        assert!(contain_hook().is_some());
    }
}
