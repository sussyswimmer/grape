//! Running the `ffmpeg` and `ffprobe` sidecars.
//!
//! - **Only from [`crate::SidecarPaths`]:** [`command`] refuses a program
//!   path that isn't absolute, because a bare name would be looked up on
//!   `PATH` and run whatever FFmpeg the user installed.
//! - **Never stuck on a full pipe:** two reader threads drain the child's
//!   stdout and stderr while the calling thread watches the child. Only the
//!   last [`TAIL_BYTES`] of stderr are kept, for the log.
//! - **Bounded output:** [`probe`] keeps at most [`MAX_PROBE_OUTPUT`] of
//!   stdout; [`probe_lines`] keeps none, handing each line over as it's
//!   read, and stops at the limit its caller sets. Past either, the child is
//!   stopped ([`RunError::TooMuch`]).
//! - **Cancel and timeouts** are checked at least every [`POLL`]. The child
//!   is killed and then waited for, on every way out of a run, a panic in
//!   the caller's callback included ([`Reaper`]): on Windows its temp
//!   output can't be deleted while the process still holds it open.
//! - **Two at a time:** [`acquire`] holds an FFmpeg job back until one of
//!   the [`MAX_FFMPEG_JOBS`] slots is free (`docs/ARCHITECTURE.md`).

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::CancelToken;

/// How many FFmpeg jobs may run at once (`docs/ARCHITECTURE.md`).
const MAX_FFMPEG_JOBS: usize = 2;

/// The longest the calling thread waits before it checks the cancel token,
/// the deadline and the child again.
const POLL: Duration = Duration::from_millis(50);

/// The pause between checks once the child closed its stdout, which it
/// usually does by exiting: there is nothing left to wait on but the child.
const EXIT_POLL: Duration = Duration::from_millis(10);

/// How much of the end of stderr is kept for the log.
const TAIL_BYTES: usize = 16 * 1024;

/// The most stdout [`probe`] accepts.
pub(crate) const MAX_PROBE_OUTPUT: usize = 4 * 1024 * 1024;

/// The longest `-progress` line that is parsed (longer ones are skipped),
/// and how much of a longer line [`probe_lines`] hands over.
pub(crate) const MAX_LINE: usize = 4 * 1024;

/// `DETACHED_PROCESS`: the child gets no console at all, so no console
/// window flashes up and Windows starts no `conhost.exe` for it. Its stdio
/// are a null handle and pipes, so it needs none. `CREATE_NO_WINDOW` would
/// still give these console programs a hidden console, whose host is a
/// second process, one more than a Job Object that allows one would hold
/// (D33 chose this flag for `grape-heif.exe` for that reason).
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// `BELOW_NORMAL_PRIORITY_CLASS`: an encode doesn't starve the desktop.
#[cfg(windows)]
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

/// Why running a sidecar failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RunError {
    /// The program path isn't absolute. Nothing was started.
    #[error("sidecar path isn't absolute: {}", .0.display())]
    NotAbsolute(PathBuf),
    /// The child couldn't be started (missing file, no permission).
    #[error("couldn't start the sidecar: {0}")]
    Spawn(io::Error),
    /// The child exited with a failure status.
    #[error("the sidecar exited with {}", exit_text(.code))]
    Exit {
        /// The exit code; `None` when a signal ended the child.
        code: Option<i32>,
        /// The end of what it wrote to stderr.
        stderr_tail: String,
    },
    /// The token was cancelled. The child, if it was started, is gone.
    #[error("cancelled")]
    Cancelled,
    /// The child ran past its timeout and was killed.
    #[error("the sidecar timed out")]
    TimedOut,
    /// The child printed more than `limit` bytes on stdout and was stopped.
    #[error("the sidecar printed more than {} MiB", .limit >> 20)]
    TooMuch {
        /// The most it could print, in bytes.
        limit: u64,
    },
    /// Watching the child or reading its output failed. The child is gone.
    #[error("sidecar I/O failed: {0}")]
    Io(io::Error),
}

/// "code 3", or how a child ended without one.
fn exit_text(code: &Option<i32>) -> String {
    match code {
        Some(code) => format!("code {code}"),
        None => "no code (ended by a signal)".to_string(),
    }
}

/// A child that exited with status 0.
#[derive(Debug)]
pub(crate) struct Finished {
    /// The end of what it wrote to stderr (FFmpeg warns there even when it
    /// succeeds), for the log.
    pub(crate) stderr_tail: String,
}

/// A [`Command`] for the sidecar at `program`: stdin empty, stdout and
/// stderr piped, `FFREPORT` removed so a user's setting can't make FFmpeg
/// write a report file. On Windows the child gets no console at all
/// ([`DETACHED_PROCESS`]) and below-normal priority.
pub(crate) fn command(program: &Path, args: &[OsString]) -> Result<Command, RunError> {
    if !program.is_absolute() {
        return Err(RunError::NotAbsolute(program.to_path_buf()));
    }
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("FFREPORT");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(DETACHED_PROCESS | BELOW_NORMAL_PRIORITY_CLASS);
    }
    Ok(command)
}

/// Runs `ffmpeg` with `args` (which include `-progress pipe:1`) and calls
/// `on_time`, on this thread, with each output time it reports. Kills it
/// when `cancel` is cancelled, even as it exits. Nothing is started if the
/// token is already cancelled.
pub(crate) fn transcode(
    program: &Path,
    args: &[OsString],
    cancel: &CancelToken,
    on_time: &mut dyn FnMut(Duration),
) -> Result<Finished, RunError> {
    #[cfg(test)]
    if let Some(fake) = tests::next_fake(args) {
        return fake.transcode(cancel, on_time);
    }
    transcode_command(command(program, args)?, cancel, on_time)
}

/// [`transcode`] with a ready [`Command`] (tests add environment to it).
fn transcode_command(
    command: Command,
    cancel: &CancelToken,
    on_time: &mut dyn FnMut(Duration),
) -> Result<Finished, RunError> {
    supervise(command, cancel, None, read_progress, &mut |time| {
        on_time(time);
        Ok(())
    })
}

/// Runs `ffprobe` with `args` and returns its stdout (lossy UTF-8). Kills
/// it when `cancel` is cancelled or after `timeout`, and refuses more than
/// [`MAX_PROBE_OUTPUT`] of output ([`RunError::TooMuch`]).
pub(crate) fn probe(
    program: &Path,
    args: &[OsString],
    cancel: &CancelToken,
    timeout: Duration,
) -> Result<String, RunError> {
    #[cfg(test)]
    if let Some(fake) = tests::next_fake(args) {
        return fake.probe(cancel, timeout);
    }
    probe_command(command(program, args)?, cancel, timeout)
}

