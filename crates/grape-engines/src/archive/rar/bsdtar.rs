//! Starting bsdtar and watching it (DECISIONS D27).
//!
//! `bsdtar -c -P -f - --format pax @-` runs in a new, empty, private
//! folder with a cleared environment. Three threads serve it while the
//! job's thread reads its stdout ([`ChildOut`]):
//!
//! - the pump writes the RAR into its stdin, 64 KiB at a time, and never
//!   hands it a file handle (progress is the bytes pumped);
//! - the stderr drain keeps the first 8 KiB and counts the rest, so bsdtar
//!   never blocks on a full stderr pipe;
//! - the watchdog kills bsdtar once the job is cancelled, or when Grape has
//!   waited on it for [`STALL`] while it took no input and wrote no output.
//!
//! [`Running::finish`] reaps it, joins the threads, removes the folder and
//! judges the run. Nothing here logs bsdtar's stderr: it holds entry
//! names. The log gets the pattern it matched and a byte count.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::super::folder::remove_tree;
use super::super::stream::Run;
use super::super::Failure;
use super::{RarFailure, RarPaths};
use crate::output::unique_suffix;
use crate::CancelToken;

/// bsdtar's arguments: create (`-c`) a pax tar on stdout (`-f -`,
/// `--format pax`) from the archive on stdin (`@-`), keeping names as
/// stored (`-P`), so Grape's own check refuses `/abs` and `..\x`.
pub(crate) const ARGS: [&str; 7] = ["-c", "-P", "-f", "-", "--format", "pax", "@-"];

/// How long Grape waits on a bsdtar that takes no input and writes no
/// output before it kills it.
pub(crate) const STALL: Duration = Duration::from_secs(60);

/// The watchdog's tick (the crate's cancel rule: 50 ms).
const TICK: Duration = Duration::from_millis(50);

/// How often the exit is polled.
const POLL: Duration = Duration::from_millis(10);

/// How long a bsdtar whose stream broke may take to exit by itself.
const GRACE: Duration = Duration::from_millis(250);

/// What the pump writes at a time.
const CHUNK: usize = 64 * 1024;

/// The read buffer on bsdtar's stdout.
const STDOUT_BUF: usize = 64 * 1024;

/// How much of bsdtar's stderr is kept.
const STDERR_KEEP: usize = 8 * 1024;

/// What the stderr drain reads at a time.
const STDERR_STEP: usize = 4 * 1024;

/// How far into stdout a test preamble is looked for.
const PREAMBLE_MAX: usize = 4 * 1024;

/// `CREATE_NO_WINDOW`: bsdtar is a console program; no window opens.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How bsdtar is started.
#[derive(Debug, Clone)]
pub(crate) struct Launch {
    /// The executable, absolute; never looked up on `PATH`.
    pub(crate) program: PathBuf,
    /// [`ARGS`] for bsdtar.
    pub(crate) args: Vec<OsString>,
    /// Tests only: added after Grape's own variables.
    pub(crate) envs: Vec<(OsString, OsString)>,
    /// [`STALL`]; tests lower it.
    pub(crate) stall: Duration,
    /// Tests only: stdout's tar starts after this marker (the fake bsdtar
    /// is the test binary, which prints a banner first).
    pub(crate) preamble: Option<&'static [u8]>,
}

impl Launch {
    /// bsdtar at `paths.bsdtar`, as Grape runs it.
    pub(crate) fn bsdtar(paths: &RarPaths) -> Launch {
        Launch {
            program: paths.bsdtar.clone(),
            args: ARGS.iter().map(OsString::from).collect(),
            envs: Vec::new(),
            stall: STALL,
            preamble: None,
        }
    }
}

/// What the threads and the job share.
struct Shared {
    /// Source bytes written into stdin.
    pumped: AtomicU64,
    /// Stdout bytes read (after a test preamble).
    out: AtomicU64,
    /// The job is waiting on bsdtar: a stdout read, or the exit.
    waiting: AtomicBool,
    /// The watchdog killed bsdtar for a stall.
    stalled: AtomicBool,
    /// Grape killed bsdtar (it didn't exit by itself).
    killed: AtomicBool,
    /// The watchdog ends.
    stop: AtomicBool,
    /// Reading the source failed (in the pump).
    pump_error: Mutex<Option<io::ErrorKind>>,
    child: Mutex<Child>,
    /// bsdtar's stdout once its reader let go of it: kept open until the
    /// reap, so a bsdtar Grape stopped reading doesn't fail on a closed
    /// pipe (its own error would then hide Grape's reason).
    parked: Mutex<Option<ChildStdout>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    /// Kills bsdtar unless it already exited. True if it was killed now.
    fn kill(&self) -> bool {
        let mut child = lock(&self.child);
        if let Ok(Some(_)) = child.try_wait() {
            return false;
        }
        let _ = child.kill();
        self.killed.store(true, Ordering::SeqCst);
        true
    }
}

