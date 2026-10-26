//! Runs a drop on a conversion or tool segment
//! (`docs/design/m1-drag-wheel.md` §F.5) and tells the user how it went in
//! the toast.
//!
//! - A drop this build can't run gets an info toast at once, and nothing
//!   is read or written. [`check_runnable`] is the one check, for this
//!   path and for the tool panels' route (`crate::panel`), so wiring an
//!   engine changes only it.
//! - Otherwise, on a thread of its own: build the selection
//!   (`grape_engines::build_selection`, which reads the disk), plan it
//!   (`grape_core::plan`), show "Making …", run each job with
//!   `grape_engines::run_job`, and finish with done, partial or error.
//!   Clicking the finished toast reveals the first output.
//! - Tools arrive in a later build. A tool's panel hands its choices over
//!   through [`submit_op`], with the items in the panel's order.
//! - Where outputs go and the fallback folder come from Settings
//!   ([`crate::settings`]), read when the drop is planned. The sidecars
//!   come from [`crate::sidecars`], found once at startup.
//! - Each drop has one `CancelToken`, shared by its jobs and kept in
//!   [`Running`] from [`submit`] until its thread ends. Quitting Grape
//!   cancels them all ([`cancel_all`], then [`wait_for_all`]), so no job
//!   outlives Grape. A cancelled drop starts no more jobs and shows no
//!   more toasts.
//!
//! The engines never touch a source and never replace an existing file
//! (DECISIONS D16); this module only reads what they report.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use grape_core::{plan, Format, Job, Operation, SelectionItem, Settings, ToolParams, WheelEntry};
use grape_engines::{
    build_selection, run_job, supports, CancelToken, EngineCtx, GrapeError, JobOutcome,
};
use grape_platform::DisplayBounds;
use tauri::{AppHandle, Manager};

use crate::events::{ToastShow, ToastTone};
use crate::toast::{self, Toaster};
use crate::{settings, sidecars};

/// At most this often, a progress update goes to the toast.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// How long quitting waits for cancelled drops to end, so each engine can
/// kill its child process and delete its temp file before Grape exits.
/// Engines check the token at least every 50 ms of work.
const QUIT_WAIT: Duration = Duration::from_secs(2);

/// The drops that are running, each with the cancel token its jobs share.
/// Managed as app state.
#[derive(Debug, Clone, Default)]
pub struct Running(Arc<Registry>);

#[derive(Debug, Default)]
struct Registry {
    drops: Mutex<Drops>,
    /// Notified whenever a drop ends.
    ended: Condvar,
}

#[derive(Debug, Default)]
struct Drops {
    next_id: u64,
    tokens: HashMap<u64, CancelToken>,
    /// Set by [`Running::cancel_all`]: Grape is quitting, so a drop that
    /// starts now starts cancelled.
    quitting: bool,
}

impl Running {
    /// Adds a drop. Its token is already cancelled if Grape is quitting.
    fn start(&self) -> Ticket {
        let mut drops = self.lock();
        let id = drops.next_id;
        drops.next_id = id.wrapping_add(1);
        let cancel = CancelToken::new();
        if drops.quitting {
            cancel.cancel();
        }
        drops.tokens.insert(id, cancel.clone());
        Ticket {
            running: self.clone(),
            id,
            cancel,
        }
    }

    /// Cancels every running drop, and every drop that starts from now on.
    /// Returns how many it cancelled that weren't cancelled already.
    fn cancel_all(&self) -> usize {
        let mut drops = self.lock();
        drops.quitting = true;
        let mut cancelled = 0;
        for token in drops.tokens.values() {
            if !token.is_cancelled() {
                token.cancel();
                cancelled += 1;
            }
        }
        cancelled
    }

    /// How many drops are running.
    fn len(&self) -> usize {
        self.lock().tokens.len()
    }

    /// Waits until no drop is running, or `timeout` has passed. Returns
    /// how many are still running.
    fn wait_until_idle(&self, timeout: Duration) -> usize {
        let drops = self.lock();
        let (drops, _) = self
            .0
            .ended
            .wait_timeout_while(drops, timeout, |drops| !drops.tokens.is_empty())
            .unwrap_or_else(PoisonError::into_inner);
        drops.tokens.len()
    }

