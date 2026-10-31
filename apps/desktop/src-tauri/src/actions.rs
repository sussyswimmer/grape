//! Runs a drop on the Actions wheel (`docs/TOOLS_AND_ACTIONS.md` → File
//! actions) and tells the user how it went in the toast.
//!
//! The work itself is in `grape_platform::file_actions`. This module picks
//! what each action works on, runs it on a thread of its own (Move waits on
//! a dialog, Share on the sheet) and words the toast. Terminal opens the app
//! chosen in Settings. Share is the share sheet on Windows and AirDrop on
//! macOS. Float puts the items on the shelf ([`crate::float`]) on Windows
//! and macOS.

use std::path::{Path, PathBuf};
use std::thread;

use grape_core::{ActionKey, ItemKind, SelectionItem};
use grape_platform::file_actions::{
    self, MoveOutcome, MovedItem, ShareOutcome, SkipReason, Skipped, TerminalApp,
};
use grape_platform::{DisplayBounds, PlatformError};
use tauri::{AppHandle, Manager};

use crate::events::{ToastShow, ToastTone};
#[cfg(any(target_os = "windows", target_os = "macos", test))]
use crate::float::{Added, CAPACITY};
use crate::settings;
use crate::toast::{self, Toaster};

/// What the toast says after an action.
#[derive(Debug, Clone, PartialEq)]
struct Report {
    tone: ToastTone,
    title: String,
    detail: Option<String>,
    /// What clicking the toast reveals in Explorer or Finder.
    reveal: Option<PathBuf>,
}

impl Report {
    fn new(tone: ToastTone, title: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            tone,
            title: title.into(),
            detail,
            reveal: None,
        }
    }

    fn revealing(mut self, path: PathBuf) -> Self {
        self.reveal = Some(path);
        self
    }
}

/// Runs `action` on `items` and shows the result in a toast near the wheel
/// at `anchor` on `display`. Returns at once; the action runs on its own
/// thread.
pub fn run(
    app: &AppHandle,
    action: ActionKey,
    items: &[SelectionItem],
    anchor: (i32, i32),
    display: DisplayBounds,
) {
    let items = items.to_vec();
    let terminal_dir = items.first().and_then(terminal_dir);
    let app_for_thread = app.clone();
    let spawned = thread::Builder::new()
        .name("grape-action".to_owned())
        .spawn(move || {
            let report = match action {
                // The shelf is a window, so Float needs the app; `perform`
                // doesn't take it.
                #[cfg(any(target_os = "windows", target_os = "macos"))]
                ActionKey::Float => floated(crate::float::add(&app_for_thread, &items, &display)),
                _ => {
                    let paths: Vec<PathBuf> = items.iter().map(|item| item.path.clone()).collect();
                    // Copied out of the settings here, off the wheel's thread.
                    let terminal_app = settings::terminal_app(&app_for_thread);
                    perform(
                        action,
                        &paths,
                        terminal_dir.as_deref(),
                        terminal_app,
                        anchor,
                    )
                }
            };
            if let Some(report) = report {
                show(&app_for_thread, report, anchor, &display);
            }
        });
    if let Err(err) = spawned {
        log::error!("could not start the {} action: {err}", action.key());
    }
}

/// Runs the action and words the result. `None`: nothing to say (the
/// terminal or the share sheet opened, or the user closed the folder
/// picker). Terminal opens `terminal_app` in `terminal_dir`; Share opens
/// near `anchor`, the wheel's center. Float on Windows and macOS never
/// comes here: [`run`] puts the items on the shelf.
fn perform(
    action: ActionKey,
    paths: &[PathBuf],
    terminal_dir: Option<&Path>,
    terminal_app: TerminalApp,
    anchor: (i32, i32),
) -> Option<Report> {
    let report = match action {
        ActionKey::Copy => word(action, file_actions::copy_files(paths), |result| {
            Some(copied(paths, result))
        }),
        ActionKey::CopyPath => word(action, file_actions::copy_paths(paths), |result| {
            Some(copied_paths(paths, result))
        }),
        ActionKey::Terminal => match terminal_dir {
            Some(dir) => word(
                action,
                file_actions::open_terminal(dir, terminal_app),
                opened_terminal,
            ),
            None => Some(Report::new(
                ToastTone::Error,
                "Couldn't open a terminal",
                Some("That item has no folder to open.".to_owned()),
            )),
        },
        ActionKey::Move => word(action, file_actions::move_to_chosen_folder(paths), moved),
        ActionKey::Share => word(action, file_actions::share(paths, Some(anchor)), |result| {
            shared(paths, result)
        }),
        ActionKey::Float => Some(later(action)),
    };
    log::info!(
        "{} on {} item(s): {}",
        action.key(),
        paths.len(),
        report
            .as_ref()
            .map_or("nothing to show", |report| report.title.as_str())
    );
    report
}

