//! The helper's Job Object (`grape_platform::windows::job`, DECISIONS D33,
//! heic-spec §14). Windows only.
//!
//! The tests start this test binary again as the helper, with
//! `--exact <child test> --ignored`, and hold the child on its stdin until
//! it's in the job, as Grape does with `grape-heif.exe`: nothing runs
//! outside the job.
//!
//! Under `cargo test` every child starts in cargo's own job, so the
//! helper's job is nested and has no UI limits (`windows::job`'s doc).
//! `ui_limits_are_set_unless_the_helper_is_already_in_a_job` checks both
//! cases; CI's heif-it-windows job runs this binary outside cargo too, with
//! `GRAPE_TEST_JOB_UI=1`, so the UI-limited job, the one an installed
//! Grape makes, is tested as well. GitHub's runner starts every step inside
//! a job of its own, so there the child is started to break away from it;
//! if that job doesn't allow it, the test says so with a warning (the PC
//! checks that case: TESTING.md → "HEIC, D33").
//!
//! With `--features heif-it` and `GRAPE_TEST_HEIF` set to `grape-heif.exe`,
//! one more test encodes HEVC level 6.2's largest picture under Grape's
//! 2 GiB cap and prints the peak (heic-spec J24). Without the variable it
//! fails rather than skips, so CI's heif-it-windows job can't pass by
//! accident.

#![cfg(windows)]
#![allow(clippy::unwrap_used)]

use std::io::{BufRead, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use std::os::windows::io::AsRawHandle;

use grape_platform::windows::job::JobGuard;
use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

const MIB: u64 = 1024 * 1024;

/// What the child test does once it reads its go-ahead.
const CHILD_ENV: &str = "GRAPE_TEST_JOB_CHILD";

/// `CREATE_BREAKAWAY_FROM_JOB`: the child starts outside the jobs this
/// process is in, or doesn't start when one of them doesn't allow that.
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// This test binary, running only `test`, held on stdin.
fn spawn_child(test: &str, task: &str) -> Child {
    spawn_child_with(test, task, 0).unwrap()
}

/// [`spawn_child`] with these process creation flags.
fn spawn_child_with(test: &str, task: &str, flags: u32) -> std::io::Result<Child> {
    use std::os::windows::process::CommandExt;

    Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            test,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
            "-q",
        ])
        .env(CHILD_ENV, task)
        .creation_flags(flags)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// Lets a held child go.
fn go(child: &mut Child) {
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"go\n").unwrap();
}

/// True when `child` is in a job already.
fn in_a_job(child: &Child) -> bool {
    let mut result = BOOL(0);
    // SAFETY: the child's handle is open and `result` is a live BOOL.
    unsafe { IsProcessInJob(HANDLE(child.as_raw_handle()), None, &raw mut result) }.unwrap();
    result.as_bool()
}

/// A job like cargo's (kill on close, nothing else), holding `child`: what
/// a helper is in when Grape runs inside a job. Closed when dropped.
struct OuterJob(HANDLE);

impl OuterJob {
    fn holding(child: &Child) -> OuterJob {
        // SAFETY: a new unnamed job; the limits structure is the one this
        // class takes; both handles are open for the assignment.
        unsafe {
            let job = OuterJob(CreateJobObjectW(None, PCWSTR::null()).unwrap());
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&info).cast(),
                u32::try_from(std::mem::size_of_val(&info)).unwrap(),
            )
            .unwrap();
            AssignProcessToJobObject(job.0, HANDLE(child.as_raw_handle())).unwrap();
            job
        }
    }
}

