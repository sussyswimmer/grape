//! The helper's Job Object (`grape_platform::windows::job`, DECISIONS D33,
//! heic-spec §14). Windows only.
//!
//! The tests start this test binary again as the helper, with
//! `--exact <child test> --ignored`, and hold the child on its stdin until
//! it's in the job, as Grape does with `grape-heif.exe`: nothing runs
//! outside the job.
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

use grape_platform::windows::job::JobGuard;

const MIB: u64 = 1024 * 1024;

/// What the child test does once it reads its go-ahead.
const CHILD_ENV: &str = "GRAPE_TEST_JOB_CHILD";

/// This test binary, running only `test`, held on stdin.
fn spawn_child(test: &str, task: &str) -> Child {
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
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// Lets a held child go.
fn go(child: &mut Child) {
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"go\n").unwrap();
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

/// grape-heif.exe at HEVC level 6.2's largest picture (8192×4352, 35.6 MP)
/// under the cap Grape uses (heic-spec J24). Prints the peak for tuning.
#[cfg(feature = "heif-it")]
#[test]
fn grape_heif_encodes_the_largest_picture_under_the_cap() {
    use std::io::BufWriter;

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
    let mut child = Command::new(exe)
        .arg(WORKER_FLAG)
        .current_dir(&work)
        .env_remove("LIBHEIF_PLUGIN_PATH")
        .env_remove("LIBHEIF_SECURITY_LIMITS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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