/// Words `result` with `words`. An action this OS doesn't have yet (Share
/// off Windows and macOS) gets "coming", not a failure.
fn word<T>(
    action: ActionKey,
    result: Result<T, PlatformError>,
    words: impl FnOnce(Result<T, PlatformError>) -> Option<Report>,
) -> Option<Report> {
    match result {
        Err(PlatformError::Unsupported) => Some(later(action)),
        result => words(result),
    }
}

fn show(app: &AppHandle, report: Report, anchor: (i32, i32), display: &DisplayBounds) {
    let toaster = app.state::<Toaster>();
    let id = toaster.next_id();
    if let Some(path) = &report.reveal {
        toaster.set_reveal(id, path.clone());
    }
    let toast = ToastShow {
        id,
        tone: report.tone,
        title: report.title,
        detail: report.detail,
        progress: None,
        revealable: report.reveal.is_some(),
    };
    toast::show(app, toast, anchor, display);
}

/// Where Terminal opens: a folder itself, or a file's parent.
fn terminal_dir(item: &SelectionItem) -> Option<PathBuf> {
    match item.kind {
        ItemKind::Folder => Some(item.path.clone()),
        ItemKind::File { .. } => item.path.parent().map(Path::to_path_buf),
    }
}

/// "photo.png", or the whole path when it has no last part (`D:\`).
fn name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn failed(title: &str, err: &PlatformError) -> Report {
    Report::new(ToastTone::Error, title, Some(err.to_string()))
}

fn copied(paths: &[PathBuf], result: Result<(), PlatformError>) -> Report {
    match (result, paths) {
        (Err(err), _) => failed("Couldn't copy", &err),
        (Ok(()), [one]) => Report::new(
            ToastTone::Done,
            format!("Copied {}", name(one)),
            Some("Paste it into any folder.".to_owned()),
        ),
        (Ok(()), many) => Report::new(
            ToastTone::Done,
            format!("Copied {} items", many.len()),
            Some("Paste them into any folder.".to_owned()),
        ),
    }
}

fn copied_paths(paths: &[PathBuf], result: Result<(), PlatformError>) -> Report {
    match (result, paths) {
        (Err(err), [_]) => failed("Couldn't copy the path", &err),
        (Err(err), _) => failed("Couldn't copy the paths", &err),
        (Ok(()), [one]) => Report::new(
            ToastTone::Done,
            "Copied the path",
            Some(one.display().to_string()),
        ),
        (Ok(()), many) => Report::new(
            ToastTone::Done,
            format!("Copied {} paths", many.len()),
            Some("One per line.".to_owned()),
        ),
    }
}

fn opened_terminal(result: Result<(), PlatformError>) -> Option<Report> {
    // The terminal window is the answer; only a failure needs a toast.
    result
        .err()
        .map(|err| failed("Couldn't open a terminal", &err))
}

/// The share sheet or AirDrop is the answer when everything went; a toast
/// says what was left out, or that nothing could go.
fn shared(paths: &[PathBuf], result: Result<ShareOutcome, PlatformError>) -> Option<Report> {
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(err) => return Some(failed("Couldn't share", &err)),
    };
    let skipped = &outcome.skipped;
    if !outcome.opened {
        let detail = if !skipped.is_empty() && skipped.iter().all(|s| s.why == SkipReason::Folder) {
            Some("Share sends files, not folders.".to_owned())
        } else {
            left_out(skipped)
        };
        return Some(Report::new(ToastTone::Info, "Nothing to share", detail));
    }
    if skipped.is_empty() {
        return None;
    }
    let of = paths.len();
    Some(Report::new(
        ToastTone::Info,
        format!("Sharing {} of {of}", of.saturating_sub(skipped.len())),
        left_out(skipped),
    ))
}

