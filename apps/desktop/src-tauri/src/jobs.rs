//! Runs a drop on a conversion or tool segment
//! (`docs/design/m1-drag-wheel.md` §F.5) and tells the user how it went in
//! the toast.
//!
//! - A conversion this build can't run gets an info toast at once, and
//!   nothing is read or written.
//! - Otherwise, on a thread of its own: build the selection
//!   (`grape_engines::build_selection`, which reads the disk), plan it
//!   (`grape_core::plan`), show "Making …", run each job with
//!   `grape_engines::run_job`, and finish with done, partial or error.
//!   Clicking the finished toast reveals the first output.
//! - Tools arrive in a later build.
//!
//! The engines never touch a source and never replace an existing file
//! (DECISIONS D16); this module only reads what they report.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use grape_core::{plan, Format, Job, Operation, SelectionItem, Settings, ToolParams, WheelEntry};
use grape_engines::{build_selection, run_job, supports, EngineCtx, GrapeError, JobOutcome};
use grape_platform::DisplayBounds;
use tauri::{AppHandle, Manager};

use crate::events::{ToastShow, ToastTone};
use crate::toast::{self, Toaster};

/// At most this often, a progress update goes to the toast.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);
/// Grape's own folder in Downloads, for outputs whose folder is read-only.
const FALLBACK_FOLDER: &str = "Grape";

/// What the toast says. `reveal` is what clicking it shows in Explorer.
#[derive(Debug, Clone, PartialEq)]
struct Report {
    tone: ToastTone,
    title: String,
    detail: Option<String>,
    /// 0–1 while working, when known.
    progress: Option<f32>,
    reveal: Option<PathBuf>,
}

impl Report {
    fn new(tone: ToastTone, title: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            tone,
            title: title.into(),
            detail,
            progress: None,
            reveal: None,
        }
    }
}

/// Runs a drop on `entry` (a conversion or a tool) for `items`, with the
/// toast near the wheel at `anchor` on `display`. Returns at once; the
/// jobs run on their own thread.
pub fn submit(
    app: &AppHandle,
    entry: WheelEntry,
    items: &[SelectionItem],
    anchor: (i32, i32),
    display: DisplayBounds,
) {
    let Some(op) = entry.operation(ToolParams::default()) else {
        log::warn!("jobs: {entry:?} isn't a job; file actions run elsewhere");
        return;
    };
    if let Some(report) = not_runnable(&op, items) {
        log::info!("jobs: {op:?} not in this build: {}", report.title);
        show(
            app,
            app.state::<Toaster>().next_id(),
            report,
            anchor,
            &display,
        );
        return;
    }
    let paths: Vec<PathBuf> = items.iter().map(|item| item.path.clone()).collect();
    let settings = Settings::new(fallback_dir(app));
    let app = app.clone();
    let spawned = thread::Builder::new()
        .name("grape-jobs".to_owned())
        .spawn(move || run(&app, &op, &paths, &settings, anchor, &display));
    if let Err(err) = spawned {
        log::error!("jobs: could not start the job thread: {err}");
    }
}

/// The info toast for a drop this build can't run, or `None` when it can.
/// Every item must be supported, so a mixed drop never half-runs.
fn not_runnable(op: &Operation, items: &[SelectionItem]) -> Option<Report> {
    match op {
        Operation::Tool { .. } => Some(Report::new(
            ToastTone::Info,
            "Tools arrive in a later build.",
            None,
        )),
        Operation::Convert { to } => {
            let item = items.iter().find(|item| !supports(op, item.format()))?;
            let from = item.format().map_or("This file", Format::display_name);
            Some(Report::new(
                ToastTone::Info,
                "Not in this build yet",
                Some(format!(
                    "{from} to {} is coming. JPG, PNG, WEBP, TIFF and BMP convert now.",
                    to.display_name()
                )),
            ))
        }
    }
}

/// `Downloads\Grape`. It's created only when a job actually uses it.
fn fallback_dir(app: &AppHandle) -> PathBuf {
    match app.path().download_dir() {
        Ok(downloads) => downloads.join(FALLBACK_FOLDER),
        Err(err) => {
            log::warn!("jobs: no Downloads folder ({err}); falling back to the temp folder");
            std::env::temp_dir().join(FALLBACK_FOLDER)
        }
    }
}