/// bsdtar's stderr: its first [`STDERR_KEEP`] bytes, and how many in all.
#[derive(Debug, Default)]
pub(crate) struct Stderr {
    pub(crate) head: Vec<u8>,
    pub(crate) total: u64,
}

/// What [`Running::end`] saw besides its verdict.
#[derive(Debug, Default)]
#[cfg_attr(not(test), allow(dead_code))] // Read by the tests only.
pub(crate) struct Seen {
    /// What bsdtar wrote on stderr.
    pub(crate) stderr: Stderr,
    /// The source bytes written into its stdin.
    pub(crate) pumped: u64,
    /// The stdout bytes read, a test preamble not counted.
    pub(crate) out: u64,
}

/// A running bsdtar and its threads. Dropped without [`Running::finish`],
/// it kills bsdtar, joins everything, reaps it and removes its folder.
pub(crate) struct Running {
    shared: Arc<Shared>,
    /// Until [`Running::stdout`] takes it.
    stdout: Option<ChildStdout>,
    preamble: Option<&'static [u8]>,
    token: CancelToken,
    pump: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<Stderr>>,
    watch: Option<JoinHandle<()>>,
    work: Option<WorkDir>,
    /// The exit status, once reaped.
    status: Option<Result<ExitStatus, io::ErrorKind>>,
    stderr_seen: Option<Stderr>,
}

impl Running {
    /// Starts bsdtar on the RAR `source` (at its start) and its threads.
    pub(crate) fn spawn(
        launch: &Launch,
        source: File,
        token: CancelToken,
    ) -> Result<Running, Failure> {
        let no_start = |e: io::Error| Failure::Rar(RarFailure::NoStart(e.kind()));
        let work = WorkDir::create().map_err(no_start)?;
        let mut command = Command::new(&launch.program);
        command
            .args(&launch.args)
            .env_clear()
            .env("PATH", work.path())
            // RAR 4 DOS times go through mktime: read as UTC.
            .env("TZ", "UTC0")
            .env("TAR_READER_OPTIONS", "rar:hdrcharset=UTF-8");
        #[cfg(windows)]
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        // hdrcharset needs a UTF-8 locale, or bsdtar fails on every RAR.
        #[cfg(target_os = "macos")]
        command.env("LC_ALL", "en_US.UTF-8");
        #[cfg(all(unix, not(target_os = "macos")))]
        command.env("LC_ALL", "C.UTF-8");
        command
            .envs(launch.envs.iter().map(|(k, v)| (k, v)))
            .current_dir(work.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command.spawn().map_err(no_start)?;
        let (stdin, stdout, stderr) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let shared = Arc::new(Shared {
            pumped: AtomicU64::new(0),
            out: AtomicU64::new(0),
            waiting: AtomicBool::new(false),
            stalled: AtomicBool::new(false),
            killed: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            pump_error: Mutex::new(None),
            child: Mutex::new(child),
            parked: Mutex::new(None),
        });
        // From here on, dropping `running` kills and reaps bsdtar.
        let mut running = Running {
            shared,
            stdout,
            preamble: launch.preamble,
            token,
            pump: None,
            stderr: None,
            watch: None,
            work: Some(work),
            status: None,
            stderr_seen: None,
        };
        let (Some(stdin), Some(stderr), true) = (stdin, stderr, running.stdout.is_some()) else {
            return Err(stopped("no pipe"));
        };
        let no_thread = |_| stopped("no thread");
        running.stderr = Some(
            thread::Builder::new()
                .name("grape-bsdtar-stderr".into())
                .spawn(move || drain_stderr(stderr))
                .map_err(no_thread)?,
        );
        let (shared, token) = (Arc::clone(&running.shared), running.token.clone());
        running.pump = Some(
            thread::Builder::new()
                .name("grape-bsdtar-stdin".into())
                .spawn(move || pump(&shared, source, stdin, &token))
                .map_err(no_thread)?,
        );
        let (shared, token, stall) = (
            Arc::clone(&running.shared),
            running.token.clone(),
            launch.stall,
        );
        running.watch = Some(
            thread::Builder::new()
                .name("grape-bsdtar-watch".into())
                .spawn(move || watch(&shared, &token, stall))
                .map_err(no_thread)?,
        );
        Ok(running)
    }

    /// bsdtar's stdout, for the job's thread (once).
    pub(crate) fn stdout<'r, 'a>(&mut self, run: &'r Run<'a>) -> BufReader<ChildOut<'r, 'a>> {
        BufReader::with_capacity(
            STDOUT_BUF,
            ChildOut {
                run,
                shared: Arc::clone(&self.shared),
                stdout: self.stdout.take(),
                preamble: self.preamble.take(),
                token: self.token.clone(),
            },
        )
    }

    /// The stdout bytes read so far, a test preamble not counted.
    pub(crate) fn stdout_total(&self) -> u64 {
        self.shared.out.load(Ordering::SeqCst)
    }

    /// bsdtar's process id.
    #[cfg(test)]
    pub(crate) fn id(&self) -> u32 {
        lock(&self.shared.child).id()
    }

    /// Ends the run once the job is done with stdout (`walk`, read to its
    /// end if it is `Ok`), and judges it.
    pub(crate) fn finish(
        self,
        walk: Result<(), Failure>,
        token: &CancelToken,
    ) -> Result<(), Failure> {
        self.end(walk, token).0
    }

    /// [`Running::finish`], with what else it saw.
    pub(crate) fn end(
        mut self,
        walk: Result<(), Failure>,
        token: &CancelToken,
    ) -> (Result<(), Failure>, Seen) {
        let shared = Arc::clone(&self.shared);
        // The watchdog guards the wait for the exit too.
        shared.waiting.store(true, Ordering::SeqCst);
        match &walk {
            // Grape's own refusal: bsdtar has nothing more to say.
            Err(failure) if failure.is_policy() => {
                shared.kill();
            }
            // A broken stream: bsdtar usually exited already, which is why
            // it broke.
            Err(_) => {
                if !self.exits_within(Some(GRACE)) {
                    shared.kill();
                }
            }
            // stdout is at its end: bsdtar is exiting.
            Ok(()) => {
                self.exits_within(None);
            }
        }
        self.cleanup();
        let status = self.status.take().unwrap_or(Err(io::ErrorKind::Other));
        let stderr = self.stderr_seen.take().unwrap_or_default();
        let verdict = judge(walk, status, &stderr, &shared, token);
        let seen = Seen {
            stderr,
            pumped: shared.pumped.load(Ordering::SeqCst),
            out: shared.out.load(Ordering::SeqCst),
        };
        (verdict, seen)
    }

    /// Polls for bsdtar's exit for at most `limit` (`None`: until it exits
    /// or the watchdog kills it). True once it exited.
    fn exits_within(&self, limit: Option<Duration>) -> bool {
        let start = Instant::now();
        loop {
            match lock(&self.shared.child).try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => {}
                Err(_) => return false,
            }
            if limit.is_some_and(|limit| start.elapsed() >= limit) {
                return false;
            }
            thread::sleep(POLL);
        }
    }