/// Why Share left `skipped` out: one item by name, several by their
/// shared reason, or in general when the reasons differ.
fn left_out(skipped: &[Skipped]) -> Option<String> {
    let first = skipped.first()?;
    let same = skipped.iter().all(|s| s.why == first.why);
    let words = match (skipped, first.why) {
        (_, SkipReason::Folder) if same => "Folders can't be shared.".to_owned(),
        ([one], SkipReason::Missing) => format!("{} is gone.", name(&one.path)),
        ([one], SkipReason::Unshareable) => format!("{} can't be shared.", name(&one.path)),
        (many, SkipReason::Missing) if same => format!("{} items are gone.", many.len()),
        (many, SkipReason::Unshareable) if same => {
            format!("{} items can't be shared.", many.len())
        }
        _ => "Some items can't be shared.".to_owned(),
    };
    Some(words)
}

/// Words a Float: the shelf is the answer, unless some items didn't fit
/// or it couldn't open.
#[cfg(any(target_os = "windows", target_os = "macos", test))]
fn floated(result: anyhow::Result<Added>) -> Option<Report> {
    match result {
        Err(err) => Some(Report::new(
            ToastTone::Error,
            "Couldn't open the shelf",
            Some(format!("{err:#}")),
        )),
        Ok(added) if added.left_out > 0 => Some(Report::new(
            ToastTone::Info,
            "The shelf is full",
            Some(format!("It holds {CAPACITY} items.")),
        )),
        Ok(_) => None,
    }
}

fn moved(result: Result<MoveOutcome, PlatformError>) -> Option<Report> {
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(err) => return Some(failed("Couldn't move", &err)),
    };
    match outcome {
        MoveOutcome::Cancelled => None,
        MoveOutcome::AlreadyThere { to } => Some(Report::new(
            ToastTone::Info,
            "Already there",
            Some(format!("Everything is already in {}.", name(&to))),
        )),
        MoveOutcome::Moved {
            to,
            moved,
            unsure,
            of,
        } => Some(moved_report(&to, &moved, &unsure, of)),
    }
}

/// The toast after a move ran: what left, and what Grape can't be sure of.
fn moved_report(to: &Path, moved: &[MovedItem], unsure: &[PathBuf], of: usize) -> Report {
    let folder = name(to);
    // Part of a folder may be in each place (a merge, a skip inside it, a
    // cancel), or an old place can't be read: say so and show the folder,
    // never "moved" or "stayed put".
    if let Some(first) = unsure.first() {
        let detail = match unsure {
            [_] => format!("Check {} in its old place and in {folder}.", name(first)),
            _ => format!("Check their old places and {folder}."),
        };
        return Report::new(
            ToastTone::Info,
            "Some of it may not have moved",
            Some(detail),
        )
        .revealing(to.to_path_buf());
    }
    let Some(first) = moved.first() else {
        return Report::new(
            ToastTone::Info,
            "Nothing moved",
            Some("Every item stayed where it was.".to_owned()),
        );
    };
    let report = match (moved, of) {
        ([one], 1) => Report::new(
            ToastTone::Done,
            format!("Moved {}", name(&one.from)),
            Some(format!("to {folder}")),
        ),
        (all, of) if all.len() == of => Report::new(
            ToastTone::Done,
            format!("Moved {of} items"),
            Some(format!("to {folder}")),
        ),
        (some, of) => Report::new(
            ToastTone::Info,
            format!("Moved {} of {of} items", some.len()),
            Some(format!("to {folder}. The rest stayed put.")),
        ),
    };
    report.revealing(first.now.clone())
}

