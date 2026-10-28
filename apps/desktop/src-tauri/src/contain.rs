//! Puts the helper processes Grape starts into an OS container (D33).
//!
//! On Windows every image helper (`grape-heif.exe`) and every SVG worker
//! (`Grape.exe --grape-svg-worker`, D39) runs in a Job Object of its own (`grape_platform::windows::job`): its memory is capped, it ends
//! when Grape does or when its job is dropped, it can't start another
//! process, an unhandled exception ends it without a dialog, and it's kept
//! from the desktop and the clipboard. When Grape itself runs inside a job
//! (`pnpm tauri dev` runs it through cargo, which makes one), the helper's
//! job is nested in that one and can't have the UI limits; the log says so
//! once. The engine
//! reaches it through the process-wide hook `grape_engines::set_contain`,
//! which [`install`] sets once in setup, before any drop can run. The
//! engine fails closed: a helper it can't contain is killed and its job
//! says the helper couldn't start.
//!
//! On macOS and elsewhere there's nothing to install: the image helper
//! worker and the SVG worker (Grape itself) end on their own once Grape is
//! gone, and cap their own memory with the 2 GiB heap bound they arm (D41,
//! D39).

/// Removes, off the calling thread, the image helper's and the SVG
/// worker's work folders a Grape that crashed or was killed left in the OS
/// temp folder (D33, D39), raw pixels included. Called once in setup, on every OS. Logs only the count.
pub fn remove_leftovers() {
    let spawned = std::thread::Builder::new()
        .name("grape-leftovers".to_owned())
        .spawn(|| {
            let removed = grape_engines::remove_stale_work_dirs();
            if removed > 0 {
                log::info!("contain: removed {removed} work folder(s) an earlier run left");
            }
        });
    if let Err(err) = spawned {
        log::warn!("contain: no thread to remove earlier work folders: {err}");
    }
}

/// Sets the engines' containment hook, once, in setup. Does nothing off
/// Windows.
pub fn install() {
    #[cfg(windows)]
    if !grape_engines::set_contain(std::sync::Arc::new(job::JobContain)) {
        // Only one setup runs per process, so this never happens; if it
        // did, the hook already set stays.
        log::warn!("contain: the helper hook was already set");
    }
}

#[cfg(windows)]
mod job {
    use std::process::Child;
    use std::sync::atomic::{AtomicBool, Ordering};

    use grape_engines::{Contain, Contained};
    use grape_platform::windows::job::JobGuard;

    /// A Job Object for each helper.
    #[derive(Debug)]
    pub(super) struct JobContain;

    /// Set once the log has said the helpers' jobs are nested.
    static SAID_NESTED: AtomicBool = AtomicBool::new(false);

    impl Contain for JobContain {
        fn contain(
            &self,
            child: &Child,
            max_memory: Option<u64>,
        ) -> Result<Box<dyn Contained>, String> {
            // PlatformError says what failed and its code, never a path.
            let guard = JobGuard::assign(child, max_memory).map_err(|err| err.to_string())?;
            if !guard.has_ui_limits() && !SAID_NESTED.swap(true, Ordering::Relaxed) {
                log::info!(
                    "contain: Grape runs inside a job, so the helpers' jobs are nested and have no UI limits"
                );
            }
            Ok(Box::new(Job(guard)))
        }
    }

    /// One helper's job. Dropping it closes the job, which ends the helper
    /// if it's still running.
    struct Job(JobGuard);

    impl Contained for Job {
        fn peak_memory(&self) -> Option<u64> {
            self.0.peak_memory()
        }
    }

    #[cfg(test)]
    mod tests {
        use std::io::Read;
        use std::process::{Command, Stdio};

        use super::*;

        /// The child for the test below: it waits for its stdin to close.
        #[test]
        #[ignore = "run by a_helper_is_contained_and_reports_its_peak"]
        fn contained_child() {
            let mut rest = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut rest);
        }

        #[test]
        fn a_helper_is_contained_and_reports_its_peak() {
            let exe = std::env::current_exe().expect("the test binary");
            let mut child = Command::new(exe)
                .args([
                    "--exact",
                    "contain::job::tests::contained_child",
                    "--ignored",
                    "--test-threads=1",
                    "-q",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("the child starts");
            // It's still waiting for its stdin, so it's running here.
            let contained = JobContain.contain(&child, Some(512 << 20));
            drop(child.stdin.take());
            let status = child.wait().expect("the child ends");
            let contained = contained.expect("the child is in a job");
            assert!(status.success(), "{status:?}");
            assert!(contained.peak_memory().is_some_and(|peak| peak > 0));
        }
    }
}