    /// Kills bsdtar if it still runs, joins the pump, reaps bsdtar, joins
    /// the stderr drain and the watchdog, and removes the folder. Runs
    /// once.
    fn cleanup(&mut self) {
        if self.work.is_none() {
            return;
        }
        // A no-op once bsdtar exited.
        self.shared.kill();
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
        self.status = Some(lock(&self.shared.child).wait().map_err(|e| e.kind()));
        if let Some(stderr) = self.stderr.take() {
            self.stderr_seen = stderr.join().ok();
        }
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(watch) = self.watch.take() {
            let _ = watch.join();
        }
        drop(lock(&self.shared.parked).take());
        drop(self.stdout.take());
        drop(self.work.take());
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn stopped(detail: &str) -> Failure {
    Failure::Rar(RarFailure::Stopped(detail.to_string()))
}

/// The judgement, the first that applies: a cancel, a stall, the source
/// failing, Grape's own refusal, bsdtar failing by itself (abnormally, then
/// with exit 1 or anything on stderr), a broken stream, a kill.
fn judge(
    walk: Result<(), Failure>,
    status: Result<ExitStatus, io::ErrorKind>,
    stderr: &Stderr,
    shared: &Shared,
    token: &CancelToken,
) -> Result<(), Failure> {
    if token.is_cancelled() {
        return Err(Failure::Cancelled);
    }
    if shared.stalled.load(Ordering::SeqCst) {
        return Err(Failure::Rar(RarFailure::Stalled));
    }
    if let Some(kind) = *lock(&shared.pump_error) {
        return Err(Failure::Read(format!("reading the source: {kind:?}")));
    }
    let walk = match walk {
        Err(failure) if failure.is_policy() => return Err(failure),
        walk => walk,
    };
    let status = match status {
        Ok(status) => status,
        Err(kind) => return Err(stopped(&format!("waiting: {kind:?}"))),
    };
    let killed = shared.killed.load(Ordering::SeqCst);
    if !killed {
        let code = status.code();
        // A signal (Unix), or an exit code bsdtar never uses (a Windows
        // access violation and the like).
        if !matches!(code, Some(0 | 1)) {
            return Err(stopped(&format!("exit {code:?}")));
        }
        if code == Some(1) || stderr.total > 0 {
            return Err(classify(&stderr.head, stderr.total, code));
        }
    }
    walk?;
    if killed {
        return Err(stopped("killed"));
    }
    Ok(())
}

/// What a line of bsdtar's stderr means.
#[derive(Debug, Clone, Copy)]
enum Means {
    Encrypted,
    Solid4,
    Dictionary,
    Feature(&'static str),
    Names(&'static str),
    /// Grape's environment is wrong, not the file.
    Locale,
    Corrupt,
}

/// libarchive's and bsdtar's own texts, never the `: <strerror>` suffix
/// (EILSEQ's text on Linux, EFTYPE's on macOS, the CRT's on Windows). The
/// first that occurs anywhere in the kept stderr wins.
const PATTERNS: &[(&str, Means)] = &[
    ("RAR encryption support unavailable", Means::Encrypted),
    ("Encryption is not supported", Means::Encrypted),
    (
        "Reading encrypted data is not currently supported",
        Means::Encrypted,
    ),
    ("RAR solid archive support unavailable", Means::Solid4),
    (
        "Declared dictionary size is not supported",
        Means::Dictionary,
    ),
    (
        "Files with unknown unpacked size are not supported",
        Means::Feature("unknown unpacked size"),
    ),
    (
        "Pathname cannot be converted from",
        Means::Names("not UTF-8"),
    ),
    (
        "Can't translate pathname",
        Means::Names("hdrcharset=BINARY"),
    ),
    (
        "empty or unreadable filename",
        Means::Names("unreadable name"),
    ),
    (
        "A character-set conversion not fully supported",
        Means::Locale,
    ),
    ("Failed to set default locale", Means::Locale),
    ("File CRC error", Means::Corrupt),
    ("Checksum error", Means::Corrupt),
    ("Header CRC error", Means::Corrupt),
    ("Bad RAR file", Means::Corrupt),
    ("Truncated", Means::Corrupt),
    ("Invalid", Means::Corrupt),
    ("Too small block", Means::Corrupt),
    ("I/O error when unstoring", Means::Corrupt),
    ("Failed to read", Means::Corrupt),
    ("Unable to read link", Means::Corrupt),
    ("Unrecognized archive format", Means::Corrupt),
    ("pax format cannot archive files with mode", Means::Corrupt),
    ("Declared solid file", Means::Corrupt),
];

/// What bsdtar's stderr (`head`, the first bytes of `total`) says went
/// wrong; `code` is its exit code.
pub(crate) fn classify(head: &[u8], total: u64, code: Option<i32>) -> Failure {
    let code = code.map_or_else(|| "?".to_string(), |code| code.to_string());
    let found = PATTERNS.iter().find(|(pattern, _)| {
        let pattern = pattern.as_bytes();
        head.windows(pattern.len()).any(|window| window == pattern)
    });
    let Some(&(pattern, means)) = found else {
        return Failure::Corrupt(format!(
            "bsdtar exit {code}: unknown message ({total} bytes on stderr)"
        ));
    };
    match means {
        Means::Encrypted => Failure::Encrypted,
        Means::Solid4 => Failure::Rar(RarFailure::Solid4("bsdtar")),
        Means::Dictionary => Failure::Rar(RarFailure::Dictionary(0)),
        Means::Feature(what) => Failure::Rar(RarFailure::Feature(what)),
        Means::Names(why) => Failure::Rar(RarFailure::Names(why)),
        Means::Locale => stopped("locale"),
        Means::Corrupt => Failure::Corrupt(format!(
            "bsdtar exit {code}: {pattern:?} ({total} bytes on stderr)"
        )),
    }
}

/// The pump: the RAR into bsdtar's stdin until its end, a cancel, a read
/// error (recorded) or a write error (bsdtar is gone: its status says
/// why). Closing stdin ends bsdtar's input.
fn pump(shared: &Shared, mut source: File, mut stdin: ChildStdin, token: &CancelToken) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        if token.is_cancelled() {
            break;
        }
        let n = match source.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                *lock(&shared.pump_error) = Some(e.kind());
                break;
            }
        };
        if stdin.write_all(&buf[..n]).is_err() {
            break;
        }
        shared.pumped.fetch_add(n as u64, Ordering::SeqCst);
    }
}

