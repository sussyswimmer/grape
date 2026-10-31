//! The apps the user was last in, other than Grape (D43). The float shelf
//! lives a long time and the user switches apps while it's open, so when
//! it hides after a click made it key, activation goes back to the app the
//! user was last in that still runs, not to the one in front when the
//! shelf was shown.
//!
//! [`follow_app_activations`] adds one observer, at launch on the main
//! thread, for `NSWorkspaceDidActivateApplicationNotification` on the
//! workspace's notification center (a selector, so no block). Each
//! activation of another app moves its pid to the front of a short list
//! (`crate::mac_rules::note_activation`). Only pids are kept, never a name
//! or a path, and nothing is logged.

use std::cell::OnceCell;
use std::sync::{Mutex, PoisonError};

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyObject, NSObject};
use objc2::{define_class, msg_send, sel, AnyThread, MainThreadMarker};
use objc2_app_kit::{
    NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey,
    NSWorkspaceDidActivateApplicationNotification,
};
use objc2_foundation::NSNotification;

use super::actions::frontmost_other_app;
use crate::mac_rules::note_activation;
use crate::PlatformError;

/// The pids of the apps last activated other than Grape, most recent
/// first. Written by the observer, read by the shelf's hide.
static RECENT: Mutex<Vec<i32>> = Mutex::new(Vec::new());

thread_local! {
    /// The observer once Grape follows activations, on the main thread.
    /// The notification center doesn't retain its observers, so this
    /// keeps it for the life of the app.
    static WATCHER: OnceCell<Retained<ActivationWatcher>> = const { OnceCell::new() };
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and
    // ActivationWatcher doesn't implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "GrapeActivationWatcher"]
    /// Hears every app's activation and notes the ones that aren't Grape.
    /// It holds nothing and only locks [`RECENT`], so any thread may call
    /// it.
    struct ActivationWatcher;

    impl ActivationWatcher {
        /// The workspace's `NSWorkspaceDidActivateApplicationNotification`.
        /// Never panics: it runs under the notification center's frames.
        #[unsafe(method(appActivated:))]
        fn app_activated(&self, notification: &NSNotification) {
            if let Some(pid) = activated_pid(notification) {
                note(pid);
            }
        }
    }
);

impl ActivationWatcher {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        // SAFETY: NSObject's designated initializer, on a fresh allocation.
        unsafe { msg_send![super(this), init] }
    }
}

/// Starts following which app the user is in: notes the app in front now
/// (unless it's Grape), then every app activated from now on. Call it once
/// on the main thread at launch, before anything activates Grape; calling
/// it again does nothing.
///
/// # Errors
/// [`PlatformError::WrongThread`] off the main thread.
pub fn follow_app_activations() -> Result<(), PlatformError> {
    let mtm = MainThreadMarker::new().ok_or(PlatformError::WrongThread)?;
    WATCHER.with(|watcher| {
        if watcher.get().is_some() {
            return;
        }
        autoreleasepool(|_| {
            if let Some(front) = frontmost_other_app(mtm) {
                note(front.processIdentifier());
            }
            let observer = ActivationWatcher::new();
            let center = NSWorkspace::sharedWorkspace().notificationCenter();
            // SAFETY: `observer` answers `appActivated:`, which takes the
            // one NSNotification the center passes, and WATCHER keeps it
            // for the life of the app, so the center never calls a freed
            // object. The name is AppKit's constant, valid for the life of
            // the program; no object, so every sender's notification comes.
            unsafe {
                center.addObserver_selector_name_object(
                    &observer,
                    sel!(appActivated:),
                    Some(NSWorkspaceDidActivateApplicationNotification),
                    None,
                );
            }
            // Can't be taken: this thread checked it was empty above.
            let _ = watcher.set(observer);
        });
    });
    Ok(())
}

/// The app the user was last in, other than Grape, that still runs. `None`
/// before [`follow_app_activations`] or when none of them runs any more.
pub(super) fn latest_running_app() -> Option<Retained<NSRunningApplication>> {
    let recent = RECENT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    recent.into_iter().find_map(running_app)
}

