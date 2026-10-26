//! Puts the helper processes Grape starts into an OS container (D33).
//!
//! On Windows every HEIC helper (`grape-heif.exe`) runs in a Job Object of
//! its own (`grape_platform::windows::job`): its memory is capped, it ends
//! when Grape does or when its job is dropped, it can't start another
//! process, and an unhandled exception ends it without a dialog. The engine
//! reaches it through the process-wide hook `grape_engines::set_contain`,
//! which [`install`] sets once in setup, before any drop can run. The
//! engine fails closed: a helper it can't contain is killed and its job
//! says the helper couldn't start.
//!
//! On macOS and elsewhere there's nothing to install: the HEIC worker
//! (Grape itself) ends on its own once Grape is gone, and a memory cap
//! there is a follow-up (D33).

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

    use grape_engines::{Contain, Contained};
    use grape_platform::windows::job::JobGuard;

    /// A Job Object for each helper.
    #[derive(Debug)]
    pub(super) struct JobContain;

    impl Contain for JobContain {
        fn contain(
            &self,
            child: &Child,
            max_memory: Option<u64>,
        ) -> Result<Box<dyn Contained>, String> {
            // PlatformError says what failed and its code, never a path.
            JobGuard::assign(child, max_memory)
                .map(|guard| Box::new(Job(guard)) as Box<dyn Contained>)
                .map_err(|err| err.to_string())
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
