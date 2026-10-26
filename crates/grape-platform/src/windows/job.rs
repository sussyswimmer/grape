//! The Job Object a helper process runs in (DECISIONS D33, heic-spec §6).
//!
//! Grape starts `grape-heif.exe` and puts it in a job of its own right
//! away, before it writes the helper's request (the helper waits for it, so
//! nothing runs outside the job). The job:
//! - ends the helper when it closes: when the [`JobGuard`] drops, and when
//!   Grape itself exits or crashes, because Grape holds its only handle;
//! - ends it on an unhandled exception instead of showing an error dialog;
//! - holds one process at most, so the helper can't start another;
//! - caps the helper's committed memory, when asked;
//! - keeps it away from the desktop, the clipboard, global atoms, other
//!   processes' windows (USER handles), display and system settings, and
//!   shutting Windows down (heic-spec J14).
//!
//! It isn't a sandbox: the helper still has the user's rights to files.

use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::process::Child;

use ::windows::core::PCWSTR;
use ::windows::Win32::Foundation::{CloseHandle, HANDLE};
use ::windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicUIRestrictions,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    JOBOBJECTINFOCLASS, JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
};

use crate::PlatformError;

/// The context of every error here.
const CONTEXT: &str = "assigning the helper to a job object";

/// A Job Object holding one helper process (D33). Dropping it closes the
/// job, which ends the helper if it's still running (`KILL_ON_JOB_CLOSE`);
/// so does Grape exiting.
#[derive(Debug)]
pub struct JobGuard {
    job: HANDLE,
}

// SAFETY: a job handle is a kernel handle, usable from any thread; the guard
// only closes it once, in drop.
unsafe impl Send for JobGuard {}

impl JobGuard {
    /// Makes a new job with the limits above and puts `child` in it.
    /// `max_memory` caps the helper's committed memory, in bytes
    /// (`JOB_OBJECT_LIMIT_PROCESS_MEMORY`); `None` leaves it uncapped.
    ///
    /// Call it right after the spawn and before the helper is given any
    /// work.
    ///
    /// # Errors
    /// [`PlatformError::Os`] if the job can't be made, configured or given
    /// the process. The caller should then kill the helper: it isn't
    /// contained (heic-spec J6).
    pub fn assign(child: &Child, max_memory: Option<u64>) -> Result<JobGuard, PlatformError> {
        // SAFETY: no security attributes and no name: a new, unnamed job
        // that only the handle returned here reaches.
        let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.map_err(|e| os_err(&e))?;
        // From here on, an early return closes the job.
        let guard = JobGuard { job };

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        let mut flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        if let Some(max) = max_memory {
            flags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            limits.ProcessMemoryLimit = usize::try_from(max).unwrap_or(usize::MAX);
        }
        limits.BasicLimitInformation.LimitFlags = flags;
        guard.set(JobObjectExtendedLimitInformation, &limits)?;

        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
                | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                | JOB_OBJECT_UILIMIT_EXITWINDOWS
                | JOB_OBJECT_UILIMIT_GLOBALATOMS
                | JOB_OBJECT_UILIMIT_HANDLES
                | JOB_OBJECT_UILIMIT_READCLIPBOARD
                | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
                | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
        };
        guard.set(JobObjectBasicUIRestrictions, &ui)?;

        let process = HANDLE(child.as_raw_handle());
        // SAFETY: both handles are open: the job is the guard's, and the
        // process handle is `child`'s own, borrowed for this call.
        unsafe { AssignProcessToJobObject(guard.job, process) }.map_err(|e| os_err(&e))?;
        Ok(guard)
    }

    /// The most memory the helper ever committed (`PeakProcessMemoryUsed`),
    /// for the "needs too much memory" message. `None` if the job can't be
    /// asked.
    pub fn peak_memory(&self) -> Option<u64> {
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // SAFETY: the job handle is open, and the pointer and size describe
        // `info`, the structure this information class fills.
        unsafe {
            QueryInformationJobObject(
                Some(self.job),
                JobObjectExtendedLimitInformation,
                (&raw mut info).cast::<c_void>(),
                size_u32::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
                None,
            )
        }
        .ok()?;
        u64::try_from(info.PeakProcessMemoryUsed).ok()
    }

    /// `SetInformationJobObject` with `info`, which must be the structure
    /// `class` takes.
    fn set<T>(&self, class: JOBOBJECTINFOCLASS, info: &T) -> Result<(), PlatformError> {
        // SAFETY: the job handle is open, and the pointer and size describe
        // `info`; both callers pass the structure their class takes.
        unsafe {
            SetInformationJobObject(
                self.job,
                class,
                std::ptr::from_ref(info).cast::<c_void>(),
                size_u32::<T>(),
            )
        }
        .map_err(|e| os_err(&e))
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        // SAFETY: the guard owns this handle and closes it only here. If the
        // helper is still running, the job's KILL_ON_JOB_CLOSE ends it.
        let _ = unsafe { CloseHandle(self.job) };
    }
}

/// A structure's size for a Win32 length argument (these are all small).
fn size_u32<T>() -> u32 {
    u32::try_from(size_of::<T>()).unwrap_or(u32::MAX)
}

fn os_err(err: &::windows::core::Error) -> PlatformError {
    PlatformError::Os {
        context: CONTEXT,
        code: err.code().0,
    }
}