/// [`probe`] with a ready [`Command`] (tests add environment to it).
fn probe_command(
    command: Command,
    cancel: &CancelToken,
    timeout: Duration,
) -> Result<String, RunError> {
    let deadline = Instant::now().checked_add(timeout);
    let mut output = None;
    let mut keep = |read: Result<Vec<u8>, RunError>| {
        output = Some(read?);
        Ok(())
    };
    supervise(command, cancel, deadline, read_all, &mut keep)?;
    let bytes =
        output.ok_or_else(|| RunError::Io(io::Error::other("the sidecar's stdout was lost")))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Runs `ffprobe` with `args` and hands `each` every line of its stdout as
/// it's read: the line (lossy UTF-8, without its line end) and whether it
/// was cut, a line longer than [`MAX_LINE`] coming as its first
/// [`MAX_LINE`] bytes only. Nothing else is kept, so a probe whose output
/// matters line by line needs no room for all of it. Kills it when `cancel`
/// is cancelled, after `timeout`, or once it printed more than `limit`
/// bytes ([`RunError::TooMuch`], with every whole line before that handed
/// over).
pub(crate) fn probe_lines(
    program: &Path,
    args: &[OsString],
    cancel: &CancelToken,
    timeout: Duration,
    limit: u64,
    each: &mut (dyn FnMut(&str, bool) + Send),
) -> Result<(), RunError> {
    #[cfg(test)]
    if let Some(fake) = tests::next_fake(args) {
        return fake.probe_lines(cancel, timeout, limit, each);
    }
    probe_lines_command(command(program, args)?, cancel, timeout, limit, each)
}

/// [`probe_lines`] with a ready [`Command`] (tests add environment to it).
fn probe_lines_command(
    command: Command,
    cancel: &CancelToken,
    timeout: Duration,
    limit: u64,
    each: &mut (dyn FnMut(&str, bool) + Send),
) -> Result<(), RunError> {
    let deadline = Instant::now().checked_add(timeout);
    let read = |stdout: ChildStdout, ended: Sender<Result<(), RunError>>| {
        // The receiver outlives the reader: both live in one scope.
        let _ = ended.send(read_heads(stdout, limit, each));
    };
    supervise(command, cancel, deadline, read, &mut |ended| ended)?;
    Ok(())
}

/// Starts `command` and sees it through: `read_stdout` runs on its own
/// thread and sends messages that `on_message` handles on this one; stderr
/// is drained into a [`Tail`]. An error or a panic from `on_message`, a
/// cancel or the deadline kills the child ([`Reaper`]). Returns once the
/// child is gone and both readers have finished.
fn supervise<M: Send>(
    mut command: Command,
    cancel: &CancelToken,
    deadline: Option<Instant>,
    read_stdout: impl FnOnce(ChildStdout, Sender<M>) + Send,
    on_message: &mut dyn FnMut(M) -> Result<(), RunError>,
) -> Result<Finished, RunError> {
    if cancel.is_cancelled() {
        return Err(RunError::Cancelled);
    }
    let mut child = Reaper {
        child: command.spawn().map_err(RunError::Spawn)?,
        exited: false,
    };
    let (Some(stdout), Some(stderr)) = (child.child.stdout.take(), child.child.stderr.take())
    else {
        return Err(RunError::Io(io::Error::other(
            "the sidecar's output isn't piped",
        )));
    };
    let (sender, messages) = mpsc::channel();
    thread::scope(|scope| {
        // Owned by this closure, so even a panic drops it, stopping the
        // child, before the scope waits for the readers, which only end
        // once the child's pipes close.
        let mut child = child;
        let readers = thread::Builder::new()
            .name("grape-sidecar-stdout".to_string())
            .spawn_scoped(scope, move || read_stdout(stdout, sender))
            .and_then(|out| {
                thread::Builder::new()
                    .name("grape-sidecar-stderr".to_string())
                    .spawn_scoped(scope, move || drain_tail(stderr))
                    .map(|err| (out, err))
            });
        let (out, err) = readers.map_err(RunError::Io)?;
        let status = watch(&mut child.child, cancel, deadline, &messages, on_message)?;
        child.exited = true;
        // The child has exited, so its pipes are closed and both readers end.
        let _ = out.join();
        let stderr_tail = err.join().map(Tail::render).unwrap_or_default();
        let drained = messages.try_iter().try_for_each(&mut *on_message);
        if cancel.is_cancelled() {
            return Err(RunError::Cancelled);
        }
        drained?;
        if status.success() {
            Ok(Finished { stderr_tail })
        } else {
            Err(RunError::Exit {
                code: status.code(),
                stderr_tail,
            })
        }
    })
}

/// Hands messages to `on_message` until the child exits, checking the
/// token, the child and the deadline at least every [`POLL`]. On an error
/// the child may still be running; the caller stops it.
fn watch<M>(
    child: &mut Child,
    cancel: &CancelToken,
    deadline: Option<Instant>,
    messages: &Receiver<M>,
    on_message: &mut dyn FnMut(M) -> Result<(), RunError>,
) -> Result<ExitStatus, RunError> {
    loop {
        let stdout_closed = match messages.recv_timeout(POLL) {
            Ok(message) => {
                on_message(message)?;
                false
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => true,
        };
        if cancel.is_cancelled() {
            return Err(RunError::Cancelled);
        }
        if let Some(status) = child.try_wait().map_err(RunError::Io)? {
            return Ok(status);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(RunError::TimedOut);
        }
        if stdout_closed {
            // `recv_timeout` returns at once from now on.
            thread::sleep(EXIT_POLL);
        }
    }
}

/// A running child, killed and waited for ([`stop`]) when dropped unless
/// it has exited: on every way out of [`supervise`], a panic included, so
/// it never outlives the run.
struct Reaper {
    child: Child,
    /// Set once `try_wait` saw the child exit, which also reaped it.
    exited: bool,
}

impl Drop for Reaper {
    fn drop(&mut self) {
        if !self.exited {
            stop(&mut self.child);
        }
    }
}

/// Kills the child and always waits for it: on Windows the output file
/// can't be deleted until the process is really gone.
fn stop(child: &mut Child) {
    // Fails only if the child already exited; `wait` then reaps it.
    let _ = child.kill();
    let _ = child.wait();
}

/// The stdout reader for [`transcode`]: sends each output time it parses.
fn read_progress(stdout: ChildStdout, times: Sender<Duration>) {
    let mut parser = ProgressParser::new();
    read_lines(stdout, |line| {
        if let Some(time) = parser.feed(line) {
            // The receiver outlives the reader: both live in one scope.
            let _ = times.send(time);
        }
    });
}

/// The stdout reader for [`probe`]: sends all of stdout once it ends, or
/// [`RunError::TooMuch`] as soon as it passes [`MAX_PROBE_OUTPUT`].
/// Returning drops the pipe, so a child that keeps writing gets an error
/// instead of blocking.
fn read_all(stdout: ChildStdout, output: Sender<Result<Vec<u8>, RunError>>) {
    let mut bytes = Vec::new();
    let limit = MAX_PROBE_OUTPUT as u64 + 1;
    let read = match stdout.take(limit).read_to_end(&mut bytes) {
        Ok(_) if bytes.len() > MAX_PROBE_OUTPUT => Err(RunError::TooMuch {
            limit: MAX_PROBE_OUTPUT as u64,
        }),
        Ok(_) => Ok(bytes),
        Err(e) => Err(RunError::Io(e)),
    };
    let _ = output.send(read);
}

/// The stdout reader for [`probe_lines`]: hands `each` every line of
/// `reader` with its line end (`\n` or `\r\n`) removed, until the end or
/// a read error. A line longer than [`MAX_LINE`] is handed over as its
/// first [`MAX_LINE`] bytes, cut, and the rest of it is skipped a buffer at
/// a time, so no line grows the buffer. Fails with [`RunError::TooMuch`]
/// once more than `limit` bytes were read; the line that passed it isn't
/// handed over. Returning drops the pipe, as in [`read_all`].
fn read_heads(
    reader: impl Read,
    limit: u64,
    each: &mut dyn FnMut(&str, bool),
) -> Result<(), RunError> {
    let mut reader = BufReader::new(reader);
    let (mut head, mut rest) = (Vec::new(), Vec::new());
    let mut total = 0u64;
    // Reads up to MAX_LINE bytes of the line into `buf`, counting them.
    let mut next = |buf: &mut Vec<u8>, total: &mut u64| -> Result<bool, RunError> {
        buf.clear();
        let n = reader
            .by_ref()
            .take(MAX_LINE as u64)
            .read_until(b'\n', buf)
            .map_err(RunError::Io)?;
        *total = total.saturating_add(n as u64);
        if *total > limit {
            return Err(RunError::TooMuch { limit });
        }
        Ok(n > 0)
    };
    while next(&mut head, &mut total)? {
        // A full buffer without a line end is the start of a longer line,
        // unless all that's left of it is the line end.
        let mut cut = false;
        if head.len() == MAX_LINE && head.last() != Some(&b'\n') {
            while next(&mut rest, &mut total)? {
                let ended = rest.last() == Some(&b'\n');
                cut |= !matches!(rest.as_slice(), b"\n" | b"\r\n" | b"\r");
                if ended {
                    break;
                }
            }
        }
        let text = String::from_utf8_lossy(&head);
        let text = text.strip_suffix('\n').unwrap_or(&text);
        each(text.strip_suffix('\r').unwrap_or(text), cut);
    }
    Ok(())
}

/// Calls `each` with every line of `reader` (lossy UTF-8, line end
/// included), until the end or a read error. A line longer than
/// [`MAX_LINE`] is skipped whole, so output without line ends can't grow
/// the buffer.
fn read_lines(reader: impl Read, mut each: impl FnMut(&str)) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    let mut skipping = false;
    loop {
        line.clear();
        match reader
            .by_ref()
            .take(MAX_LINE as u64)
            .read_until(b'\n', &mut line)
        {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                // A full buffer without a line end is part of an overlong line.
                let overlong = line.len() == MAX_LINE && line.last() != Some(&b'\n');
                if !skipping && !overlong {
                    each(&String::from_utf8_lossy(&line));
                }
                skipping = overlong;
            }
        }
    }
}

