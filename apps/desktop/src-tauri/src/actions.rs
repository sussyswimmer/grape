//! Runs a drop on the Actions wheel (`docs/TOOLS_AND_ACTIONS.md` → File
//! actions) and tells the user how it went in the toast.
//!
//! The work itself is in `grape_platform::file_actions`. This module picks
//! what each action works on, runs it on a thread of its own (Move waits on
//! a dialog) and words the toast. Share and Float arrive in a later build.

use std::path::{Path, PathBuf};
use std::thread;

use grape_core::{ActionKey, ItemKind, SelectionItem};
use grape_platform::file_actions::{self, MoveOutcome};
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
    /// What clicking the toast reveals in Explorer.
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
        ActionKey::Share | ActionKey::Float => Some(later(action)),
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

/// Words `result` with `words`. Before M8 nothing is wired on macOS, so an
/// unsupported OS gets "coming", not a failure.
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
        MoveOutcome::Moved { moved, .. } if moved.is_empty() => Some(Report::new(
            ToastTone::Info,
            "Nothing moved",
            Some("Every item stayed where it was.".to_owned()),
        )),
        MoveOutcome::Moved { to, moved, of } => {
            // The first item in its new place, or the folder if it was
            // renamed on the way (a name clash the user kept both of).
            let reveal = moved[0]
                .file_name()
                .map(|file| to.join(file))
                .filter(|path| path.exists())
                .unwrap_or_else(|| to.clone());
            let report = match (moved.as_slice(), of) {
                ([one], 1) => Report::new(
                    ToastTone::Done,
                    format!("Moved {}", name(one)),
                    Some(format!("to {}", name(&to))),
                ),
                (all, of) if all.len() == of => Report::new(
                    ToastTone::Done,
                    format!("Moved {of} items"),
                    Some(format!("to {}", name(&to))),
                ),
                (some, of) => Report::new(
                    ToastTone::Info,
                    format!("Moved {} of {of} items", some.len()),
                    Some(format!("to {}. The rest stayed put.", name(&to))),
                ),
            };
            Some(report.revealing(reveal))
        }
    }
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

    #[test]
    fn a_full_move_is_done_and_reveals_the_folder_when_the_item_is_gone() {
        let report = moved(Ok(MoveOutcome::Moved {
            to: PathBuf::from("/nowhere/into"),
            moved: vec![PathBuf::from("/a/photo.png")],
            of: 1,
        }))
        .unwrap();
        assert_eq!(report.tone, ToastTone::Done);
        assert_eq!(report.title, "Moved photo.png");
        assert_eq!(report.detail.as_deref(), Some("to into"));
        assert_eq!(report.reveal, Some(PathBuf::from("/nowhere/into")));

        let many = moved(Ok(MoveOutcome::Moved {
            to: PathBuf::from("/into"),
            moved: vec![PathBuf::from("/a"), PathBuf::from("/b")],
            of: 2,
        }))
        .unwrap();
        assert_eq!(many.title, "Moved 2 items");
    }

    #[test]
    fn a_move_reveals_the_first_item_in_its_new_place() {
        let into = std::env::temp_dir().join(format!("grape-reveal-{}", std::process::id()));
        std::fs::create_dir_all(&into).unwrap();
        std::fs::write(into.join("photo.png"), b"grape").unwrap();
        let report = moved(Ok(MoveOutcome::Moved {
            to: into.clone(),
            moved: vec![PathBuf::from("/a/photo.png")],
            of: 1,
        }))
        .unwrap();
        let _ = std::fs::remove_dir_all(&into);
        assert_eq!(report.reveal, Some(into.join("photo.png")));
    }

    #[test]
    fn a_partial_move_says_how_many_and_none_says_so() {
        let some = moved(Ok(MoveOutcome::Moved {
            to: PathBuf::from("/into"),
            moved: vec![PathBuf::from("/a")],
            of: 3,
        }))
        .unwrap();
        assert_eq!(some.tone, ToastTone::Info);
        assert_eq!(some.title, "Moved 1 of 3 items");
        let none = moved(Ok(MoveOutcome::Moved {
            to: PathBuf::from("/into"),
            moved: vec![],
            of: 3,
        }))
        .unwrap();
        assert_eq!(none.title, "Nothing moved");
        assert_eq!(none.reveal, None);
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
    fn share_and_float_are_coming() {
        let share = perform(ActionKey::Share, &[PathBuf::from("/a")], None).unwrap();
        assert_eq!(share.tone, ToastTone::Info);
        assert_eq!(share.detail.as_deref(), Some("Sharing is coming."));
        let float = perform(ActionKey::Float, &[PathBuf::from("/a")], None).unwrap();
        assert_eq!(float.detail.as_deref(), Some("The float shelf is coming."));
    }

    #[cfg(not(target_os = "windows"))]
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