    fn lock(&self) -> MutexGuard<'_, Drops> {
        self.0.drops.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One running drop's place in [`Running`]. Dropping it, when the drop's
/// thread ends or couldn't start, takes the drop out.
#[derive(Debug)]
struct Ticket {
    running: Running,
    id: u64,
    cancel: CancelToken,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.running.lock().tokens.remove(&self.id);
        self.running.0.ended.notify_all();
    }
}

/// Cancels every running drop, and any that starts from now on, because
/// Grape is quitting. Returns at once: each engine stops on its own
/// (FFmpeg's child gets killed) and deletes its temp file.
pub fn cancel_all(app: &AppHandle) {
    let Some(running) = app.try_state::<Running>() else {
        return;
    };
    let cancelled = running.cancel_all();
    if cancelled > 0 {
        log::info!("jobs: cancelled {cancelled} running drop(s): Grape is quitting");
    }
}

/// Waits up to [`QUIT_WAIT`] for the cancelled drops to end. For
/// `RunEvent::Exit`, after [`cancel_all`].
pub fn wait_for_all(app: &AppHandle) {
    let Some(running) = app.try_state::<Running>() else {
        return;
    };
    let waiting = running.len();
    if waiting == 0 {
        return;
    }
    let started = Instant::now();
    match running.wait_until_idle(QUIT_WAIT) {
        0 => log::info!(
            "jobs: waited {} ms for {waiting} cancelled drop(s) to end",
            started.elapsed().as_millis()
        ),
        left => log::warn!(
            "jobs: {left} drop(s) still running after {} s; quitting anyway",
            QUIT_WAIT.as_secs()
        ),
    }
}

/// What the toast says. `reveal` is what clicking it shows in Explorer or Finder.
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

/// Why this build can't run a drop, as the info toast says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotRunnable {
    /// The toast's main line.
    pub title: String,
    /// Its second line, if any.
    pub detail: Option<String>,
}

/// Runs a drop on `entry` (a conversion or a tool) for `items`, with the
/// toast near the wheel at `anchor` on `display`. Tools get their default
/// params. Returns at once; the jobs run on their own thread.
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
    submit_op(app, op, items, anchor, display);
}