/// Reads `stderr` to the end (or a read error), keeping its [`Tail`].
fn drain_tail(mut stderr: impl Read) -> Tail {
    let mut tail = Tail::default();
    let mut buf = [0u8; 8 * 1024];
    loop {
        match stderr.read(&mut buf) {
            Ok(0) => return tail,
            Ok(n) => tail.push(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return tail,
        }
    }
}

/// The last [`TAIL_BYTES`] bytes of a stream.
#[derive(Debug, Default)]
struct Tail {
    bytes: VecDeque<u8>,
}

impl Tail {
    /// Appends `chunk`, dropping the oldest bytes past [`TAIL_BYTES`].
    fn push(&mut self, chunk: &[u8]) {
        let chunk = &chunk[chunk.len().saturating_sub(TAIL_BYTES)..];
        let excess = (self.bytes.len() + chunk.len()).saturating_sub(TAIL_BYTES);
        self.bytes.drain(..excess);
        self.bytes.extend(chunk);
    }

    /// The bytes as text; invalid UTF-8 (such as a character cut in two at
    /// the start) becomes U+FFFD.
    fn render(mut self) -> String {
        String::from_utf8_lossy(self.bytes.make_contiguous()).into_owned()
    }
}

/// Reads `ffmpeg -progress pipe:1` output, one `key=value` line at a time.
#[derive(Debug, Default)]
pub(crate) struct ProgressParser;

impl ProgressParser {
    /// A parser for a new run.
    pub(crate) fn new() -> Self {
        ProgressParser
    }

    /// The output time on an `out_time_us=N` line (microseconds), else
    /// `None`. A trailing `\r` (CRLF) is ignored. `N/A` and negative times
    /// (before the first frame) give `None`, as does every other line:
    /// `progress=end`, the other keys, and `out_time_ms`, which despite its
    /// name holds the same microseconds (FFmpeg 6–9).
    pub(crate) fn feed(&mut self, line: &str) -> Option<Duration> {
        let (key, value) = line.trim_end().split_once('=')?;
        if key != "out_time_us" {
            return None;
        }
        value.parse::<u64>().ok().map(Duration::from_micros)
    }
}

/// A counting gate: at most `slots` holders at once.
#[derive(Debug)]
struct Gate {
    /// How many slots are held.
    busy: Mutex<usize>,
    /// Notified whenever a slot is released.
    freed: Condvar,
    /// How many holders may run at once.
    slots: usize,
}

impl Gate {
    const fn new(slots: usize) -> Gate {
        Gate {
            busy: Mutex::new(0),
            freed: Condvar::new(),
            slots,
        }
    }

    /// Waits for a free slot, rechecking `cancel` at least every [`POLL`].
    /// `None` once the token is cancelled, even if a slot is free: a
    /// cancelled job doesn't start.
    fn acquire(&self, cancel: &CancelToken) -> Option<SlotGuard<'_>> {
        // The count stays right even if a holder panicked, so a poisoned
        // lock is still usable.
        let mut busy = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if cancel.is_cancelled() {
                return None;
            }
            if *busy < self.slots {
                *busy += 1;
                return Some(SlotGuard { gate: self });
            }
            busy = self
                .freed
                .wait_timeout(busy, POLL)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// The FFmpeg job slots.
static FFMPEG_SLOTS: Gate = Gate::new(MAX_FFMPEG_JOBS);

/// Waits until fewer than [`MAX_FFMPEG_JOBS`] FFmpeg jobs run, and holds a
/// slot until the guard drops. `None` if `cancel` is cancelled first.
pub(crate) fn acquire(cancel: &CancelToken) -> Option<SlotGuard<'static>> {
    FFMPEG_SLOTS.acquire(cancel)
}

/// A held job slot, released when dropped.
#[derive(Debug)]
#[must_use = "the slot is released as soon as the guard drops"]
pub(crate) struct SlotGuard<'a> {
    gate: &'a Gate,
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        let mut busy = self
            .gate
            .busy
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *busy = busy.saturating_sub(1);
        drop(busy);
        self.gate.freed.notify_all();
    }
}