/// The stderr drain: keeps the first [`STDERR_KEEP`] bytes, counts all.
fn drain_stderr(mut stderr: impl Read) -> Stderr {
    let mut seen = Stderr::default();
    let mut buf = [0u8; STDERR_STEP];
    loop {
        match stderr.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                seen.total = seen.total.saturating_add(n as u64);
                let room = STDERR_KEEP - seen.head.len();
                seen.head.extend_from_slice(&buf[..n.min(room)]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    seen
}

/// The watchdog: every [`TICK`], kills bsdtar once the job is cancelled,
/// or once the job has waited on it for `stall` while neither the bytes
/// pumped nor the bytes read moved. Time the job spends on its own output
/// never counts.
fn watch(shared: &Shared, token: &CancelToken, stall: Duration) {
    let counts = || {
        (
            shared.pumped.load(Ordering::SeqCst),
            shared.out.load(Ordering::SeqCst),
        )
    };
    let mut last = counts();
    let mut quiet_since = Instant::now();
    loop {
        thread::sleep(TICK);
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        if shared.killed.load(Ordering::SeqCst) {
            continue;
        }
        if token.is_cancelled() {
            shared.kill();
            continue;
        }
        let now = counts();
        if shared.waiting.load(Ordering::SeqCst) && now == last {
            if quiet_since.elapsed() >= stall && shared.kill() {
                shared.stalled.store(true, Ordering::SeqCst);
            }
        } else {
            last = now;
            quiet_since = Instant::now();
        }
    }
}

/// bsdtar's stdout on the job's thread: stops once the job is cancelled,
/// counts what it reads, and passes the bytes pumped on as progress.
pub(crate) struct ChildOut<'r, 'a> {
    run: &'r Run<'a>,
    shared: Arc<Shared>,
    stdout: Option<ChildStdout>,
    preamble: Option<&'static [u8]>,
    token: CancelToken,
}

impl ChildOut<'_, '_> {
    /// Reads past the test preamble `marker`, at most [`PREAMBLE_MAX`]
    /// bytes, one at a time so no tar byte is taken with it.
    fn skip_preamble(stdout: &mut ChildStdout, marker: &[u8]) -> io::Result<()> {
        let mut seen = Vec::new();
        let mut byte = [0u8; 1];
        while seen.len() < PREAMBLE_MAX {
            match stdout.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    seen.push(byte[0]);
                    if seen.ends_with(marker) {
                        return Ok(());
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "no preamble before the tar",
        ))
    }
}

impl Read for ChildOut<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.token.is_cancelled() {
            // Not `Interrupted`: `read_exact` retries that.
            return Err(io::Error::other("cancelled"));
        }
        let Some(stdout) = self.stdout.as_mut() else {
            return Ok(0);
        };
        let shared = &self.shared;
        shared.waiting.store(true, Ordering::SeqCst);
        let result = match self.preamble.take() {
            Some(marker) => Self::skip_preamble(stdout, marker).and_then(|()| stdout.read(buf)),
            None => stdout.read(buf),
        };
        shared.waiting.store(false, Ordering::SeqCst);
        let n = result?;
        shared.out.fetch_add(n as u64, Ordering::SeqCst);
        self.run.set_consumed(shared.pumped.load(Ordering::SeqCst));
        Ok(n)
    }
}

impl Drop for ChildOut<'_, '_> {
    fn drop(&mut self) {
        if let Some(stdout) = self.stdout.take() {
            *lock(&self.shared.parked) = Some(stdout);
        }
    }
}

#[cfg(test)]
thread_local! {
    /// The last private folder made on this thread, for the tests.
    static LAST_WORK: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// The private folder made on this thread since the last call (tests).
#[cfg(test)]
pub(crate) fn take_work_dir() -> Option<PathBuf> {
    LAST_WORK.with(|last| last.borrow_mut().take())
}

/// bsdtar's working folder: new, empty, private (0700 on Unix), and
/// removed with whatever is in it when dropped.
struct WorkDir(PathBuf);

impl WorkDir {
    fn create() -> io::Result<WorkDir> {
        // `unique_suffix` starts with the process id.
        let path = std::env::temp_dir().join(format!("grape-bsdtar-{}", unique_suffix()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(&path)?;
        }
        #[cfg(not(unix))]
        fs::create_dir(&path)?;
        #[cfg(test)]
        LAST_WORK.with(|last| *last.borrow_mut() = Some(path.clone()));
        Ok(WorkDir(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = remove_tree(&self.0);
    }
}