/// Runs `op` (a tool with its panel's params, or a conversion) on `items`
/// in this order: the panel path, and the rest of [`submit`]. A drop this
/// build can't run gets the info toast at once. Returns at once; the jobs
/// run on their own thread.
pub fn submit_op(
    app: &AppHandle,
    op: Operation,
    items: &[SelectionItem],
    anchor: (i32, i32),
    display: DisplayBounds,
) {
    if let Err(why) = check_runnable(&op, items) {
        log::info!("jobs: {op:?} not in this build: {}", why.title);
        let report = Report::new(ToastTone::Info, why.title, why.detail);
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
    let ticket = app.state::<Running>().start();
    let app = app.clone();
    let spawned = thread::Builder::new()
        .name("grape-jobs".to_owned())
        .spawn(move || {
            // On this thread: it checks the fallback folder on disk.
            let settings = settings::planning_settings(&app);
            let cancel = ticket.cancel.clone();
            run(&app, &op, &paths, &settings, anchor, &display, cancel);
            // The drop has ended. (If the thread can't start, the closure
            // and the ticket with it are dropped at once.)
            drop(ticket);
        });
    if let Err(err) = spawned {
        log::error!("jobs: could not start the job thread: {err}");
    }
}

/// The one runnability check, for the direct path ([`submit`],
/// [`submit_op`]) and the panel route (`panel::route_for`): `Ok` when this
/// build can run `op` on every item, or what the info toast says. Every
/// item must be supported, so a mixed drop never half-runs. Wiring an
/// engine (archives, PDF, lane B's media) changes only this function.
pub fn check_runnable(op: &Operation, items: &[SelectionItem]) -> Result<(), NotRunnable> {
    match op {
        Operation::Tool { .. } => Err(NotRunnable {
            title: "Tools arrive in a later build.".to_owned(),
            detail: None,
        }),
        Operation::Convert { to } => {
            let Some(item) = items.iter().find(|item| !supports(op, item.format())) else {
                return Ok(());
            };
            let from = item.format().map_or("This file", Format::display_name);
            Err(NotRunnable {
                title: "Not in this build yet".to_owned(),
                // One line: the detail row is cut with an ellipsis.
                detail: Some(format!("{from} to {} is coming.", to.display_name())),
            })
        }
    }
}

/// Plans and runs the jobs for one drop, updating the toast as it goes,
/// until `cancel` says Grape is quitting.
fn run(
    app: &AppHandle,
    op: &Operation,
    paths: &[PathBuf],
    settings: &Settings,
    anchor: (i32, i32),
    display: &DisplayBounds,
    cancel: CancelToken,
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
    // The engine creates the fallback folder on first use, and no other.
    // It's an existing folder or Grape's own `Downloads/Grape`
    // (`settings::core_settings`), so a folder the user chose is never
    // created.
    let ctx = EngineCtx {
        sidecars: sidecars::for_jobs(app),
        cancel,
        fallback_dir: Some(settings.fallback_dir.clone()),
    };
    let mut results = Vec::with_capacity(jobs.len());
    let mut last_update: Option<Instant> = None;
    for (done, job) in jobs.iter().enumerate() {
        if ctx.cancel.is_cancelled() {
            break;
        }
        let started = working(&jobs, done, None);
        show(app, id, started, anchor, display);
        let mut progress = |fraction: f32| {
            if ctx.cancel.is_cancelled()
                || last_update.is_some_and(|at| at.elapsed() < PROGRESS_EVERY)
            {
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
            // Counts only: a log never carries a path or a file name.
            Ok(outcome) => log::info!(
                "jobs: {} input(s) -> {} output(s){}",
                job.inputs.len(),
                outcome.outputs.len(),
                if outcome.renamed { " (renamed)" } else { "" }
            ),
            Err(err) => log::warn!("jobs: {err} ({})", err.detail),
        }
        results.push(result);
    }
    // Only quitting cancels a drop, and then nobody's left to tell.
    if ctx.cancel.is_cancelled() {
        log::info!(
            "jobs: drop cancelled with {} of {} job(s) started",
            results.len(),
            jobs.len()
        );
        return;
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
            // Only the jobs that wrote something count. The detail row is
            // one line cut with an ellipsis, so where the files went leads.
            let fell_back = jobs
                .iter()
                .zip(results)
                .filter(|(job, result)| job.used_fallback && result.is_ok())
                .count();
            let folder = short(&settings.fallback_dir);
            match (fell_back, outputs.as_slice()) {
                (0, _) => Report::new(ToastTone::Done, "Done", Some(what)),
                (1, [_]) => Report::new(
                    ToastTone::Done,
                    format!("Saved in {folder}"),
                    Some(format!("{what}; its folder is read-only")),
                ),
                (all, many) if all == many.len() => Report::new(
                    ToastTone::Done,
                    format!("Saved in {folder}"),
                    Some(format!("{what}; their folders are read-only")),
                ),
                (some, _) => Report::new(
                    ToastTone::Done,
                    "Done",
                    Some(format!("{what}; {some} in {folder}")),
                ),
            }
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
        assert_eq!(check_runnable(&to_jpg, &png), Ok(()));

        let to_heic = Operation::Convert { to: Format::Heic };
        assert_eq!(
            check_runnable(&to_heic, &png),
            Err(NotRunnable {
                title: "Not in this build yet".to_owned(),
                detail: Some("PNG to HEIC is coming.".to_owned()),
            })
        );
    }

    #[test]
    fn one_unsupported_item_stops_the_whole_drop() {
        let to_png = Operation::Convert { to: Format::Png };
        let mixed = [
            SelectionItem::file("/a/one.jpg"),
            SelectionItem::file("/a/two.heic"),
            SelectionItem::file("/a/three.avif"),
        ];
        let why = check_runnable(&to_png, &mixed).unwrap_err();
        // The first unsupported item names the toast.
        assert_eq!(why.detail.as_deref(), Some("HEIC to PNG is coming."));
    }

    #[test]
    fn tools_wait_for_a_later_build() {
        for tool in [
            ToolKey::Compress,
            ToolKey::MergePdf,
            ToolKey::RemoveMetadata,
        ] {
            let op = Operation::Tool {
                tool,
                params: ToolParams::default(),
            };
            let why = check_runnable(&op, &[SelectionItem::file("/a/photo.png")]).unwrap_err();
            assert_eq!(why.title, "Tools arrive in a later build.");
            assert_eq!(why.detail, None);
        }
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

    fn grape_folder() -> String {
        Path::new("Downloads").join("Grape").display().to_string()
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
        assert_eq!(report.tone, ToastTone::Done);
        assert_eq!(report.title, format!("Saved in {}", grape_folder()));
        assert_eq!(
            report.detail.as_deref(),
            Some("photo.jpg; its folder is read-only")
        );
        assert_eq!(
            report.reveal,
            Some(PathBuf::from("/home/max/Downloads/Grape/photo.jpg"))
        );

        let two = [
            job("/ro/1.png", "/home/max/Downloads/Grape/1.jpg", true),
            job("/ro/2.png", "/home/max/Downloads/Grape/2.jpg", true),
        ];
        let report = finished(
            &two,
            &[
                wrote("/home/max/Downloads/Grape/1.jpg"),
                wrote("/home/max/Downloads/Grape/2.jpg"),
            ],
            &settings(),
        );
        assert_eq!(report.title, format!("Saved in {}", grape_folder()));
        assert_eq!(
            report.detail.as_deref(),
            Some("2 files; their folders are read-only")
        );
    }

    #[test]
    fn a_batch_where_only_some_fell_back_says_how_many() {
        let jobs = [
            job("/a/1.png", "/a/1.jpg", false),
            job("/ro/2.png", "/home/max/Downloads/Grape/2.jpg", true),
            job("/a/3.png", "/a/3.jpg", false),
        ];
        let results = [
            wrote("/a/1.jpg"),
            wrote("/home/max/Downloads/Grape/2.jpg"),
            wrote("/a/3.jpg"),
        ];
        let report = finished(&jobs, &results, &settings());
        assert_eq!(report.title, "Done");
        let expected = format!("3 files; 1 in {}", grape_folder());
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
    fn a_drop_is_running_from_start_until_its_ticket_drops() {
        let running = Running::default();
        assert_eq!(running.len(), 0);
        let first = running.start();
        let second = running.start();
        assert_ne!(first.id, second.id);
        assert_eq!(running.len(), 2);
        drop(first);
        assert_eq!(running.len(), 1);
        drop(second);
        assert_eq!(running.len(), 0);
        assert_eq!(running.wait_until_idle(Duration::ZERO), 0);
    }

    #[test]
    fn cancel_all_reaches_every_drop_and_the_engines_copies() {
        let running = Running::default();
        let first = running.start();
        let second = running.start();
        // What the engine holds, through `EngineCtx`.
        let engines = first.cancel.clone();
        assert!(!engines.is_cancelled());

        assert_eq!(running.cancel_all(), 2);
        assert!(engines.is_cancelled());
        assert!(first.cancel.is_cancelled() && second.cancel.is_cancelled());
        // Again (tray → Quit, then the exit events): nothing new to cancel.
        assert_eq!(running.cancel_all(), 0);
        // Cancelling doesn't take a drop out; its thread ending does.
        assert_eq!(running.len(), 2);
    }

    #[test]
    fn a_drop_that_starts_while_quitting_starts_cancelled() {
        let running = Running::default();
        assert_eq!(running.cancel_all(), 0);
        let late = running.start();
        assert!(late.cancel.is_cancelled());
        assert_eq!(running.cancel_all(), 0);
    }

    #[test]
    fn a_drop_ending_after_cancel_all_is_fine() {
        let running = Running::default();
        let ticket = running.start();
        let other = running.start();
        running.cancel_all();
        drop(ticket);
        assert_eq!(running.len(), 1);
        drop(other);
        assert_eq!(running.len(), 0);
        assert_eq!(running.cancel_all(), 0);
    }

    #[test]
    fn quitting_waits_for_a_drop_to_notice_and_end() {
        let running = Running::default();
        let ticket = running.start();
        let worker = thread::spawn(move || {
            // An engine checking its token between steps.
            while !ticket.cancel.is_cancelled() {
                thread::sleep(Duration::from_millis(5));
            }
            drop(ticket);
        });
        assert_eq!(running.cancel_all(), 1);
        assert_eq!(running.wait_until_idle(Duration::from_secs(10)), 0);
        worker.join().unwrap();
    }

    #[test]
    fn quitting_stops_waiting_for_a_drop_that_never_ends() {
        let running = Running::default();
        let _stuck = running.start();
        running.cancel_all();
        assert_eq!(running.wait_until_idle(Duration::from_millis(20)), 1);
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