#[cfg(test)]
pub(super) mod tests {
    #![allow(clippy::unwrap_used)]

    use std::cell::RefCell;
    use std::io::{Cursor, Write};

    use super::*;

    /// The environment variable that turns [`fake_child`] into a child.
    const FAKE_MODE: &str = "GRAPE_FAKE_CHILD";

    /// The text a scripted [`Fake::Child`] writes.
    const FAKE_TEXT: &str = "GRAPE_FAKE_TEXT";

    /// A scripted [`Fake::Child`]'s output path.
    const FAKE_OUT: &str = "GRAPE_FAKE_OUT";

    /// A sidecar run that a test scripts with [`script`], in place of
    /// `ffmpeg` or `ffprobe`.
    pub(crate) enum Fake {
        /// `ffprobe` printing this. No child runs: the test harness prints
        /// its own lines around a fake child's output, which `probe::parse`
        /// would refuse.
        Prints(String),
        /// `ffprobe` printing what this returns, after doing what it does:
        /// how a test acts between two steps of a job.
        Runs(Box<dyn FnOnce() -> String>),
        /// [`fake_child`] in this mode, with [`FAKE_TEXT`] set to the text
        /// and [`FAKE_OUT`] to the run's output path (its last argument,
        /// without `file:`).
        Child(&'static str, String),
        /// `ffmpeg` or `ffprobe` failing this way without a child: how a
        /// sidecar that can't be started or watched fails.
        Fails(RunError),
    }

    /// A scripted run, ready to start.
    pub(super) struct Next {
        fake: Fake,
        out: OsString,
    }

    impl Next {
        /// The child of a scripted run.
        fn command(self) -> Command {
            match self.fake {
                Fake::Child(mode, text) => {
                    let mut command = fake(mode);
                    command.env(FAKE_TEXT, text).env(FAKE_OUT, self.out);
                    command
                }
                Fake::Prints(_) | Fake::Runs(_) | Fake::Fails(_) => {
                    panic!("a scripted run without a child")
                }
            }
        }

        /// The outcome of a scripted `ffmpeg` run.
        pub(super) fn transcode(
            self,
            cancel: &CancelToken,
            on_time: &mut dyn FnMut(Duration),
        ) -> Result<Finished, RunError> {
            match self.fake {
                Fake::Fails(e) => Err(e),
                Fake::Prints(_) | Fake::Runs(_) => panic!("ffmpeg was scripted as ffprobe"),
                Fake::Child(..) => transcode_command(self.command(), cancel, on_time),
            }
        }

        /// The outcome of a scripted `ffprobe` run.
        pub(super) fn probe(
            self,
            cancel: &CancelToken,
            timeout: Duration,
        ) -> Result<String, RunError> {
            match self.fake {
                Fake::Prints(text) => Ok(text),
                Fake::Runs(run) => Ok(run()),
                Fake::Fails(e) => Err(e),
                Fake::Child(..) => probe_command(self.command(), cancel, timeout),
            }
        }

        /// The outcome of a scripted `ffprobe` run read line by line: what
        /// it prints goes through the same reader as a child's stdout.
        pub(super) fn probe_lines(
            self,
            cancel: &CancelToken,
            timeout: Duration,
            limit: u64,
            each: &mut (dyn FnMut(&str, bool) + Send),
        ) -> Result<(), RunError> {
            match self.fake {
                Fake::Prints(text) => read_heads(text.as_bytes(), limit, each),
                Fake::Runs(run) => read_heads(run().as_bytes(), limit, each),
                Fake::Fails(e) => Err(e),
                Fake::Child(..) => {
                    probe_lines_command(self.command(), cancel, timeout, limit, each)
                }
            }
        }
    }

    thread_local! {
        /// The runs [`script`] queued on this thread.
        static SCRIPT: RefCell<VecDeque<Fake>> = const { RefCell::new(VecDeque::new()) };
        /// The argv of each scripted run on this thread.
        static RAN: RefCell<Vec<Vec<OsString>>> = const { RefCell::new(Vec::new()) };
    }

    /// Taken by each test that holds an FFmpeg slot or runs a job, so a test
    /// can count the slots in use ([`slots_held`]).
    static SLOT_USERS: Mutex<()> = Mutex::new(());