/// Plans and runs the jobs for one drop, updating the toast as it goes.
fn run(
    app: &AppHandle,
    op: &Operation,
    paths: &[PathBuf],
    settings: &Settings,
    anchor: (i32, i32),
    display: &DisplayBounds,
) {
    let id = app.state::<Toaster>().next_id();
    let selection = build_selection(paths, settings);
    let planned = match plan(&selection, op, settings) {
        Ok(planned) => planned,
        Err(err) => {
            log::error!(
                "jobs: could not plan {op:?} for {} item(s): {err}",
                paths.len()
            );
            let report = Report::new(ToastTone::Error, sentence(&err.to_string()), None);
            show(app, id, report, anchor, display);
            return;
        }
    };
    let jobs = planned.jobs;
    let ctx = EngineCtx::default();
    let mut results = Vec::with_capacity(jobs.len());
    let mut last_update: Option<Instant> = None;
    for (done, job) in jobs.iter().enumerate() {
        let started = working(&jobs, done, None);
        show(app, id, started, anchor, display);
        let mut progress = |fraction: f32| {
            if last_update.is_some_and(|at| at.elapsed() < PROGRESS_EVERY) {
                return;
            }
            last_update = Some(Instant::now());
            show(
                app,
                id,
                working(&jobs, done, Some(fraction)),
                anchor,
                display,
            );
        };
        let result = run_job(job, &ctx, &mut progress);
        match &result {
            Ok(outcome) => log::info!(
                "jobs: {} -> {}{}",
                job.inputs
                    .first()
                    .map_or_else(String::new, |input| input.display().to_string()),
                outcome
                    .outputs
                    .first()
                    .map_or_else(String::new, |output| output.display().to_string()),
                if outcome.renamed { " (renamed)" } else { "" }
            ),
            Err(err) => log::warn!("jobs: {err} ({})", err.detail),
        }
        results.push(result);
    }
    show(
        app,
        id,
        finished(&jobs, &results, settings),
        anchor,
        display,
    );
}

/// "Making photo.jpg…" or "Making 3 files…", while job `done` (counting
/// from 0) runs and has reported `fraction` of its work. Progress is for
/// the whole drop: unknown for a single job that hasn't reported yet.
fn working(jobs: &[Job], done: usize, fraction: Option<f32>) -> Report {
    let title = match jobs {
        [one] => format!("Making {}…", name(one.output.display_path())),
        many => format!("Making {} files…", many.len()),
    };
    let mut report = Report::new(ToastTone::Working, title, None);
    let fraction = fraction.filter(|fraction| fraction.is_finite());
    if jobs.len() > 1 || fraction.is_some() {
        let total = jobs.len().max(1) as f32;
        let part = fraction.map_or(0.0, |fraction| fraction.clamp(0.0, 1.0));
        report.progress = Some(((done as f32 + part) / total).clamp(0.0, 1.0));
    }
    report
}

/// The toast once every job ran: done, partial or error.
fn finished(
    jobs: &[Job],
    results: &[Result<JobOutcome, GrapeError>],
    settings: &Settings,
) -> Report {
    let outputs: Vec<&Path> = results
        .iter()
        .filter_map(|result| result.as_ref().ok())
        .filter_map(|outcome| outcome.outputs.first().map(PathBuf::as_path))
        .collect();
    let first_error = results.iter().find_map(|result| result.as_ref().err());
    let reveal = outputs.first().map(|path| path.to_path_buf());
    let mut report = match (outputs.as_slice(), first_error) {
        (_, None) => {
            let what = match outputs.as_slice() {
                [one] => name(one),
                many => format!("{} files", many.len()),
            };
            let used_fallback = jobs.iter().any(|job| job.used_fallback);
            let detail = if used_fallback {
                format!(
                    "{what}, saved in {} (the folder is read-only)",
                    short(&settings.fallback_dir)
                )
            } else {
                what
            };
            Report::new(ToastTone::Done, "Done", Some(detail))
        }
        ([], Some(err)) => Report::new(ToastTone::Error, err.user_message.clone(), None),
        (some, Some(err)) => Report::new(
            ToastTone::Error,
            format!("{} of {} done", some.len(), results.len()),
            Some(err.user_message.clone()),
        ),
    };
    report.reveal = reveal;
    report
}

