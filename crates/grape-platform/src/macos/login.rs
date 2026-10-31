//! macOS's own relaunch at login, kept off while Grape's launch at login is
//! on (`docs/PLATFORM_MACOS.md` → "Launch at login").
//!
//! By default macOS relaunches every app that was running at logout when
//! the user logs back in with "Reopen windows when logging back in" ticked
//! (whether loginwindow does so for an `LSUIElement` app like Grape is a
//! Mac check). Grape's LaunchAgent starts it at login too, so the two would
//! race: the second copy hands over to the first, and Settings could open.
//! Apple's remedy for an app launched "via some other mechanism (e.g.
//! launchd)" is `-[NSApplication disableRelaunchOnLogin]`. Those calls
//! change a counter that's relaunched at zero only, so
//! [`set_relaunch_on_login`] keeps at most one disable in effect and pairs
//! it with exactly one enable (`mac_rules::relaunch_call`).

use std::sync::atomic::{AtomicBool, Ordering};

use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;

use crate::mac_rules::{relaunch_call, RelaunchCall};
use crate::PlatformError;

/// Whether Grape's one `disableRelaunchOnLogin` is in effect. Only the main
/// thread changes it.
static DISABLED: AtomicBool = AtomicBool::new(false);

/// Lets macOS relaunch Grape at login (`relaunch`: launch at login is off)
/// or stops it (launch at login is on, so the LaunchAgent starts Grape).
/// Calls `disableRelaunchOnLogin` or `enableRelaunchOnLogin` only when that
/// changes something, so the calls stay paired however often it's asked.
/// Returns whether it called one. Main thread only.
///
/// # Errors
/// [`PlatformError::WrongThread`] off the main thread.
pub fn set_relaunch_on_login(relaunch: bool) -> Result<bool, PlatformError> {
    let mtm = MainThreadMarker::new().ok_or(PlatformError::WrongThread)?;
    let Some(call) = relaunch_call(DISABLED.load(Ordering::Acquire), relaunch) else {
        return Ok(false);
    };
    let app = NSApplication::sharedApplication(mtm);
    match call {
        RelaunchCall::Disable => app.disableRelaunchOnLogin(),
        RelaunchCall::Enable => app.enableRelaunchOnLogin(),
    }
    DISABLED.store(call == RelaunchCall::Disable, Ordering::Release);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_is_refused_off_the_main_thread() {
        // libtest runs every test on a thread of its own, and nothing
        // changes then.
        for relaunch in [false, true] {
            assert_eq!(
                set_relaunch_on_login(relaunch),
                Err(PlatformError::WrongThread)
            );
        }
        assert!(!DISABLED.load(Ordering::Acquire));
    }
}