    /// Waits until no other test holds FFmpeg slots or runs a job.
    pub(crate) fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
        SLOT_USERS.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many FFmpeg slots are held.
    pub(crate) fn slots_held() -> usize {
        *FFMPEG_SLOTS
            .busy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes the next sidecar runs on this thread `fakes`, in order. A run
    /// past the last one starts the real program.
    pub(crate) fn script(fakes: impl IntoIterator<Item = Fake>) {
        SCRIPT.with(|script| *script.borrow_mut() = fakes.into_iter().collect());
        RAN.with(|ran| ran.borrow_mut().clear());
    }

    /// The argv of each scripted run on this thread since [`script`].
    pub(crate) fn ran() -> Vec<Vec<OsString>> {
        RAN.with(|ran| ran.borrow().clone())
    }

    /// The next scripted run on this thread, if any, noting its argv.
    pub(super) fn next_fake(args: &[OsString]) -> Option<Next> {
        let fake = SCRIPT.with(|script| script.borrow_mut().pop_front())?;
        RAN.with(|ran| ran.borrow_mut().push(args.to_vec()));
        let last = args.last().and_then(|arg| arg.to_str()).unwrap_or_default();
        let out = OsString::from(last.strip_prefix("file:").unwrap_or(last));
        Some(Next { fake, out })
    }

    /// Runs only [`fake_child`] in this test binary, printing straight to
    /// the pipes.
    const FAKE_CHILD_ARGS: [&str; 4] = [
        "--exact",
        "media::run::tests::fake_child",
        "--nocapture",
        "--test-threads=1",
    ];

    /// What the fake child prints in `probe` mode.
    const PROBE_OUTPUT: &str = "{\n  \"format\": {\n    \"duration\": \"12.345000\"\n  }\n}\n";

    /// A stand-in for `ffmpeg` and `ffprobe`, so these tests need neither.
    /// It does nothing in a normal test run. The tests start this test
    /// binary again with [`FAKE_MODE`] set, and it then acts as a child:
    /// - `progress`: `-progress` blocks on stdout, then exits 0;
    /// - `stderr-flood`: [`flood`] (over 1 MiB) on stderr, then exits 0;
    /// - `sleep`: runs until killed;
    /// - `fail`: "boom" on stderr, then exits 3;
    /// - `probe`: [`PROBE_OUTPUT`] on stdout, then exits 0;
    /// - `big`: 5 MiB on stdout, then exits 0;
    /// - `big-hold`: 5 MiB on stdout, then runs until killed, or for 30 s,
    ///   without closing it;
    /// - `write`: writes [`FAKE_TEXT`] to [`FAKE_OUT`], then as `progress`;
    /// - `error`: as `progress`, but then [`FAKE_TEXT`] on stderr and
    ///   exits 1;
    /// - `measure`: as `progress`, then [`FAKE_TEXT`] on stderr, and exits
    ///   0 (a measuring pass, whose output is `-`);
    /// - `hold`: writes its pid to [`FAKE_OUT`] and holds it open (on
    ///   Windows, so nobody else can open or delete it), reports one output
    ///   time, then runs until killed, or for 30 s;
    /// - `tags-flood`: a tags probe's lines for one stream, then the same
    ///   format tag over and over, until its stdout is closed or for 30 s.
    #[test]
    fn fake_child() {
        let Ok(mode) = std::env::var(FAKE_MODE) else {
            return;
        };
        let code = match mode.as_str() {
            "progress" => fake_progress(),
            "stderr-flood" => {
                io::stderr().lock().write_all(&flood()).unwrap();
                0
            }
            "sleep" => loop {
                thread::sleep(Duration::from_millis(50));
            },
            "fail" => {
                io::stderr().lock().write_all(b"boom\n").unwrap();
                3
            }
            "probe" => {
                // Ends the harness's "test ... " line first.
                print_flushed(format!("\n{PROBE_OUTPUT}").as_bytes());
                0
            }
            "big" => {
                // Fails with a broken pipe once the reader gives up.
                let _ = io::stdout().lock().write_all(&vec![b'x'; 5 << 20]);
                0
            }
            "big-hold" => {
                let _ = io::stdout().lock().write_all(&vec![b'x'; 5 << 20]);
                thread::sleep(Duration::from_secs(30));
                0
            }
            "tags-flood" => {
                let mut out = io::stdout().lock();
                let stream = "\nstreams.stream.0.index=0\nstreams.stream.0.codec_type=\"audio\"\n\
                              streams.stream.0.tags.language=\"deu\"\n";
                let tags = "format.tags.title=\"again\"\n".repeat(4096);
                let started = Instant::now();
                let mut wrote = out.write_all(stream.as_bytes());
                while wrote.is_ok() && started.elapsed() < Duration::from_secs(30) {
                    wrote = out.write_all(tags.as_bytes());
                }
                0
            }
            "write" => {
                let text = std::env::var(FAKE_TEXT).unwrap();
                std::fs::write(std::env::var_os(FAKE_OUT).unwrap(), text).unwrap();
                fake_progress()
            }
            "error" => {
                fake_progress();
                let text = std::env::var(FAKE_TEXT).unwrap();
                io::stderr().lock().write_all(text.as_bytes()).unwrap();
                1
            }
            "measure" => {
                fake_progress();
                let text = std::env::var(FAKE_TEXT).unwrap();
                io::stderr().lock().write_all(text.as_bytes()).unwrap();
                0
            }
            "hold" => {
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create(true).truncate(true);
                #[cfg(windows)]
                {
                    use std::os::windows::fs::OpenOptionsExt;
                    options.share_mode(0);
                }
                let mut held = options.open(std::env::var_os(FAKE_OUT).unwrap()).unwrap();
                write!(held, "{}", std::process::id()).unwrap();
                print_flushed(b"\nout_time_us=1\n");
                // Ends by itself, so a test that fails can't leave it for good.
                thread::sleep(Duration::from_secs(30));
                0
            }
            other => panic!("unknown {FAKE_MODE} mode {other:?}"),
        };
        std::process::exit(code);
    }

    fn print_flushed(bytes: &[u8]) {
        let mut out = io::stdout().lock();
        out.write_all(bytes).unwrap();
        out.flush().unwrap();
    }

    /// Output times 0, 0.5, 1, 1.5 and 2 s, among lines to ignore.
    fn fake_progress() -> i32 {
        // Ends the harness's "test ... " line, so the first key starts a line.
        print_flushed(b"\nout_time_us=N/A\nout_time_us=-23220\nprogress=continue\n");
        for frame in 0u64..4 {
            let us = frame * 500_000;
            let block = format!(
                "frame={frame}\nfps=0.00\nout_time_us={us}\nout_time_ms={us}\n\
                 out_time=00:00:00.000000\nspeed=N/A\nprogress=continue\n"
            );
            print_flushed(block.as_bytes());
            thread::sleep(Duration::from_millis(20));
        }
        print_flushed(b"out_time_us=2000000\r\nout_time_ms=2000000\r\nprogress=end\r\n");
        0
    }

    /// What the fake child writes to stderr in `stderr-flood` mode.
    fn flood() -> Vec<u8> {
        (0..45_000)
            .flat_map(|i| format!("stderr flood line {i:07}\n").into_bytes())
            .collect()
    }

    /// A command that starts this test binary as a fake child in `mode`.
    fn fake(mode: &str) -> Command {
        let exe = std::env::current_exe().unwrap();
        let args: Vec<OsString> = FAKE_CHILD_ARGS.iter().map(OsString::from).collect();
        let mut fake = command(&exe, &args).unwrap();
        fake.env(FAKE_MODE, mode);
        fake
    }

    /// A [`fake`] child in `hold` mode that holds `file`.
    fn holding(file: &Path) -> Command {
        let mut child = fake("hold");
        child.env(FAKE_OUT, file);
        child
    }

    /// A file for a `hold` child, in a fresh folder that drops with it.
    struct Held {
        dir: PathBuf,
        file: PathBuf,
    }

    impl Held {
        fn new() -> Held {
            let dir =
                std::env::temp_dir().join(format!("grape-held-{}", crate::output::unique_suffix()));
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join("held");
            Held { dir, file }
        }

        /// Asserts, right after the run, that the child that held the file
        /// is gone: on Windows the file opens with no sharing at all, which
        /// only works once the child's handle is closed; it can be read and
        /// deleted; and its pid isn't even a zombie (Unix) or is no longer
        /// listed (Windows, whose process ids aren't reused at once).
        fn assert_gone(&self) {
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                // Once, without a retry: the child's handle must already be
                // closed when the run returns.
                let alone = std::fs::OpenOptions::new()
                    .read(true)
                    .share_mode(0)
                    .open(&self.file);
                assert!(alone.is_ok(), "the child still holds the file: {alone:?}");
            }
            let pid = std::fs::read_to_string(&self.file).unwrap();
            std::fs::remove_file(&self.file).unwrap();
            let pid = pid.trim();
            #[cfg(target_os = "linux")]
            assert!(
                !Path::new(&format!("/proc/{pid}")).exists(),
                "child {pid} is still in the process table"
            );
            #[cfg(all(unix, not(target_os = "linux")))]
            {
                let status = Command::new("/bin/ps")
                    .args(["-p", pid])
                    .stdout(Stdio::null())
                    .status()
                    .unwrap();
                assert!(
                    !status.success(),
                    "child {pid} is still in the process table"
                );
            }
            #[cfg(windows)]
            {
                let filter = format!("PID eq {pid}");
                let out = Command::new("tasklist")
                    .args(["/FI", filter.as_str(), "/NH"])
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                assert!(out.status.success(), "tasklist /FI {filter:?} failed");
                // A match is a row with the pid; no match, a message in the
                // system's language without it.
                let listed = String::from_utf8_lossy(&out.stdout);
                assert!(
                    !listed.split_whitespace().any(|word| word == pid),
                    "child {pid} is still running: {listed}"
                );
            }
        }
    }