impl Drop for OuterJob {
    fn drop(&mut self) {
        // SAFETY: the handle is this guard's, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn wait_with_deadline(child: &mut Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The helper's side: waits for its go-ahead, then does `GRAPE_TEST_JOB_CHILD`.
#[test]
#[ignore = "the child of the tests below"]
fn job_child() {
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).unwrap();
    match std::env::var(CHILD_ENV).as_deref() {
        // 512 MiB in 8 MiB steps, kept: under a 128 MiB cap an allocation
        // fails part way. 3 means the cap stopped it.
        Ok("allocate") => {
            let mut kept: Vec<Vec<u8>> = Vec::with_capacity(64);
            for _ in 0..64 {
                let mut block: Vec<u8> = Vec::new();
                if block.try_reserve_exact(8 * MIB as usize).is_err() {
                    std::process::exit(3);
                }
                block.resize(8 * MIB as usize, 1);
                kept.push(block);
            }
            std::process::exit(0);
        }
        Ok("sleep") => std::thread::sleep(Duration::from_secs(60)),
        // A second process in a one-process job: 7 when it's refused.
        Ok("spawn") => {
            let started = Command::new("cmd.exe")
                .args(["/c", "exit 0"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            std::process::exit(match started {
                Err(_) => 7,
                Ok(mut second) => {
                    let _ = second.wait();
                    8
                }
            });
        }
        other => panic!("unknown child task {other:?}"),
    }
}

#[test]
fn the_memory_cap_stops_a_greedy_helper_and_the_peak_is_known() {
    let mut child = spawn_child("job_child", "allocate");
    let job = JobGuard::assign(&child, Some(128 * MIB)).unwrap();
    go(&mut child);
    let status = wait_with_deadline(&mut child, Duration::from_secs(60)).expect("still running");
    assert_eq!(status.code(), Some(3), "the cap didn't stop the allocation");
    let peak = job.peak_memory().unwrap();
    assert!(peak >= 100 * MIB, "peak {peak} bytes");
    assert!(peak <= 136 * MIB, "peak {peak} bytes is over the cap");
}

#[test]
fn dropping_the_guard_ends_the_helper() {
    let mut child = spawn_child("job_child", "sleep");
    let job = JobGuard::assign(&child, None).unwrap();
    go(&mut child);
    std::thread::sleep(Duration::from_millis(200));
    assert!(child.try_wait().unwrap().is_none(), "the child ended early");
    drop(job);
    let status = wait_with_deadline(&mut child, Duration::from_secs(1));
    assert!(status.is_some(), "the helper outlived its job");
}

#[test]
fn the_helper_cant_start_another_process() {
    let mut child = spawn_child("job_child", "spawn");
    let _job = JobGuard::assign(&child, None).unwrap();
    go(&mut child);
    let status = wait_with_deadline(&mut child, Duration::from_secs(30)).expect("still running");
    assert_eq!(
        status.code(),
        Some(7),
        "a second process started inside the job"
    );
}

/// Grape under `pnpm tauri dev` (cargo run) or a launcher's job: the helper
/// starts in that job, and a job with UI limits can't nest under it. The
/// helper still gets one, with every other limit.
#[test]
fn a_helper_already_in_a_job_gets_a_nested_one_with_its_limits() {
    let mut child = spawn_child("job_child", "spawn");
    let _outer = OuterJob::holding(&child);
    let job = JobGuard::assign(&child, None).expect("a nested job");
    assert!(!job.has_ui_limits());
    go(&mut child);
    let status = wait_with_deadline(&mut child, Duration::from_secs(30)).expect("still running");
    assert_eq!(status.code(), Some(7), "a second process started");

    let mut child = spawn_child("job_child", "allocate");
    let _outer = OuterJob::holding(&child);
    let job = JobGuard::assign(&child, Some(128 * MIB)).expect("a nested job");
    go(&mut child);
    let status = wait_with_deadline(&mut child, Duration::from_secs(60)).expect("still running");
    assert_eq!(status.code(), Some(3), "the cap didn't stop the allocation");
    assert!(job.peak_memory().unwrap() <= 136 * MIB);
}

/// The UI limits are set exactly when the helper is in no other job. Under
/// cargo it always is; CI also runs this outside cargo with
/// `GRAPE_TEST_JOB_UI=1`, which insists on the UI-limited case: the child
/// breaks away from the runner's own job, or the test warns that it can't.
#[test]
fn ui_limits_are_set_unless_the_helper_is_already_in_a_job() {
    let insist = std::env::var_os("GRAPE_TEST_JOB_UI").is_some_and(|v| v == "1");
    let (mut child, broke_away) = if insist {
        match spawn_child_with("job_child", "sleep", CREATE_BREAKAWAY_FROM_JOB) {
            Ok(child) => (child, true),
            Err(err) => {
                println!(
                    "::warning::GRAPE_TEST_JOB_UI=1, but this process's job lets no child break away ({err}), so the UI-limited job isn't tested here; the PC checks it (TESTING.md, HEIC, D33)"
                );
                (spawn_child("job_child", "sleep"), false)
            }
        }
    } else {
        (spawn_child("job_child", "sleep"), false)
    };
    let nested = in_a_job(&child);
    let job = JobGuard::assign(&child, Some(512 * MIB)).unwrap();
    println!(
        "helper job: {}",
        if nested {
            "nested in another job, no UI limits"
        } else {
            "on its own, with UI limits"
        }
    );
    assert_eq!(job.has_ui_limits(), !nested);
    if broke_away {
        assert!(
            !nested,
            "GRAPE_TEST_JOB_UI=1 and the helper broke away, but it's still inside a job: run this binary outside cargo"
        );
    }
    drop(job);
    let status = wait_with_deadline(&mut child, Duration::from_secs(5));
    assert!(status.is_some(), "the helper outlived its job");
}

/// grape-heif.exe at HEVC level 6.2's largest picture (8192×4352, 35.6 MP)
/// under the cap Grape uses (heic-spec J24). Prints the peak for tuning.
#[cfg(feature = "heif-it")]
#[test]
fn grape_heif_encodes_the_largest_picture_under_the_cap() {
    use std::io::BufWriter;
    use std::os::windows::process::CommandExt;

    use grape_heif_proto::{
        names, Limits, Message, Op, PixelHeader, PixelWriter, Request, PROTOCOL, WORKER_FLAG,
    };

    let exe = std::env::var_os("GRAPE_TEST_HEIF")
        .expect("GRAPE_TEST_HEIF: the grape-heif.exe to test (CI's grape-heif artifact)");
    let work = std::env::temp_dir().join(format!("grape-heif-job-test-{}", std::process::id()));
    std::fs::create_dir(&work).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(work.clone());

    let (width, height) = (8192u32, 4352u32);
    let header = PixelHeader {
        width,
        height,
        channels: 3,
        icc_len: 0,
    };
    let file = std::fs::File::create_new(work.join(names::INPUT)).unwrap();
    let mut writer = PixelWriter::begin(BufWriter::new(file), &header, None).unwrap();
    let mut row = vec![0u8; width as usize * 3];
    for y in 0..height {
        for x in 0..width {
            let px = &mut row[x as usize * 3..x as usize * 3 + 3];
            px[0] = (x * 255 / (width - 1)) as u8;
            px[1] = (y * 255 / (height - 1)) as u8;
            px[2] = ((x + y) % 256) as u8;
        }
        writer.row(&row).unwrap();
    }
    writer
        .finish()
        .unwrap()
        .into_inner()
        .unwrap()
        .sync_all()
        .unwrap();

    let request = Request {
        protocol: PROTOCOL,
        work_dir: work.clone(),
        op: Op::Encode { quality: None },
        limits: Limits::DEFAULT,
    };
    // As grape-engines' helper.rs starts it: DETACHED_PROCESS, so no console.
    let mut child = Command::new(exe)
        .arg(WORKER_FLAG)
        .current_dir(&work)
        .env_remove("LIBHEIF_PLUGIN_PATH")
        .env_remove("LIBHEIF_SECURITY_LIMITS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(0x0000_0008)
        .spawn()
        .unwrap();
    let job = JobGuard::assign(&child, Some(2048 * MIB)).unwrap();
    let started = Instant::now();
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(&serde_json::to_vec(&request).unwrap())
            .unwrap();
    }
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| serde_json::from_str::<Message>(&line).ok())
            .last()
    });
    let status = wait_with_deadline(&mut child, Duration::from_secs(600));
    if status.is_none() {
        let _ = child.kill();
    }
    let last = reader.join().unwrap();
    let peak = job.peak_memory().unwrap_or(0);
    println!(
        "grape-heif: 8192x4352 RGB encoded in {:.1} s, peak committed memory {} MiB (cap 2048 MiB)",
        started.elapsed().as_secs_f64(),
        peak / MIB
    );
    let status = status.expect("grape-heif.exe took over 10 minutes");
    match last {
        Some(Message::Done(done)) => assert!(done.bytes > 0),
        other => panic!("grape-heif.exe ended with {other:?} ({status})"),
    }
    assert!(status.success(), "{status}");
    let out = std::fs::metadata(work.join(names::OUTPUT)).unwrap();
    assert!(out.len() > 0);
}