fn show(app: &AppHandle, id: u64, report: Report, anchor: (i32, i32), display: &DisplayBounds) {
    let toaster = app.state::<Toaster>();
    if let Some(path) = &report.reveal {
        toaster.set_reveal(id, path.clone());
    }
    let toast = ToastShow {
        id,
        tone: report.tone,
        title: report.title,
        detail: report.detail,
        progress: report.progress,
        revealable: report.reveal.is_some(),
    };
    toast::show(app, toast, anchor, display);
}

/// "photo.jpg", or the whole path when it has no last part.
fn name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// The last two parts of a folder, e.g. `Downloads\Grape`.
fn short(folder: &Path) -> String {
    match (
        folder.parent().and_then(Path::file_name),
        folder.file_name(),
    ) {
        (Some(parent), Some(name)) => Path::new(parent).join(name).display().to_string(),
        _ => folder.display().to_string(),
    }
}

/// `err` as a sentence: a capital first letter.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grape_core::{JobOutput, ToolKey};
    use grape_engines::ErrorKind;

    fn job(input: &str, output: &str, used_fallback: bool) -> Job {
        Job {
            inputs: vec![PathBuf::from(input)],
            output: JobOutput::File(PathBuf::from(output)),
            op: Operation::Convert { to: Format::Jpg },
            used_fallback,
        }
    }

    fn wrote(path: &str) -> Result<JobOutcome, GrapeError> {
        Ok(JobOutcome {
            outputs: vec![PathBuf::from(path)],
            renamed: false,
        })
    }

    fn failed(message: &str) -> Result<JobOutcome, GrapeError> {
        Err(GrapeError {
            kind: ErrorKind::DecodeFailed,
            user_message: message.to_owned(),
            detail: "test".to_owned(),
        })
    }

    fn settings() -> Settings {
        Settings::new(PathBuf::from("/home/max/Downloads/Grape"))
    }

    #[test]
    fn raster_pairs_run_and_the_rest_say_they_are_coming() {
        let to_jpg = Operation::Convert { to: Format::Jpg };
        let png = [SelectionItem::file("/a/photo.png")];
        assert_eq!(not_runnable(&to_jpg, &png), None);

        let to_heic = Operation::Convert { to: Format::Heic };
        let report = not_runnable(&to_heic, &png).unwrap();
        assert_eq!(report.tone, ToastTone::Info);
        assert_eq!(report.title, "Not in this build yet");
        assert_eq!(
            report.detail.as_deref(),
            Some("PNG to HEIC is coming. JPG, PNG, WEBP, TIFF and BMP convert now.")
        );
        assert_eq!(report.reveal, None);
    }

    #[test]
    fn one_unsupported_item_stops_the_whole_drop() {
        let to_png = Operation::Convert { to: Format::Png };
        let mixed = [
            SelectionItem::file("/a/one.jpg"),
            SelectionItem::file("/a/two.heic"),
        ];
        let report = not_runnable(&to_png, &mixed).unwrap();
        assert_eq!(
            report.detail.as_deref(),
            Some("HEIC to PNG is coming. JPG, PNG, WEBP, TIFF and BMP convert now.")
        );
    }

    #[test]
    fn tools_wait_for_a_later_build() {
        let tool = Operation::Tool {
            tool: ToolKey::Compress,
            params: ToolParams::default(),
        };
        let report = not_runnable(&tool, &[SelectionItem::file("/a/photo.png")]).unwrap();
        assert_eq!(report.title, "Tools arrive in a later build.");
    }

    #[test]
    fn working_names_one_output_or_counts_many() {
        let one = [job("/a/photo.png", "/a/photo.jpg", false)];
        let report = working(&one, 0, None);
        assert_eq!(report.tone, ToastTone::Working);
        assert_eq!(report.title, "Making photo.jpg…");
        assert_eq!(report.progress, None);
        assert_eq!(working(&one, 0, Some(0.5)).progress, Some(0.5));

        let three = [
            job("/a/1.png", "/a/1.jpg", false),
            job("/a/2.png", "/a/2.jpg", false),
            job("/a/3.png", "/a/3.jpg", false),
        ];
        let report = working(&three, 1, None);
        assert_eq!(report.title, "Making 3 files…");
        assert_eq!(report.progress, Some(1.0 / 3.0));
        assert_eq!(working(&three, 2, Some(1.0)).progress, Some(1.0));
    }

    #[test]
    fn progress_ignores_odd_fractions() {
        let one = [job("/a/photo.png", "/a/photo.jpg", false)];
        assert_eq!(working(&one, 0, Some(f32::NAN)).progress, None);
        assert_eq!(working(&one, 0, Some(7.0)).progress, Some(1.0));
        assert_eq!(working(&one, 0, Some(-1.0)).progress, Some(0.0));
    }

    #[test]
    fn done_names_the_output_and_reveals_it() {
        let jobs = [job("/a/photo.png", "/a/photo.jpg", false)];
        let report = finished(&jobs, &[wrote("/a/photo 2.jpg")], &settings());
        assert_eq!(report.tone, ToastTone::Done);
        assert_eq!(report.title, "Done");
        assert_eq!(report.detail.as_deref(), Some("photo 2.jpg"));
        assert_eq!(report.reveal, Some(PathBuf::from("/a/photo 2.jpg")));

        let two = [
            job("/a/1.png", "/a/1.jpg", false),
            job("/a/2.png", "/a/2.jpg", false),
        ];
        let report = finished(&two, &[wrote("/a/1.jpg"), wrote("/a/2.jpg")], &settings());
        assert_eq!(report.detail.as_deref(), Some("2 files"));
        assert_eq!(report.reveal, Some(PathBuf::from("/a/1.jpg")));
    }

    #[test]
    fn done_says_where_a_fallback_output_went() {
        let jobs = [job(
            "/ro/photo.png",
            "/home/max/Downloads/Grape/photo.jpg",
            true,
        )];
        let report = finished(
            &jobs,
            &[wrote("/home/max/Downloads/Grape/photo.jpg")],
            &settings(),
        );
        let expected = format!(
            "photo.jpg, saved in {} (the folder is read-only)",
            Path::new("Downloads").join("Grape").display()
        );
        assert_eq!(report.detail.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn a_failure_shows_the_engines_words() {
        let jobs = [job("/a/photo.png", "/a/photo.jpg", false)];
        let report = finished(
            &jobs,
            &[failed("photo.png isn't a valid PNG.")],
            &settings(),
        );
        assert_eq!(report.tone, ToastTone::Error);
        assert_eq!(report.title, "photo.png isn't a valid PNG.");
        assert_eq!(report.detail, None);
        assert_eq!(report.reveal, None);
    }

    #[test]
    fn a_partial_batch_counts_and_shows_the_first_error() {
        let jobs = [
            job("/a/1.png", "/a/1.jpg", false),
            job("/a/2.png", "/a/2.jpg", false),
            job("/a/3.png", "/a/3.jpg", false),
        ];
        let results = [
            failed("1.png isn't a valid PNG."),
            wrote("/a/2.jpg"),
            failed("3.png couldn't be read."),
        ];
        let report = finished(&jobs, &results, &settings());
        assert_eq!(report.tone, ToastTone::Error);
        assert_eq!(report.title, "1 of 3 done");
        assert_eq!(report.detail.as_deref(), Some("1.png isn't a valid PNG."));
        assert_eq!(report.reveal, Some(PathBuf::from("/a/2.jpg")));
    }

    #[test]
    fn plan_errors_read_as_sentences() {
        assert_eq!(
            sentence("neither the destination nor the fallback folder is writable"),
            "Neither the destination nor the fallback folder is writable"
        );
        assert_eq!(sentence(""), "");
    }
}