    impl Drop for Held {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Cancels `token` if the test still runs after 10 s, so a hang fails
    /// the test instead of stalling the run. Dropping the returned sender
    /// ends the watch.
    fn watchdog(token: &CancelToken) -> Sender<()> {
        let (done, wait) = mpsc::channel::<()>();
        let token = token.clone();
        thread::spawn(move || {
            if wait.recv_timeout(Duration::from_secs(10)) == Err(RecvTimeoutError::Timeout) {
                token.cancel();
            }
        });
        done
    }

    #[test]
    fn progress_arrives_in_order_and_a_zero_exit_is_ok() {
        let token = CancelToken::new();
        let _watchdog = watchdog(&token);
        let mut times = Vec::new();
        transcode_command(fake("progress"), &token, &mut |time| times.push(time)).unwrap();
        assert_eq!(times, [0, 500, 1000, 1500, 2000].map(Duration::from_millis));
    }

    #[test]
    fn a_stderr_flood_cannot_block_the_child_and_only_its_tail_is_kept() {
        let token = CancelToken::new();
        let _watchdog = watchdog(&token);
        let finished = transcode_command(fake("stderr-flood"), &token, &mut |_| {}).unwrap();
        let tail = finished.stderr_tail;
        assert!(tail.len() <= TAIL_BYTES, "{}", tail.len());
        assert!(tail.ends_with("stderr flood line 0044999\n"));
        let flood = flood();
        assert_eq!(tail.as_bytes(), &flood[flood.len() - TAIL_BYTES..]);
    }

