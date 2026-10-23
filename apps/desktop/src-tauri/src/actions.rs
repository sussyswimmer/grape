//! Runs a drop on the Actions wheel (`docs/TOOLS_AND_ACTIONS.md` → File
//! actions) and tells the user how it went in the toast.
//!
//! The work itself is in `grape_platform::file_actions`. This module picks
//! what each action works on, runs it on a thread of its own (Move waits on
//! a dialog) and words the toast. Share is AirDrop on macOS and arrives
//! later on Windows; Float arrives in a later build.

use std::path::{Path, PathBuf};
use std::thread;

use grape_core::{ActionKey, ItemKind, SelectionItem};
use grape_platform::file_actions::{self, MoveOutcome, MovedItem};
use grape_platform::{DisplayBounds, PlatformError};
use tauri::{AppHandle, Manager};

use crate::events::{ToastShow, ToastTone};
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
    let paths: Vec<PathBuf> = items.iter().map(|item| item.path.clone()).collect();
    let terminal_dir = items.first().and_then(terminal_dir);
    let app_for_thread = app.clone();
    let spawned = thread::Builder::new()
        .name("grape-action".to_owned())
        .spawn(move || {
            let report = perform(action, &paths, terminal_dir.as_deref());
            if let Some(report) = report {
                show(&app_for_thread, report, anchor, &display);
            }
        });
    if let Err(err) = spawned {
        log::error!("could not start the {} action: {err}", action.key());
    }
}

/// Runs the action and words the result. `None`: nothing to say (the
/// terminal opened, or the user closed the folder picker).
fn perform(action: ActionKey, paths: &[PathBuf], terminal_dir: Option<&Path>) -> Option<Report> {
    let report = match action {
        ActionKey::Copy => word(action, file_actions::copy_files(paths), |result| {
            Some(copied(paths, result))
        }),
        ActionKey::CopyPath => word(action, file_actions::copy_paths(paths), |result| {
            Some(copied_paths(paths, result))
        }),
        ActionKey::Terminal => match terminal_dir {
            Some(dir) => word(action, file_actions::open_terminal(dir), opened_terminal),
            None => Some(Report::new(
                ToastTone::Error,
                "Couldn't open a terminal",
                Some("That item has no folder to open.".to_owned()),
            )),
        },
        ActionKey::Move => word(action, file_actions::move_to_chosen_folder(paths), moved),
        ActionKey::Share => word(action, file_actions::share(paths), shared),
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
/// on Windows) gets "coming", not a failure.
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

fn shared(result: Result<(), PlatformError>) -> Option<Report> {
    // The AirDrop sheet is the answer; only a failure needs a toast.
    result.err().map(|err| failed("Couldn't share", &err))
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

/// Share and Float, until they're built.
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

    #[test]
    fn share_is_quiet_unless_it_fails() {
        assert_eq!(shared(Ok(())), None);
        let failed = shared(Err(os_error())).unwrap();
        assert_eq!(failed.tone, ToastTone::Error);
        assert_eq!(failed.title, "Couldn't share");
    }

    #[test]
    fn float_is_coming() {
        let float = perform(ActionKey::Float, &[PathBuf::from("/a")], None).unwrap();
        assert_eq!(float.tone, ToastTone::Info);
        assert_eq!(float.detail.as_deref(), Some("The float shelf is coming."));
    }

    // On macOS Share opens AirDrop, so it's never run from a test there.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn share_is_coming_where_there_is_no_airdrop() {
        let share = perform(ActionKey::Share, &[PathBuf::from("/a")], None).unwrap();
        assert_eq!(share.tone, ToastTone::Info);
        assert_eq!(share.detail.as_deref(), Some("Sharing is coming."));
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    #[test]
    fn off_windows_every_action_is_coming_not_failing() {
        for action in [ActionKey::Copy, ActionKey::CopyPath, ActionKey::Move] {
            let report = perform(action, &[PathBuf::from("/a")], Some(Path::new("/"))).unwrap();
            assert_eq!(report.tone, ToastTone::Info, "{action:?}");
            assert_eq!(report.title, "Not in this build yet");
        }
        let terminal = perform(
            ActionKey::Terminal,
            &[PathBuf::from("/a")],
            Some(Path::new("/")),
        )
        .unwrap();
        assert_eq!(terminal.detail.as_deref(), Some("Terminal is coming."));
    }
}