/// An action this OS doesn't have yet: Float and Share off Windows and
/// macOS, everything on other hosts.
fn later(action: ActionKey) -> Report {
    let detail = match action {
        ActionKey::Share => "Sharing is coming.",
        ActionKey::Float => "The float shelf is coming.",
        ActionKey::Copy => "Copy is coming.",
        ActionKey::Move => "Move is coming.",
        ActionKey::CopyPath => "Copy Path is coming.",
        ActionKey::Terminal => "Terminal is coming.",
    };
    Report::new(
        ToastTone::Info,
        "Not in this build yet",
        Some(detail.to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_error() -> PlatformError {
        PlatformError::Os {
            context: "OpenClipboard",
            code: 5,
        }
    }

    #[test]
    fn copy_names_one_file_or_counts_many() {
        let one = copied(&[PathBuf::from("/a/photo.png")], Ok(()));
        assert_eq!(one.tone, ToastTone::Done);
        assert_eq!(one.title, "Copied photo.png");
        assert_eq!(one.detail.as_deref(), Some("Paste it into any folder."));
        let many = copied(&[PathBuf::from("/a"), PathBuf::from("/b")], Ok(()));
        assert_eq!(many.title, "Copied 2 items");
        let failed = copied(&[PathBuf::from("/a")], Err(os_error()));
        assert_eq!(failed.tone, ToastTone::Error);
        assert_eq!(failed.title, "Couldn't copy");
    }

    #[test]
    fn copy_path_shows_the_path_or_counts_lines() {
        let one = copied_paths(&[PathBuf::from("/a/photo.png")], Ok(()));
        assert_eq!(one.title, "Copied the path");
        assert_eq!(one.detail.as_deref(), Some("/a/photo.png"));
        let many = copied_paths(&[PathBuf::from("/a"), PathBuf::from("/b")], Ok(()));
        assert_eq!(many.title, "Copied 2 paths");
        assert_eq!(many.detail.as_deref(), Some("One per line."));
    }

    #[test]
    fn terminal_is_quiet_unless_it_fails() {
        assert_eq!(opened_terminal(Ok(())), None);
        let failed = opened_terminal(Err(os_error())).unwrap();
        assert_eq!(failed.tone, ToastTone::Error);
        assert_eq!(failed.title, "Couldn't open a terminal");
    }

    #[test]
    fn terminal_opens_a_folder_or_a_files_parent() {
        assert_eq!(
            terminal_dir(&SelectionItem::folder("/a/b")),
            Some(PathBuf::from("/a/b"))
        );
        assert_eq!(
            terminal_dir(&SelectionItem::file("/a/b/c.png")),
            Some(PathBuf::from("/a/b"))
        );
    }

    #[test]
    fn a_closed_picker_says_nothing() {
        assert_eq!(moved(Ok(MoveOutcome::Cancelled)), None);
    }

    fn item(from: &str, now: &str) -> MovedItem {
        MovedItem {
            from: PathBuf::from(from),
            now: PathBuf::from(now),
        }
    }

    fn outcome(moved: Vec<MovedItem>, unsure: Vec<PathBuf>, of: usize) -> MoveOutcome {
        MoveOutcome::Moved {
            to: PathBuf::from("/b/into"),
            moved,
            unsure,
            of,
        }
    }

    #[test]
    fn a_full_move_is_done_and_reveals_where_the_first_item_is_now() {
        let report = moved(Ok(outcome(
            vec![item("/a/photo.png", "/b/into/photo (2).png")],
            vec![],
            1,
        )))
        .unwrap();
        assert_eq!(report.tone, ToastTone::Done);
        assert_eq!(report.title, "Moved photo.png");
        assert_eq!(report.detail.as_deref(), Some("to into"));
        assert_eq!(report.reveal, Some(PathBuf::from("/b/into/photo (2).png")));

        let many = moved(Ok(outcome(
            vec![item("/a", "/b/into/a"), item("/b", "/b/into/b")],
            vec![],
            2,
        )))
        .unwrap();
        assert_eq!(many.title, "Moved 2 items");
        assert_eq!(many.reveal, Some(PathBuf::from("/b/into/a")));
    }

    #[test]
    fn a_partial_move_says_how_many_and_none_says_so() {
        let some = moved(Ok(outcome(vec![item("/a", "/b/into/a")], vec![], 3))).unwrap();
        assert_eq!(some.tone, ToastTone::Info);
        assert_eq!(some.title, "Moved 1 of 3 items");
        assert_eq!(
            some.detail.as_deref(),
            Some("to into. The rest stayed put.")
        );
        let none = moved(Ok(outcome(vec![], vec![], 3))).unwrap();
        assert_eq!(none.title, "Nothing moved");
        assert_eq!(none.reveal, None);
    }

    #[test]
    fn an_unsure_item_never_reads_as_moved_or_stayed() {
        let one = moved(Ok(outcome(vec![], vec![PathBuf::from("/a/Trip")], 1))).unwrap();
        assert_eq!(one.tone, ToastTone::Info);
        assert_eq!(one.title, "Some of it may not have moved");
        assert_eq!(
            one.detail.as_deref(),
            Some("Check Trip in its old place and in into.")
        );
        assert_eq!(one.reveal, Some(PathBuf::from("/b/into")));
        let many = moved(Ok(outcome(
            vec![item("/a/x.png", "/b/into/x.png")],
            vec![PathBuf::from("/a/Trip"), PathBuf::from("/a/Pics")],
            3,
        )))
        .unwrap();
        assert_eq!(
            many.detail.as_deref(),
            Some("Check their old places and into.")
        );
    }

    #[test]
    fn already_there_and_errors_get_their_own_words() {
        let there = moved(Ok(MoveOutcome::AlreadyThere {
            to: PathBuf::from("/into"),
        }))
        .unwrap();
        assert_eq!(there.title, "Already there");
        assert_eq!(
            there.detail.as_deref(),
            Some("Everything is already in into.")
        );
        let failed = moved(Err(os_error())).unwrap();
        assert_eq!(failed.tone, ToastTone::Error);
    }

    fn skip(path: &str, why: SkipReason) -> Skipped {
        Skipped {
            path: PathBuf::from(path),
            why,
        }
    }

    fn share(opened: bool, skipped: Vec<Skipped>) -> Result<ShareOutcome, PlatformError> {
        Ok(ShareOutcome { opened, skipped })
    }

    fn three() -> Vec<PathBuf> {
        ["/a/photo.png", "/a/Trip", "/a/notes.txt"]
            .map(PathBuf::from)
            .to_vec()
    }

    #[test]
    fn share_is_quiet_when_everything_went() {
        assert_eq!(shared(&three(), share(true, vec![])), None);
        let failed = shared(&three(), Err(os_error())).unwrap();
        assert_eq!(failed.tone, ToastTone::Error);
        assert_eq!(failed.title, "Couldn't share");
    }

    #[test]
    fn share_says_what_it_left_out() {
        let folder = shared(
            &three(),
            share(true, vec![skip("/a/Trip", SkipReason::Folder)]),
        )
        .unwrap();
        assert_eq!(folder.tone, ToastTone::Info);
        assert_eq!(folder.title, "Sharing 2 of 3");
        assert_eq!(folder.detail.as_deref(), Some("Folders can't be shared."));

        let gone = shared(
            &three(),
            share(true, vec![skip("/a/notes.txt", SkipReason::Missing)]),
        )
        .unwrap();
        assert_eq!(gone.detail.as_deref(), Some("notes.txt is gone."));

        let shortcut = shared(
            &three(),
            share(true, vec![skip("/a/notes.txt", SkipReason::Unshareable)]),
        )
        .unwrap();
        assert_eq!(
            shortcut.detail.as_deref(),
            Some("notes.txt can't be shared.")
        );

        let mixed = shared(
            &three(),
            share(
                true,
                vec![
                    skip("/a/Trip", SkipReason::Folder),
                    skip("/a/notes.txt", SkipReason::Missing),
                ],
            ),
        )
        .unwrap();
        assert_eq!(mixed.title, "Sharing 1 of 3");
        assert_eq!(mixed.detail.as_deref(), Some("Some items can't be shared."));

        let both_gone = shared(
            &three(),
            share(
                true,
                vec![
                    skip("/a/photo.png", SkipReason::Missing),
                    skip("/a/notes.txt", SkipReason::Missing),
                ],
            ),
        )
        .unwrap();
        assert_eq!(both_gone.detail.as_deref(), Some("2 items are gone."));
    }

    #[test]
    fn share_with_nothing_left_says_why() {
        let folders = shared(
            &[PathBuf::from("/a/Trip"), PathBuf::from("/a/Pics")],
            share(
                false,
                vec![
                    skip("/a/Trip", SkipReason::Folder),
                    skip("/a/Pics", SkipReason::Folder),
                ],
            ),
        )
        .unwrap();
        assert_eq!(folders.tone, ToastTone::Info);
        assert_eq!(folders.title, "Nothing to share");
        assert_eq!(
            folders.detail.as_deref(),
            Some("Share sends files, not folders.")
        );

        let gone = shared(
            &[PathBuf::from("/a/notes.txt")],
            share(false, vec![skip("/a/notes.txt", SkipReason::Missing)]),
        )
        .unwrap();
        assert_eq!(gone.title, "Nothing to share");
        assert_eq!(gone.detail.as_deref(), Some("notes.txt is gone."));

        let mixed = shared(
            &three(),
            share(
                false,
                vec![
                    skip("/a/photo.png", SkipReason::Unshareable),
                    skip("/a/Trip", SkipReason::Folder),
                    skip("/a/notes.txt", SkipReason::Unshareable),
                ],
            ),
        )
        .unwrap();
        assert_eq!(mixed.detail.as_deref(), Some("Some items can't be shared."));
    }

    #[test]
    fn the_shelf_is_quiet_unless_it_is_full_or_wont_open() {
        let added = |added, already, left_out| {
            Ok(Added {
                added,
                already,
                left_out,
            })
        };
        assert_eq!(floated(added(2, 0, 0)), None);
        assert_eq!(floated(added(0, 3, 0)), None);
        let full = floated(added(1, 0, 4)).unwrap();
        assert_eq!(full.tone, ToastTone::Info);
        assert_eq!(full.title, "The shelf is full");
        assert_eq!(full.detail.as_deref(), Some("It holds 50 items."));
        let failed = floated(Err(anyhow::anyhow!("no window"))).unwrap();
        assert_eq!(failed.tone, ToastTone::Error);
        assert_eq!(failed.title, "Couldn't open the shelf");
    }

    // On Windows and macOS Float goes to the shelf in `run`, never
    // `perform`.
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    #[test]
    fn float_is_coming_where_there_is_no_shelf() {
        let float = perform(
            ActionKey::Float,
            &[PathBuf::from("/a")],
            None,
            TerminalApp::WindowsTerminal,
            (0, 0),
        )
        .unwrap();
        assert_eq!(float.tone, ToastTone::Info);
        assert_eq!(float.detail.as_deref(), Some("The float shelf is coming."));
    }

    // Share opens the share sheet on Windows and AirDrop on macOS, so it's
    // never run from a test there.
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    #[test]
    fn share_is_coming_where_there_is_no_sheet() {
        let share = perform(
            ActionKey::Share,
            &[PathBuf::from("/a")],
            None,
            TerminalApp::WindowsTerminal,
            (0, 0),
        )
        .unwrap();
        assert_eq!(share.tone, ToastTone::Info);
        assert_eq!(share.detail.as_deref(), Some("Sharing is coming."));
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    #[test]
    fn off_windows_every_action_is_coming_not_failing() {
        let any = TerminalApp::WindowsTerminal;
        for action in [ActionKey::Copy, ActionKey::CopyPath, ActionKey::Move] {
            let report = perform(
                action,
                &[PathBuf::from("/a")],
                Some(Path::new("/")),
                any,
                (0, 0),
            )
            .unwrap();
            assert_eq!(report.tone, ToastTone::Info, "{action:?}");
            assert_eq!(report.title, "Not in this build yet");
        }
        for app in [
            TerminalApp::WindowsTerminal,
            TerminalApp::PowerShell,
            TerminalApp::Terminal,
            TerminalApp::ITerm2,
        ] {
            let terminal = perform(
                ActionKey::Terminal,
                &[PathBuf::from("/a")],
                Some(Path::new("/")),
                app,
                (0, 0),
            )
            .unwrap();
            assert_eq!(terminal.detail.as_deref(), Some("Terminal is coming."));
        }
    }
}