/// The app with `pid`, if it still runs.
pub(super) fn running_app(pid: i32) -> Option<Retained<NSRunningApplication>> {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .filter(|app| !app.isTerminated())
}

/// The pid of the app an activation notification names
/// (`NSWorkspaceApplicationKey` in its `userInfo`).
fn activated_pid(notification: &NSNotification) -> Option<i32> {
    let info = notification.userInfo()?;
    // SAFETY: AppKit's constant, an NSString valid for the life of the
    // program.
    let key: &AnyObject = unsafe { NSWorkspaceApplicationKey };
    let app = info
        .objectForKey(key)?
        .downcast::<NSRunningApplication>()
        .ok()?;
    Some(app.processIdentifier())
}

/// Notes that the app with `pid` became active.
fn note(pid: i32) {
    let grape = NSRunningApplication::currentApplication().processIdentifier();
    let mut recent = RECENT.lock().unwrap_or_else(PoisonError::into_inner);
    note_activation(&mut recent, pid, grape);
}

#[cfg(test)]
mod tests {
    use objc2_foundation::{NSDictionary, NSString};

    use super::*;

    /// A DidActivate notification as the workspace posts it, naming `app`.
    fn activated(app: &NSRunningApplication) -> Retained<NSNotification> {
        // SAFETY: AppKit's constants, valid for the life of the program.
        let (name, key) = unsafe {
            (
                NSWorkspaceDidActivateApplicationNotification,
                NSWorkspaceApplicationKey,
            )
        };
        let info = NSDictionary::<NSString, NSRunningApplication>::from_slices(&[key], &[app]);
        // SAFETY: an NSDictionary of NSString keys and NSRunningApplication
        // values is an NSDictionary of objects; the cast only forgets the
        // types. No object, as the workspace sends it.
        unsafe {
            NSNotification::notificationWithName_object_userInfo(
                name,
                None,
                Some(info.cast_unchecked::<AnyObject, AnyObject>()),
            )
        }
    }

    #[test]
    fn following_is_refused_off_the_main_thread() {
        // Tests don't run on the main thread.
        assert_eq!(follow_app_activations(), Err(PlatformError::WrongThread));
    }

    #[test]
    fn the_observer_notes_other_apps_and_never_grape() {
        let watcher = ActivationWatcher::new();
        let grape = NSRunningApplication::currentApplication();
        // Grape's own activation is never noted, through the real selector.
        // SAFETY: `appActivated:` takes one NSNotification and returns
        // nothing.
        let () = unsafe { msg_send![&*watcher, appActivated: &*activated(&grape)] };
        assert!(!RECENT
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&grape.processIdentifier()));
        // A notification without the app changes nothing and doesn't panic.
        // SAFETY: the name is AppKit's constant; no object.
        let bare = unsafe {
            NSNotification::notificationWithName_object(
                NSWorkspaceDidActivateApplicationNotification,
                None,
            )
        };
        let before = RECENT
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        // SAFETY: as for the first message.
        let () = unsafe { msg_send![&*watcher, appActivated: &*bare] };
        assert_eq!(
            *RECENT.lock().unwrap_or_else(PoisonError::into_inner),
            before
        );
        // Another app that runs (CI's Mac has Finder and the Dock): noted
        // first, and it's the one to hand back to.
        let grape_pid = grape.processIdentifier();
        let other = NSWorkspace::sharedWorkspace()
            .runningApplications()
            .iter()
            .find(|app| {
                let pid = app.processIdentifier();
                pid > 0 && pid != grape_pid && !app.isTerminated()
            });
        let Some(other) = other else {
            return;
        };
        // SAFETY: as for the first message.
        let () = unsafe { msg_send![&*watcher, appActivated: &*activated(&other)] };
        assert_eq!(
            RECENT
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .first(),
            Some(&other.processIdentifier())
        );
        assert_eq!(
            latest_running_app().map(|app| app.processIdentifier()),
            Some(other.processIdentifier())
        );
    }
}