    #[test]
    fn cancel_kills_a_running_child_within_a_second() {
        let token = CancelToken::new();
        let canceller = {
            let token = token.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(200));
                token.cancel();
                Instant::now()
            })
        };
        let result = transcode_command(fake("sleep"), &token, &mut |_| {});
        let returned = Instant::now();
        assert!(matches!(result, Err(RunError::Cancelled)), "{result:?}");
        // Returning means both readers saw their pipe close, which the kill
        // does; `a_cancelled_child_is_reaped` checks it was waited for.
        let cancelled = canceller.join().unwrap();
        assert!(returned.duration_since(cancelled) < Duration::from_secs(1));
    }

    #[test]
    fn a_cancelled_child_is_reaped() {
        let held = Held::new();
        let token = CancelToken::new();
        let _watchdog = watchdog(&token);
        // It reports progress once it holds the file.
        let result = transcode_command(holding(&held.file), &token, &mut |_| token.cancel());
        assert!(matches!(result, Err(RunError::Cancelled)), "{result:?}");
        held.assert_gone();

        // The same for a probe, cancelled once the child holds the file.
        let held = Held::new();
        let token = CancelToken::new();
        let canceller = {
            let (token, file) = (token.clone(), held.file.clone());
            thread::spawn(move || {
                let started = Instant::now();
                while !file.exists() && started.elapsed() < Duration::from_secs(10) {
                    thread::sleep(Duration::from_millis(5));
                }
                token.cancel();
            })
        };
        let result = probe_command(holding(&held.file), &token, Duration::from_secs(20));
        assert!(matches!(result, Err(RunError::Cancelled)), "{result:?}");
        canceller.join().unwrap();
        held.assert_gone();
    }

    #[test]
    fn a_child_past_its_timeout_is_reaped() {
        let held = Held::new();
        let result = probe_command(
            holding(&held.file),
            &CancelToken::new(),
            Duration::from_secs(1),
        );
        assert!(matches!(result, Err(RunError::TimedOut)), "{result:?}");
        assert!(
            held.file.exists(),
            "the child didn't start within its timeout"
        );
        held.assert_gone();
    }

    #[test]
    fn a_panic_while_handling_progress_still_stops_the_child() {
        let held = Held::new();
        let child = holding(&held.file);
        let (returned, unwound) = mpsc::channel();
        let run = thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                transcode_command(child, &CancelToken::new(), &mut |_| {
                    panic!("the progress callback failed")
                })
            }));
            returned.send(result.is_err()).unwrap();
        });
        // Without the kill, the run waits on the child's pipes for 30 s.
        let panicked = unwound.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(panicked, "the panic was swallowed");
        run.join().unwrap();
        held.assert_gone();
    }

    #[test]
    fn a_cancel_as_the_child_exits_still_cancels() {
        let token = CancelToken::new();
        let result = transcode_command(fake("progress"), &token, &mut |time| {
            if time == Duration::from_secs(2) {
                token.cancel();
            }
        });
        assert!(matches!(result, Err(RunError::Cancelled)), "{result:?}");
    }

    #[test]
    fn a_cancel_while_reading_what_an_exited_child_left_still_cancels() {
        let token = CancelToken::new();
        let _watchdog = watchdog(&token);
        let mut first = true;
        let result = transcode_command(fake("progress"), &token, &mut |time| {
            // The child prints the rest and exits meanwhile, so the later
            // times are only read once it is gone.
            if first {
                first = false;
                thread::sleep(Duration::from_millis(500));
            }
            if time == Duration::from_secs(2) {
                token.cancel();
            }
        });
        assert!(matches!(result, Err(RunError::Cancelled)), "{result:?}");
    }

    #[test]
    fn a_cancelled_token_starts_nothing() {
        let token = CancelToken::new();
        token.cancel();
        // A missing program would fail to spawn, so `Cancelled` means no
        // spawn was tried.
        let missing = missing_program();
        let result = transcode(&missing, &[], &token, &mut |_| {});
        assert!(matches!(result, Err(RunError::Cancelled)), "{result:?}");
        let result = probe(&missing, &[], &token, Duration::from_secs(1));
        assert!(matches!(result, Err(RunError::Cancelled)), "{result:?}");
        let result = probe_lines(
            &missing,
            &[],
            &token,
            Duration::from_secs(1),
            1,
            &mut |_, _| {},
        );
        assert!(matches!(result, Err(RunError::Cancelled)), "{result:?}");
    }

    #[test]
    fn a_failing_child_reports_its_code_and_stderr() {
        let token = CancelToken::new();
        let _watchdog = watchdog(&token);
        let result = transcode_command(fake("fail"), &token, &mut |_| {});
        match result {
            Err(RunError::Exit {
                code: Some(3),
                stderr_tail,
            }) => assert!(stderr_tail.contains("boom"), "{stderr_tail:?}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn probe_returns_stdout() {
        let token = CancelToken::new();
        let output = probe_command(fake("probe"), &token, Duration::from_secs(10)).unwrap();
        // The test harness prints its own lines around it.
        assert!(output.contains(PROBE_OUTPUT), "{output:?}");
    }

    /// True for the error of a child that printed more than `limit`.
    fn too_much(result: &Result<impl std::fmt::Debug, RunError>, limit: usize) -> bool {
        matches!(result, Err(RunError::TooMuch { limit: l }) if *l == limit as u64)
    }

    #[test]
    fn probe_refuses_more_than_4_mib() {
        let token = CancelToken::new();
        let result = probe_command(fake("big"), &token, Duration::from_secs(10));
        assert!(too_much(&result, MAX_PROBE_OUTPUT), "{result:?}");
        let message = result.unwrap_err().to_string();
        assert_eq!(message, "the sidecar printed more than 4 MiB");
    }

    #[test]
    fn probe_stops_reading_at_4_mib_without_waiting_for_the_end() {
        let token = CancelToken::new();
        let started = Instant::now();
        // The child never closes its stdout: only the limit ends the read.
        let result = probe_command(fake("big-hold"), &token, Duration::from_secs(10));
        assert!(too_much(&result, MAX_PROBE_OUTPUT), "{result:?}");
        let took = started.elapsed();
        assert!(took < Duration::from_secs(5), "{took:?}");
    }

    /// What [`read_heads`] hands over for `text` with `limit`, and how it
    /// ended.
    fn heads(text: &str, limit: u64) -> (Vec<(String, bool)>, Result<(), RunError>) {
        let mut lines = Vec::new();
        let ended = read_heads(Cursor::new(text), limit, &mut |line, cut| {
            lines.push((line.to_string(), cut));
        });
        (lines, ended)
    }

    #[test]
    fn probe_lines_hands_over_each_line_and_the_start_of_long_ones() {
        let long = format!("k=\"{}\"", "x".repeat(MAX_LINE * 2 + 7));
        let exact = "y".repeat(MAX_LINE);
        let text = format!("a=1\r\n{long}\nb=2\n{exact}\n{exact}\r\n\nc=3");
        let (lines, ended) = heads(&text, u64::MAX);
        ended.unwrap();
        let want = [
            ("a=1", false),
            (&long[..MAX_LINE], true),
            ("b=2", false),
            // A line of exactly the buffer's size isn't cut by its line end.
            (exact.as_str(), false),
            (exact.as_str(), false),
            ("", false),
            // The last line needs no line end.
            ("c=3", false),
        ];
        let want: Vec<(String, bool)> = want.iter().map(|&(l, c)| (l.to_string(), c)).collect();
        assert_eq!(lines, want);
        // A character cut in two at the end of a long line's start.
        let wide = format!("a{}", "\u{e9}".repeat(MAX_LINE));
        let (lines, _) = heads(&wide, u64::MAX);
        let start = format!("a{}\u{fffd}", "\u{e9}".repeat(MAX_LINE / 2 - 1));
        assert_eq!(lines, [(start, true)]);
    }

    #[test]
    fn probe_lines_stop_once_past_their_limit() {
        let text = "a=1\nb=2\nc=3\n";
        // Exactly the limit is fine; one byte less stops before the line
        // that passes it.
        let (lines, ended) = heads(text, 12);
        assert_eq!(lines.len(), 3);
        ended.unwrap();
        let (lines, ended) = heads(text, 11);
        let names: Vec<&str> = lines.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(names, ["a=1", "b=2"]);
        assert!(
            matches!(ended, Err(RunError::TooMuch { limit: 11 })),
            "{ended:?}"
        );
        // Skipping a long line counts too.
        let long = "x".repeat(MAX_LINE * 3);
        let (lines, ended) = heads(&long, MAX_LINE as u64 * 2);
        assert!(lines.is_empty());
        assert!(ended.is_err());
    }

    #[test]
    fn probe_lines_stop_a_child_that_prints_too_much() {
        let token = CancelToken::new();
        let _watchdog = watchdog(&token);
        let limit = 1 << 20;
        let mut lines = 0;
        for mode in ["big", "big-hold"] {
            let started = Instant::now();
            let result = probe_lines_command(
                fake(mode),
                &token,
                Duration::from_secs(20),
                limit as u64,
                &mut |_, _| lines += 1,
            );
            assert!(too_much(&result, limit), "{mode}: {result:?}");
            assert!(started.elapsed() < Duration::from_secs(5), "{mode}");
        }
        // The harness's own lines, before the child's single long one.
        assert!(lines < 5, "{lines}");
        // A child that ends within its limit.
        let mut seen = Vec::new();
        let result = probe_lines_command(
            fake("probe"),
            &token,
            Duration::from_secs(10),
            limit as u64,
            &mut |line, cut| seen.push((line.to_string(), cut)),
        );
        result.unwrap();
        assert!(
            seen.contains(&("  \"format\": {".to_string(), false)),
            "{seen:?}"
        );
    }

    #[test]
    fn a_cancelled_line_probe_is_reaped() {
        let held = Held::new();
        let token = CancelToken::new();
        let canceller = {
            let (token, file) = (token.clone(), held.file.clone());
            thread::spawn(move || {
                let started = Instant::now();
                while !file.exists() && started.elapsed() < Duration::from_secs(10) {
                    thread::sleep(Duration::from_millis(5));
                }
                token.cancel();
            })
        };
        let result = probe_lines_command(
            holding(&held.file),
            &token,
            Duration::from_secs(20),
            u64::MAX,
            &mut |_, _| {},
        );
        assert!(matches!(result, Err(RunError::Cancelled)), "{result:?}");
        canceller.join().unwrap();
        held.assert_gone();
    }

    #[test]
    fn probe_times_out() {
        let token = CancelToken::new();
        let started = Instant::now();
        let result = probe_command(fake("sleep"), &token, Duration::from_millis(300));
        assert!(matches!(result, Err(RunError::TimedOut)), "{result:?}");
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(300), "{took:?}");
        // Generous, for a slow CI machine: the point is that it ends.
        assert!(took < Duration::from_secs(2), "{took:?}");
    }

    #[test]
    fn the_sidecar_never_inherits_ffreport() {
        // A user's FFREPORT would make FFmpeg write a report file, with the
        // command line and the source's path, wherever it points.
        let command = command(&std::env::current_exe().unwrap(), &[]).unwrap();
        assert!(command
            .get_envs()
            .any(|(key, value)| key == "FFREPORT" && value.is_none()));
    }

    #[test]
    fn relative_programs_are_refused_before_spawning() {
        let relative: &[&str] = if cfg!(windows) {
            &["ffmpeg", r"bin\ffmpeg.exe", r"\ffmpeg.exe", "C:ffmpeg.exe"]
        } else {
            &["ffmpeg", "bin/ffmpeg", "./ffmpeg"]
        };
        let token = CancelToken::new();
        for program in relative.iter().map(Path::new) {
            let result = command(program, &[]);
            assert!(
                matches!(&result, Err(RunError::NotAbsolute(p)) if p == program),
                "{result:?}"
            );
            let result = transcode(program, &[], &token, &mut |_| {});
            assert!(
                matches!(result, Err(RunError::NotAbsolute(_))),
                "{result:?}"
            );
            let result = probe(program, &[], &token, Duration::from_secs(1));
            assert!(
                matches!(result, Err(RunError::NotAbsolute(_))),
                "{result:?}"
            );
        }
    }

    /// An absolute path to a program that doesn't exist.
    fn missing_program() -> PathBuf {
        let path = std::env::current_exe()
            .unwrap()
            .with_file_name("grape-no-such-sidecar");
        assert!(path.is_absolute() && !path.exists());
        path
    }

    #[test]
    fn a_missing_program_fails_to_spawn() {
        let token = CancelToken::new();
        let missing = missing_program();
        let result = transcode(&missing, &[], &token, &mut |_| {});
        assert!(
            matches!(&result, Err(RunError::Spawn(e)) if e.kind() == io::ErrorKind::NotFound),
            "{result:?}"
        );
        let result = probe(&missing, &[], &token, Duration::from_secs(1));
        assert!(
            matches!(&result, Err(RunError::Spawn(e)) if e.kind() == io::ErrorKind::NotFound),
            "{result:?}"
        );
    }

    #[test]
    fn the_progress_parser_reads_out_time_us_only() {
        let mut parser = ProgressParser::new();
        let mut feed = |line| parser.feed(line);
        assert_eq!(
            feed("out_time_us=1500000"),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(feed("out_time_us=0\n"), Some(Duration::ZERO));
        assert_eq!(
            feed("out_time_us=2500000\r\n"),
            Some(Duration::from_millis(2500))
        );
        assert_eq!(
            feed("out_time_us=2500000\r"),
            Some(Duration::from_millis(2500))
        );
        assert_eq!(feed("out_time_us=N/A"), None);
        assert_eq!(feed("out_time_us=-23220"), None);
        assert_eq!(feed("out_time_us="), None);
        assert_eq!(feed("out_time_ms=1500000"), None);
        assert_eq!(feed("out_time=00:00:01.500000"), None);
        assert_eq!(feed("progress=continue"), None);
        assert_eq!(feed("progress=end\r\n"), None);
        assert_eq!(feed("running 1 test"), None);
        assert_eq!(
            feed("test media::run::tests::fake_child ... out_time_us=5"),
            None
        );
        assert_eq!(feed(""), None);
    }

    #[test]
    fn the_line_reader_skips_overlong_lines_and_keeps_the_last_one() {
        let overlong = "x".repeat(MAX_LINE * 2 + 7);
        let exact = format!("{}\n", "y".repeat(MAX_LINE - 1));
        let text = format!("a=1\r\n{overlong}\nb=2\n{exact}c=3");
        let mut lines = Vec::new();
        read_lines(Cursor::new(text), |line| lines.push(line.to_string()));
        assert_eq!(lines, ["a=1\r\n", "b=2\n", exact.as_str(), "c=3"]);
    }

    #[test]
    fn the_tail_keeps_the_last_16_kib() {
        let mut tail = Tail::default();
        tail.push(&[b'a'; 10 * 1024]);
        tail.push(&[b'b'; 10 * 1024]);
        let text = tail.render();
        assert_eq!(text.len(), TAIL_BYTES);
        assert_eq!(
            text,
            format!("{}{}", "a".repeat(6 * 1024), "b".repeat(10 * 1024))
        );

        let mut tail = Tail::default();
        let big: Vec<u8> = (0..TAIL_BYTES + 100)
            .map(|i| b'0' + (i % 10) as u8)
            .collect();
        tail.push(&big);
        assert_eq!(tail.render().as_bytes(), &big[100..]);

        let mut tail = Tail::default();
        tail.push(b"ok \xff");
        assert_eq!(tail.render(), "ok \u{fffd}");
    }

    #[test]
    fn a_third_job_waits_until_a_slot_frees() {
        let gate = Gate::new(MAX_FFMPEG_JOBS);
        let token = CancelToken::new();
        let first = gate.acquire(&token).unwrap();
        let second = gate.acquire(&token).unwrap();
        thread::scope(|scope| {
            let (sender, got) = mpsc::channel();
            let (gate, token) = (&gate, &token);
            let third = scope.spawn(move || {
                let guard = gate.acquire(token);
                sender.send(()).unwrap();
                guard.is_some()
            });
            assert_eq!(
                got.recv_timeout(Duration::from_millis(200)),
                Err(RecvTimeoutError::Timeout)
            );
            drop(first);
            got.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(third.join().unwrap());
        });
        drop(second);
        assert_eq!(*gate.busy.lock().unwrap(), 0);
    }

    #[test]
    fn a_cancelled_waiter_gets_no_slot() {
        let gate = Gate::new(1);
        let _held = gate.acquire(&CancelToken::new()).unwrap();
        let token = CancelToken::new();
        thread::scope(|scope| {
            let waiter = scope.spawn(|| gate.acquire(&token).is_none());
            thread::sleep(Duration::from_millis(100));
            let cancelled = Instant::now();
            token.cancel();
            assert!(waiter.join().unwrap());
            assert!(cancelled.elapsed() < Duration::from_millis(500));
        });
        assert_eq!(*gate.busy.lock().unwrap(), 1);
        // A token cancelled beforehand gets nothing, even from a free slot.
        assert!(Gate::new(1).acquire(&token).is_none());
    }

    #[test]
    fn the_ffmpeg_gate_has_two_slots() {
        let _serial = one_at_a_time();
        let token = CancelToken::new();
        let first = acquire(&token).unwrap();
        let second = acquire(&token).unwrap();
        let waiting = CancelToken::new();
        let canceller = {
            let waiting = waiting.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                waiting.cancel();
            })
        };
        assert!(acquire(&waiting).is_none());
        canceller.join().unwrap();
        drop((first, second));
        assert!(acquire(&token).is_some());
    }
}
