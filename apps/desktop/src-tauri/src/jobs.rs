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
//! - Tools run like conversions. A tool's panel hands its choices over
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

use grape_core::{
    plan, Format, Job, JobOutput, Operation, SelectionItem, Settings, ToolParams, WheelEntry,
};
use grape_engines::{
    build_selection, run_job, supports_with, CancelToken, EngineCtx, GrapeError, JobOutcome,
    MetaField, Note, SidecarPaths,
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
    if let Err(why) = check_runnable(&op, items, &sidecars::for_jobs(app)) {
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
/// build can run `op` on every item with the `sidecars` Grape found, or
/// what the info toast says. Every item must be supported, so a mixed drop
/// never half-runs. Wiring an engine changes only `grape_engines` and this
/// function's copy.
pub fn check_runnable(
    op: &Operation,
    items: &[SelectionItem],
    sidecars: &SidecarPaths,
) -> Result<(), NotRunnable> {
    runnable_with(op, items, sidecars, &HELPERS)
}

/// [`check_runnable`] with `helpers` as the helpers Grape ships (tests
/// pass their own, so each OS's case runs everywhere).
fn runnable_with(
    op: &Operation,
    items: &[SelectionItem],
    sidecars: &SidecarPaths,
    helpers: &[Helper],
) -> Result<(), NotRunnable> {
    let formats: Vec<Option<Format>> = items.iter().map(SelectionItem::format).collect();
    if supports_with(op, &formats, sidecars) {
        return Ok(());
    }
    if let Some(helper) = missing_helper(op, &formats, sidecars, helpers) {
        return Err(NotRunnable {
            title: helper.title.to_owned(),
            detail: Some(helper.detail.to_owned()),
        });
    }
    let shipped = helpers
        .iter()
        .filter(|helper| helper.ships)
        .fold(sidecars.clone(), |found, helper| (helper.with)(&found));
    // No one helper is enough, but those Grape ships are: several are
    // missing (a PDF and a HEIC without PDFium and grape-heif).
    if supports_with(op, &formats, &shipped) {
        return Err(NotRunnable {
            title: "Some of Grape's helpers are missing".to_owned(),
            detail: Some("Reinstalling Grape brings them back.".to_owned()),
        });
    }
    // One item names the toast: the first that can't run even with every
    // helper Grape ships, so a missing PDFium never blames the PDF in a
    // PDF and PNG drop; else the first that can't run as is.
    let fails =
        |item: &SelectionItem, with: &SidecarPaths| !supports_with(op, &[item.format()], with);
    let first = items
        .iter()
        .find(|item| fails(item, &shipped))
        .or_else(|| items.iter().find(|item| fails(item, sidecars)))
        .and_then(SelectionItem::format);
    let from = first.map_or("this file", Format::display_name);
    let detail = match op {
        Operation::Convert { to } => format!(
            "{} to {} is coming.",
            first.map_or("This file", Format::display_name),
            to.display_name()
        ),
        Operation::Tool { tool, .. } => format!("{} on {from} is coming.", tool.name()),
    };
    Err(NotRunnable {
        title: "Not in this build yet".to_owned(),
        // One line: the detail row is cut with an ellipsis.
        detail: Some(detail),
    })
}

/// A program or library Grape ships whose absence alone stops a drop.
#[derive(Debug, Clone, Copy)]
struct Helper {
    /// The toast's title when it's missing.
    title: &'static str,
    /// The toast's detail when it's missing: one line.
    detail: &'static str,
    /// True once installers and DMGs carry it: only then is a missing one
    /// a broken install. Until then the drop is "not in this build yet".
    ships: bool,
    /// `sidecars` with this helper filled in (any path will do: nothing
    /// runs it).
    with: fn(&SidecarPaths) -> SidecarPaths,
}

/// The helpers the engines need. PDFium ships wherever a build has one
/// (D29). FFmpeg (`ffmpeg` and `ffprobe`, Grape's own build, D24) and
/// bsdtar (D27) ship in every installer and DMG through the per-OS
/// sidecars overlay (D25). The HEIC helper ships on Windows
/// (`grape-heif.exe`, D33); on macOS it's Grape itself, so it's never
/// missing there. Grape ships for no other OS, so there every drop that
/// needs a helper is "coming".
const HELPERS: [Helper; 4] = [
    Helper {
        title: "Grape's PDF library is missing",
        detail: "Reinstalling Grape brings it back.",
        ships: cfg!(any(windows, target_os = "macos")),
        with: with_pdf,
    },
    Helper {
        title: "Grape's RAR helper is missing",
        detail: "Reinstalling Grape brings it back.",
        ships: cfg!(any(windows, target_os = "macos")),
        with: with_bsdtar,
    },
    Helper {
        title: "Grape's HEIC helper is missing",
        detail: "Reinstall Grape to convert HEIC files.",
        ships: cfg!(windows),
        with: with_heif,
    },
    // 221 px in the title's 13 px semibold (Inter), of its 248.
    Helper {
        title: "Grape's media converter is missing",
        detail: "Reinstalling Grape brings it back.",
        ships: cfg!(any(windows, target_os = "macos")),
        with: with_ffmpeg,
    },
];

fn with_pdf(sidecars: &SidecarPaths) -> SidecarPaths {
    SidecarPaths {
        pdfium: Some(PathBuf::from("pdfium")),
        pdf_worker: Some(PathBuf::from("grape")),
        ..sidecars.clone()
    }
}

fn with_bsdtar(sidecars: &SidecarPaths) -> SidecarPaths {
    SidecarPaths {
        bsdtar: Some(PathBuf::from("bsdtar")),
        ..sidecars.clone()
    }
}

fn with_heif(sidecars: &SidecarPaths) -> SidecarPaths {
    SidecarPaths {
        heif: Some(PathBuf::from("grape-heif")),
        ..sidecars.clone()
    }
}

/// Both programs: the media engine needs `ffmpeg` and `ffprobe`, and they
/// ship together, so one missing is the same broken install.
fn with_ffmpeg(sidecars: &SidecarPaths) -> SidecarPaths {
    SidecarPaths {
        ffmpeg: Some(PathBuf::from("ffmpeg")),
        ffprobe: Some(PathBuf::from("ffprobe")),
        ..sidecars.clone()
    }
}

/// The shipped helper among `helpers` whose absence alone stops `op` on
/// `formats`.
fn missing_helper(
    op: &Operation,
    formats: &[Option<Format>],
    sidecars: &SidecarPaths,
    helpers: &[Helper],
) -> Option<Helper> {
    helpers
        .iter()
        .copied()
        .find(|helper| helper.ships && supports_with(op, formats, &(helper.with)(sidecars)))
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
                "jobs: {} input(s) -> {} output(s){}{}",
                job.inputs.len(),
                outcome.outputs.len(),
                if outcome.renamed { " (renamed)" } else { "" },
                log_notes(&outcome.notes)
            ),
            // The kind and the detail without the job's paths: the user
            // message names the file, so it's only for the toast.
            Err(err) => log::warn!("{}", failure_log(err, job)),
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

/// "Making photo.jpg…" or "Making 3 files…" (folders, items), while job
/// `done` (counting from 0) runs and has reported `fraction` of its work.
/// Progress is for the whole drop: unknown for a single job that hasn't
/// reported yet.
fn working(jobs: &[Job], done: usize, fraction: Option<f32>) -> Report {
    let title = match jobs {
        [one] => format!("Making {}…", output_name(&one.output)),
        many => format!(
            "Making {}…",
            several(&many.iter().map(|job| &job.output).collect::<Vec<_>>())
        ),
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
    // What each of those jobs was planned to make: files, folders or both.
    let written: Vec<&JobOutput> = jobs
        .iter()
        .zip(results)
        .filter(|(_, result)| {
            result
                .as_ref()
                .is_ok_and(|outcome| !outcome.outputs.is_empty())
        })
        .map(|(job, _)| &job.output)
        .collect();
    let first_error = results.iter().find_map(|result| result.as_ref().err());
    let reveal = outputs.first().map(|path| path.to_path_buf());
    let mut report = match (outputs.as_slice(), first_error) {
        (_, None) => {
            let what = match outputs.as_slice() {
                [one] => name(one),
                _ => several(&written),
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
                (0, _) => Report::new(
                    ToastTone::Done,
                    noted(results).unwrap_or_else(|| "Done".to_owned()),
                    Some(what),
                ),
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
                // The detail says where some went, so the notes keep the
                // title.
                (some, _) => Report::new(
                    ToastTone::Done,
                    noted(results).unwrap_or_else(|| "Done".to_owned()),
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

/// The done toast's title when the jobs' notes say more than "Done":
/// what was left out, pages drawn below 300 DPI, or what Remove Metadata
/// removed. macOS metadata left out of an archive isn't mentioned (it's
/// never the user's content); the log counts it.
fn noted(results: &[Result<JobOutcome, GrapeError>]) -> Option<String> {
    let notes: Vec<&Note> = results
        .iter()
        .filter_map(|result| result.as_ref().ok())
        .flat_map(|outcome| &outcome.notes)
        .collect();
    let (mut links, mut specials, mut copies, mut scaled) = (0u32, 0u32, 0u32, 0u32);
    let mut removed: Option<Vec<MetaField>> = None;
    for note in &notes {
        match note {
            Note::LeftOut(left) => {
                links = links.saturating_add(left.links);
                specials = specials.saturating_add(left.specials);
                copies = copies.saturating_add(left.copies);
            }
            Note::PagesScaled(pages) => scaled = scaled.saturating_add(*pages),
            Note::MetadataRemoved(fields) => removed.get_or_insert_with(Vec::new).extend(fields),
            _ => {}
        }
    }
    let left_out = match (links, specials, copies) {
        (0, 0, 0) => None,
        (n, 0, 0) => Some(count(n, "link", "links")),
        (0, n, 0) => Some(count(n, "special file", "special files")),
        (0, 0, n) => Some(count(n, "repeated file", "repeated files")),
        _ => Some(count(
            links.saturating_add(specials).saturating_add(copies),
            "item",
            "items",
        )),
    };
    if let Some(what) = left_out {
        return Some(format!("{what} left out"));
    }
    if scaled > 0 {
        return Some(format!(
            "{} drawn below 300 DPI",
            count(scaled, "page", "pages")
        ));
    }
    removed.map(|fields| removed_title(&fields))
}

/// "Removed author and title", "Removed author, title and 3 more",
/// "Removed keywords and 4 more" when two names would make the title too
/// wide, or "No document metadata found". PDFs keep page-level XMP and
/// their images' metadata (ENGINES.md → Metadata removal), so it never
/// says the file has none.
fn removed_title(fields: &[MetaField]) -> String {
    let mut names: Vec<&str> = Vec::new();
    let mut sorted = fields.to_vec();
    sorted.sort();
    for field in sorted {
        let name = match field {
            MetaField::Author => "author",
            MetaField::Title => "title",
            MetaField::Subject => "subject",
            MetaField::Keywords => "keywords",
            MetaField::Creator => "creator",
            MetaField::Producer => "producer",
            MetaField::CreationDate | MetaField::ModDate => "dates",
            MetaField::Xmp => "XMP",
            _ => "other fields",
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let title = match names.as_slice() {
        [] => return "No document metadata found".to_owned(),
        [one] => return format!("Removed {one}"),
        [one, two] => format!("Removed {one} and {two}"),
        [one, two, rest @ ..] => format!("Removed {one}, {two} and {} more", rest.len()),
    };
    if title.chars().count() <= REMOVED_MAX {
        title
    } else {
        format!("Removed {} and {} more", names[0], names.len() - 1)
    }
}

/// The longest "Removed …" title with two names: 33 characters. The
/// widest of those is about 231 px in the title's 13 px semibold, measured
/// with Inter (the system fonts differ a little), and the toast's title
/// has 248 px.
const REMOVED_MAX: usize = 33;

/// "1 link", "2 links".
fn count(n: u32, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// The log line for a job that failed: its kind and the engine's detail,
/// with the job's paths, their folders and their file names taken out (an
/// engine's detail may hold the paths involved). Never the user message,
/// which names the file.
fn failure_log(err: &GrapeError, job: &Job) -> String {
    format!("jobs: {:?} ({})", err.kind, without_paths(&err.detail, job))
}

/// `detail` with every input and output path of `job` replaced by
/// `<path>`, their folders by `<folder>` and their file names by `<name>`.
/// Words under 3 characters are left (a bare root, a one-letter name).
fn without_paths(detail: &str, job: &Job) -> String {
    let mut paths: Vec<&Path> = job.inputs.iter().map(PathBuf::as_path).collect();
    match &job.output {
        JobOutput::File(path) | JobOutput::Folder(path) => paths.push(path),
        JobOutput::PagesFileOrFolder { file, folder, .. } => {
            paths.push(file);
            paths.push(folder);
        }
    }
    let mut words: Vec<(String, &str)> = Vec::new();
    for path in paths {
        words.push((path.to_string_lossy().into_owned(), "<path>"));
        if let Some(parent) = path.parent() {
            words.push((parent.to_string_lossy().into_owned(), "<folder>"));
        }
        if let Some(name) = path.file_name() {
            words.push((name.to_string_lossy().into_owned(), "<name>"));
        }
    }
    // Longest first, so a path goes before its folder and its name.
    words.sort_by_key(|word| std::cmp::Reverse(word.0.len()));
    let mut out = detail.to_owned();
    for (word, with) in &words {
        if word.chars().count() >= 3 {
            out = out.replace(word.as_str(), with);
        }
    }
    out
}

/// The notes for the log: counts and kinds, never names.
fn log_notes(notes: &[Note]) -> String {
    notes
        .iter()
        .map(|note| match note {
            Note::LeftOut(left) => format!(
                "; left out {} link(s), {} special, {} macOS metadata, {} copies",
                left.links, left.specials, left.mac_metadata, left.copies
            ),
            Note::PagesScaled(pages) => format!("; {pages} page(s) below 300 DPI"),
            Note::MetadataRemoved(fields) => format!("; removed {fields:?}"),
            other => format!("; {other:?}"),
        })
        .collect()
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

/// What a job makes, as the toast names it before it runs: the file or the
/// folder. A PDF → image export makes a file for one page and a folder for
/// more, which isn't known yet, so it's the source's stem ("doc").
fn output_name(output: &JobOutput) -> String {
    match output {
        JobOutput::File(path) | JobOutput::Folder(path) => name(path),
        JobOutput::PagesFileOrFolder { page_stem, .. } => page_stem.clone(),
    }
}

/// "3 files", "2 folders", or "3 items" when they're mixed or include a
/// page export, which may be either.
fn several(outputs: &[&JobOutput]) -> String {
    let n = outputs.len();
    if outputs
        .iter()
        .all(|output| matches!(output, JobOutput::File(_)))
    {
        format!("{n} files")
    } else if outputs
        .iter()
        .all(|output| matches!(output, JobOutput::Folder(_)))
    {
        format!("{n} folders")
    } else {
        format!("{n} items")
    }
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
            notes: Vec::new(),
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

    /// What Grape finds in a full install on every OS: FFmpeg and ffprobe,
    /// PDFium, the worker and bsdtar. (The HEIC helper is
    /// [`with_heic_helper`].)
    fn installed() -> SidecarPaths {
        SidecarPaths {
            ffmpeg: Some(PathBuf::from("ffmpeg.exe")),
            ffprobe: Some(PathBuf::from("ffprobe.exe")),
            pdfium: Some(PathBuf::from("pdfium.dll")),
            pdf_worker: Some(PathBuf::from("Grape.exe")),
            bsdtar: Some(PathBuf::from("bsdtar.exe")),
            ..SidecarPaths::default()
        }
    }

    fn none() -> SidecarPaths {
        SidecarPaths::default()
    }

    #[test]
    fn raster_pairs_run_and_the_rest_say_they_are_coming() {
        let to_jpg = Operation::Convert { to: Format::Jpg };
        let png = [SelectionItem::file("/a/photo.png")];
        assert_eq!(check_runnable(&to_jpg, &png, &none()), Ok(()));

        let to_avif = Operation::Convert { to: Format::Avif };
        assert_eq!(
            check_runnable(&to_avif, &png, &installed()),
            Err(NotRunnable {
                title: "Not in this build yet".to_owned(),
                detail: Some("PNG to AVIF is coming.".to_owned()),
            })
        );
    }

    #[test]
    fn one_unsupported_item_stops_the_whole_drop() {
        let to_png = Operation::Convert { to: Format::Png };
        let mixed = [
            SelectionItem::file("/a/one.jpg"),
            SelectionItem::file("/a/two.avif"),
            SelectionItem::file("/a/three.heic"),
        ];
        let why = check_runnable(&to_png, &mixed, &installed()).unwrap_err();
        // The first unsupported item names the toast.
        assert_eq!(why.detail.as_deref(), Some("AVIF to PNG is coming."));
    }

    /// [`installed`] plus the HEIC helper: `grape-heif.exe` on Windows,
    /// Grape itself on macOS.
    fn with_heic_helper() -> SidecarPaths {
        SidecarPaths {
            heif: Some(PathBuf::from("grape-heif.exe")),
            ..installed()
        }
    }

    #[test]
    fn heic_runs_with_its_helper_both_ways() {
        let heic = [SelectionItem::file("/a/IMG_0001.HEIC")];
        let png = [SelectionItem::file("/a/photo.png")];
        for to in [
            Format::Jpg,
            Format::Png,
            Format::Webp,
            Format::Tiff,
            Format::Bmp,
            Format::Pdf,
        ] {
            let op = Operation::Convert { to };
            assert_eq!(
                check_runnable(&op, &heic, &with_heic_helper()),
                Ok(()),
                "{to:?}"
            );
        }
        for from in ["/a/photo.png", "/a/photo.jpg", "/a/scan.tiff"] {
            let op = Operation::Convert { to: Format::Heic };
            let items = [SelectionItem::file(from)];
            assert_eq!(
                check_runnable(&op, &items, &with_heic_helper()),
                Ok(()),
                "{from}"
            );
        }
        // HEIC ⇄ AVIF waits for an AVIF engine.
        let to_avif = Operation::Convert { to: Format::Avif };
        assert_eq!(
            check_runnable(&to_avif, &heic, &with_heic_helper())
                .unwrap_err()
                .detail
                .as_deref(),
            Some("HEIC to AVIF is coming.")
        );
        let to_heic = Operation::Convert { to: Format::Heic };
        assert_eq!(check_runnable(&to_heic, &png, &with_heic_helper()), Ok(()));
    }

    #[test]
    fn heic_without_its_helper_says_so_on_windows_and_is_coming_elsewhere() {
        let to_png = Operation::Convert { to: Format::Png };
        let to_heic = Operation::Convert { to: Format::Heic };
        let heic = [SelectionItem::file("/a/IMG_0001.HEIC")];
        let png = [SelectionItem::file("/a/photo.png")];
        for (op, items, coming) in [
            (&to_png, &heic, "HEIC to PNG is coming."),
            (&to_heic, &png, "PNG to HEIC is coming."),
        ] {
            let why = check_runnable(op, items, &installed()).unwrap_err();
            if cfg!(windows) {
                assert_eq!(
                    why,
                    NotRunnable {
                        title: "Grape's HEIC helper is missing".to_owned(),
                        detail: Some("Reinstall Grape to convert HEIC files.".to_owned()),
                    }
                );
            } else {
                // macOS always has its helper (Grape itself); Linux none.
                assert_eq!(
                    why,
                    NotRunnable {
                        title: "Not in this build yet".to_owned(),
                        detail: Some(coming.to_owned()),
                    }
                );
            }
        }
    }

    #[test]
    fn a_heic_and_an_avif_never_blame_the_heic_helper() {
        let to_png = Operation::Convert { to: Format::Png };
        let mixed = [
            SelectionItem::file("/a/one.heic"),
            SelectionItem::file("/a/two.avif"),
        ];
        // Without the helper: where it ships, even with it the AVIF can't
        // run, so the AVIF names the toast; elsewhere the HEIC does.
        let why = check_runnable(&to_png, &mixed, &installed()).unwrap_err();
        assert_eq!(why.title, "Not in this build yet");
        let named = if cfg!(windows) {
            "AVIF to PNG is coming."
        } else {
            "HEIC to PNG is coming."
        };
        assert_eq!(why.detail.as_deref(), Some(named));
        // With it, only the AVIF can't run.
        assert_eq!(
            check_runnable(&to_png, &mixed, &with_heic_helper()),
            Err(NotRunnable {
                title: "Not in this build yet".to_owned(),
                detail: Some("AVIF to PNG is coming.".to_owned()),
            })
        );
    }

    #[test]
    fn several_missing_helpers_say_so_and_never_blame_a_pdf_as_coming() {
        let shipped = [
            Helper {
                ships: true,
                ..HELPERS[0]
            },
            Helper {
                ships: true,
                ..HELPERS[2]
            },
        ];
        let to_png = Operation::Convert { to: Format::Png };
        let both = [
            SelectionItem::file("/a/a.pdf"),
            SelectionItem::file("/a/b.heic"),
        ];
        let several = Err(NotRunnable {
            title: "Some of Grape's helpers are missing".to_owned(),
            detail: Some("Reinstalling Grape brings them back.".to_owned()),
        });
        assert_eq!(runnable_with(&to_png, &both, &none(), &shipped), several);
        // One missing: that one is named.
        let heif_only = SidecarPaths {
            heif: Some(PathBuf::from("grape-heif.exe")),
            ..none()
        };
        assert_eq!(
            runnable_with(&to_png, &both, &heif_only, &shipped)
                .unwrap_err()
                .title,
            "Grape's PDF library is missing"
        );
        assert_eq!(
            runnable_with(&to_png, &both, &with_heic_helper(), &shipped),
            Ok(())
        );
        // Something that can't run even with both is what the toast names.
        let three = [
            SelectionItem::file("/a/a.pdf"),
            SelectionItem::file("/a/b.heic"),
            SelectionItem::file("/a/c.avif"),
        ];
        assert_eq!(
            runnable_with(&to_png, &three, &none(), &shipped)
                .unwrap_err()
                .detail
                .as_deref(),
            Some("AVIF to PNG is coming.")
        );
        // The real list on Windows, where both ship.
        if cfg!(windows) {
            assert_eq!(check_runnable(&to_png, &both, &none()), several);
        }
    }

    #[test]
    fn a_failure_is_logged_without_the_jobs_names_or_paths() {
        let job = job(
            "/home/max/Taxes/Divorce papers scan.heic",
            "/home/max/Taxes/Divorce papers scan.jpg",
            false,
        );
        let err = GrapeError {
            kind: ErrorKind::DecodeFailed,
            user_message: "Divorce papers scan.heic isn't a readable HEIC image.".to_owned(),
            detail: "decode /home/max/Taxes/Divorce papers scan.heic: bad; create temp in /home/max/Taxes: denied; Divorce papers scan.jpg".to_owned(),
        };
        let line = failure_log(&err, &job);
        assert!(
            !line.contains("Divorce") && !line.contains("/home/max"),
            "{line}"
        );
        assert_eq!(
            line,
            "jobs: DecodeFailed (decode <path>: bad; create temp in <folder>: denied; <name>)"
        );
        // A detail without the job's paths (the HEIC helper's) stays as is.
        let helper = GrapeError {
            detail: "HEIC helper: Damaged: ImageIO's image status is 3".to_owned(),
            ..err
        };
        assert_eq!(
            failure_log(&helper, &job),
            "jobs: DecodeFailed (HEIC helper: Damaged: ImageIO's image status is 3)"
        );
    }

    #[test]
    fn archives_run_and_rar_needs_bsdtar() {
        for (name, to) in [
            ("/a/x.zip", Format::Tar),
            ("/a/x.tar", Format::Zip),
            ("/a/x.tar.gz", Format::Zip),
        ] {
            let op = Operation::Convert { to };
            assert_eq!(
                check_runnable(&op, &[SelectionItem::file(name)], &none()),
                Ok(())
            );
        }
        let rar = [SelectionItem::file("/a/x.rar")];
        let to_zip = Operation::Convert { to: Format::Zip };
        assert_eq!(check_runnable(&to_zip, &rar, &installed()), Ok(()));
        let extract = Operation::tool(ToolKey::ExtractArchive);
        let both = [
            SelectionItem::file("/a/x.zip"),
            SelectionItem::file("/a/y.rar"),
        ];
        assert_eq!(check_runnable(&extract, &both, &installed()), Ok(()));
        // Every installer and DMG carries bsdtar (D27), so without it a RAR
        // drop is a broken install there. Grape ships for no other OS, so
        // there RAR is "coming".
        let missing = NotRunnable {
            title: "Grape's RAR helper is missing".to_owned(),
            detail: Some("Reinstalling Grape brings it back.".to_owned()),
        };
        for (op, items, coming) in [
            (&to_zip, &rar[..], "RAR to ZIP is coming."),
            (&extract, &both[..], "Extract Archive on RAR is coming."),
        ] {
            let why = check_runnable(op, items, &none()).unwrap_err();
            if cfg!(any(windows, target_os = "macos")) {
                assert_eq!(why, missing, "{op:?}");
            } else {
                assert_eq!(
                    why,
                    NotRunnable {
                        title: "Not in this build yet".to_owned(),
                        detail: Some(coming.to_owned()),
                    }
                );
            }
        }
        // A build where bsdtar doesn't ship says "coming" on every OS.
        let unshipped = [Helper {
            ships: false,
            ..HELPERS[1]
        }];
        assert_eq!(
            runnable_with(&extract, &both, &none(), &unshipped),
            Err(NotRunnable {
                title: "Not in this build yet".to_owned(),
                detail: Some("Extract Archive on RAR is coming.".to_owned()),
            })
        );
        // With the RAR helper shipped, a missing one is named on every OS.
        let shipped = [Helper {
            ships: true,
            ..HELPERS[1]
        }];
        assert_eq!(
            runnable_with(&to_zip, &rar, &none(), &shipped),
            Err(missing)
        );
    }

    #[test]
    fn mp4_to_mov_runs_with_ffmpeg_and_says_when_it_is_missing() {
        let to_mov = Operation::Convert { to: Format::Mov };
        let mp4 = [SelectionItem::file("/a/clip.mp4")];
        assert_eq!(check_runnable(&to_mov, &mp4, &installed()), Ok(()));
        let missing = NotRunnable {
            title: "Grape's media converter is missing".to_owned(),
            detail: Some("Reinstalling Grape brings it back.".to_owned()),
        };
        let coming = NotRunnable {
            title: "Not in this build yet".to_owned(),
            detail: Some("MP4 to MOV is coming.".to_owned()),
        };
        // Either program missing is a broken install: the engine needs both.
        let no_ffmpeg = SidecarPaths {
            ffmpeg: None,
            ..installed()
        };
        let no_ffprobe = SidecarPaths {
            ffprobe: None,
            ..installed()
        };
        for sidecars in [none(), no_ffmpeg, no_ffprobe] {
            let why = check_runnable(&to_mov, &mp4, &sidecars).unwrap_err();
            // Every installer and DMG carries FFmpeg (D24, D25); Grape
            // ships for no other OS.
            if cfg!(any(windows, target_os = "macos")) {
                assert_eq!(why, missing, "{sidecars:?}");
            } else {
                assert_eq!(why, coming, "{sidecars:?}");
            }
        }
        // Each OS's case, on every OS: FFmpeg shipped, and not.
        let ffmpeg = HELPERS[3];
        assert_eq!(ffmpeg.title, "Grape's media converter is missing");
        let shipped = [Helper {
            ships: true,
            ..ffmpeg
        }];
        let unshipped = [Helper {
            ships: false,
            ..ffmpeg
        }];
        assert_eq!(
            runnable_with(&to_mov, &mp4, &none(), &shipped),
            Err(missing)
        );
        assert_eq!(
            runnable_with(&to_mov, &mp4, &none(), &unshipped),
            Err(coming)
        );
        assert_eq!(
            runnable_with(&to_mov, &mp4, &installed(), &unshipped),
            Ok(())
        );
    }

    #[test]
    fn pdf_jobs_run_with_pdfium_and_say_when_it_is_missing() {
        let pdfs = [
            SelectionItem::file("/a/one.pdf"),
            SelectionItem::file("/a/two.pdf"),
        ];
        let ops = [
            Operation::Convert { to: Format::Png },
            Operation::Convert { to: Format::Md },
            Operation::tool(ToolKey::SplitPdf),
            Operation::tool(ToolKey::MergePdf),
            Operation::tool(ToolKey::RemoveMetadata),
        ];
        for op in ops {
            assert_eq!(check_runnable(&op, &pdfs, &installed()), Ok(()), "{op:?}");
            let why = check_runnable(&op, &pdfs, &none()).unwrap_err();
            if cfg!(any(windows, target_os = "macos")) {
                assert_eq!(why.title, "Grape's PDF library is missing", "{op:?}");
                assert_eq!(
                    why.detail.as_deref(),
                    Some("Reinstalling Grape brings it back.")
                );
            } else {
                assert_eq!(why.title, "Not in this build yet", "{op:?}");
            }
        }
        // A PDF and a TIFF without PDFium: where PDFium ships, the TIFF is
        // what this build can't run, so it names the toast. Elsewhere no
        // PDF runs, so the PDF does. (TIFF, not PNG or JPG: lane B's tools
        // (D26) run Remove Metadata on those.)
        let op = Operation::tool(ToolKey::RemoveMetadata);
        let mixed = [
            SelectionItem::file("/a/a.pdf"),
            SelectionItem::file("/a/b.tiff"),
        ];
        let why = check_runnable(&op, &mixed, &none()).unwrap_err();
        assert_eq!(why.title, "Not in this build yet");
        let named = if cfg!(any(windows, target_os = "macos")) {
            "Remove Metadata on TIFF is coming."
        } else {
            "Remove Metadata on PDF is coming."
        };
        assert_eq!(why.detail.as_deref(), Some(named));
        // Compress on a PDF is v2: coming, whatever is installed.
        let compress = Operation::tool(ToolKey::Compress);
        assert_eq!(
            check_runnable(&compress, &pdfs, &none()),
            Err(NotRunnable {
                title: "Not in this build yet".to_owned(),
                detail: Some("Compress on PDF is coming.".to_owned()),
            })
        );
    }

    #[test]
    fn images_to_pdf_run_in_grapes_own_process() {
        // Grape writes the PDF itself (D37): no PDFium, no worker, no
        // helper for the five raster formats.
        let to_pdf = Operation::Convert { to: Format::Pdf };
        for from in [
            "/a/photo.jpg",
            "/a/photo.png",
            "/a/photo.webp",
            "/a/scan.tiff",
            "/a/scan.bmp",
        ] {
            let items = [SelectionItem::file(from)];
            assert_eq!(check_runnable(&to_pdf, &items, &none()), Ok(()), "{from}");
            assert_eq!(
                check_runnable(&to_pdf, &items, &installed()),
                Ok(()),
                "{from}"
            );
        }
        // SVG and AVIF wait for their decoders, whatever is installed.
        for (from, coming) in [
            ("/a/logo.svg", "SVG to PDF is coming."),
            ("/a/photo.avif", "AVIF to PDF is coming."),
        ] {
            assert_eq!(
                check_runnable(&to_pdf, &[SelectionItem::file(from)], &with_heic_helper()),
                Err(NotRunnable {
                    title: "Not in this build yet".to_owned(),
                    detail: Some(coming.to_owned()),
                })
            );
        }
    }

    #[test]
    fn create_pdf_runs_on_the_images_the_wheel_offers_it_for() {
        let create = Operation::tool(ToolKey::CreatePdf);
        let jpgs = [
            SelectionItem::file("/a/1.jpg"),
            SelectionItem::file("/a/2.jpg"),
            SelectionItem::file("/a/3.jpg"),
        ];
        assert_eq!(check_runnable(&create, &jpgs, &none()), Ok(()));
        // JPG, PNG, WEBP and TIFF (FORMAT_MATRIX gives BMP no tools, so
        // the wheel never offers Create PDF with a BMP selected).
        let mixed = [
            SelectionItem::file("/a/1.jpg"),
            SelectionItem::file("/a/2.png"),
            SelectionItem::file("/a/3.webp"),
            SelectionItem::file("/a/4.tiff"),
        ];
        assert_eq!(check_runnable(&create, &mixed, &installed()), Ok(()));
        // One input it can't read yet stops the whole PDF, and names it.
        let with_svg = [
            SelectionItem::file("/a/1.jpg"),
            SelectionItem::file("/a/2.svg"),
            SelectionItem::file("/a/3.png"),
        ];
        assert_eq!(
            check_runnable(&create, &with_svg, &with_heic_helper()),
            Err(NotRunnable {
                title: "Not in this build yet".to_owned(),
                detail: Some("Create PDF on SVG is coming.".to_owned()),
            })
        );
    }

    #[test]
    fn a_heic_in_a_pdf_needs_the_heic_helper() {
        let to_pdf = Operation::Convert { to: Format::Pdf };
        let create = Operation::tool(ToolKey::CreatePdf);
        let heic = [SelectionItem::file("/a/IMG_0001.HEIC")];
        let mixed = [
            SelectionItem::file("/a/photo.jpg"),
            SelectionItem::file("/a/IMG_0001.HEIC"),
        ];
        assert_eq!(check_runnable(&to_pdf, &heic, &with_heic_helper()), Ok(()));
        assert_eq!(check_runnable(&create, &mixed, &with_heic_helper()), Ok(()));
        // The HEIC helper alone does it: PDFium plays no part.
        let heif_only = SidecarPaths {
            heif: Some(PathBuf::from("grape-heif.exe")),
            ..none()
        };
        assert_eq!(check_runnable(&create, &mixed, &heif_only), Ok(()));

        let missing = NotRunnable {
            title: "Grape's HEIC helper is missing".to_owned(),
            detail: Some("Reinstall Grape to convert HEIC files.".to_owned()),
        };
        let coming = |detail: &str| NotRunnable {
            title: "Not in this build yet".to_owned(),
            detail: Some(detail.to_owned()),
        };
        let cases = [
            (&to_pdf, &heic[..], "HEIC to PDF is coming."),
            (&create, &mixed[..], "Create PDF on HEIC is coming."),
        ];
        for (op, items, detail) in cases {
            // Without PDFium too: it's never the reason.
            for sidecars in [installed(), none()] {
                let why = check_runnable(op, items, &sidecars).unwrap_err();
                if cfg!(windows) {
                    assert_eq!(why, missing, "{op:?}");
                } else {
                    // macOS always has its helper (Grape itself); Linux none.
                    assert_eq!(why, coming(detail), "{op:?}");
                }
            }
            // Each OS's case, on every OS: the helper shipped, and not.
            let shipped = [Helper {
                ships: true,
                ..HELPERS[2]
            }];
            let unshipped = [Helper {
                ships: false,
                ..HELPERS[2]
            }];
            assert_eq!(
                runnable_with(op, items, &none(), &shipped),
                Err(missing.clone()),
                "{op:?}"
            );
            assert_eq!(
                runnable_with(op, items, &none(), &unshipped),
                Err(coming(detail)),
                "{op:?}"
            );
        }
        // A HEIC and an SVG: even with the helper the SVG can't go in, so
        // the SVG names the toast and the helper is never blamed.
        let with_svg = [
            SelectionItem::file("/a/IMG_0001.HEIC"),
            SelectionItem::file("/a/logo.svg"),
        ];
        let shipped = [Helper {
            ships: true,
            ..HELPERS[2]
        }];
        assert_eq!(
            runnable_with(&create, &with_svg, &none(), &shipped),
            Err(coming("Create PDF on SVG is coming."))
        );
    }

    #[test]
    fn create_pdf_names_its_one_output() {
        let create = Job {
            inputs: vec![
                PathBuf::from("/a/a.jpg"),
                PathBuf::from("/a/b.png"),
                PathBuf::from("/a/c.heic"),
            ],
            output: JobOutput::File(PathBuf::from("/a/a Combined.pdf")),
            op: Operation::tool(ToolKey::CreatePdf),
            used_fallback: false,
        };
        let jobs = [create];
        assert_eq!(working(&jobs, 0, None).title, "Making a Combined.pdf…");
        let report = finished(&jobs, &[wrote("/a/a Combined.pdf")], &settings());
        assert_eq!(report.tone, ToastTone::Done);
        assert_eq!(report.title, "Done");
        assert_eq!(report.detail.as_deref(), Some("a Combined.pdf"));
        assert_eq!(report.reveal, Some(PathBuf::from("/a/a Combined.pdf")));
        // A file it can't read stops the whole PDF: the toast has the
        // engine's words, which name that file; the log names none.
        let report = finished(
            &jobs,
            &[failed("b.png isn't a readable image.")],
            &settings(),
        );
        assert_eq!(report.tone, ToastTone::Error);
        assert_eq!(report.title, "b.png isn't a readable image.");
        assert_eq!(report.reveal, None);
        let err = GrapeError {
            kind: ErrorKind::DecodeFailed,
            user_message: "b.png isn't a readable image.".to_owned(),
            detail: "decode /a/b.png: bad".to_owned(),
        };
        assert_eq!(
            failure_log(&err, &jobs[0]),
            "jobs: DecodeFailed (decode <path>: bad)"
        );
    }

    #[test]
    fn other_tools_say_they_are_coming() {
        // TIFF stays "coming" for both tools once lane B's tools (D26) run
        // them on JPG, PNG and media.
        for tool in [ToolKey::Compress, ToolKey::RemoveMetadata] {
            let op = Operation::Tool {
                tool,
                params: ToolParams::default(),
            };
            let why = check_runnable(&op, &[SelectionItem::file("/a/photo.tiff")], &installed())
                .unwrap_err();
            assert_eq!(why.title, "Not in this build yet");
            assert_eq!(
                why.detail,
                Some(format!("{} on TIFF is coming.", tool.name()))
            );
        }
        // Remove Metadata on a PDF and a TIFF: the TIFF names the toast.
        let op = Operation::tool(ToolKey::RemoveMetadata);
        let mixed = [
            SelectionItem::file("/a/a.pdf"),
            SelectionItem::file("/a/b.tiff"),
        ];
        assert_eq!(
            check_runnable(&op, &mixed, &installed())
                .unwrap_err()
                .detail
                .as_deref(),
            Some("Remove Metadata on TIFF is coming.")
        );
    }

    fn noting(path: &str, notes: Vec<Note>) -> Result<JobOutcome, GrapeError> {
        Ok(JobOutcome {
            outputs: vec![PathBuf::from(path)],
            renamed: false,
            notes,
        })
    }

    fn left_out(links: u32, specials: u32, mac_metadata: u32, copies: u32) -> Note {
        Note::LeftOut(grape_engines::LeftOut {
            links,
            specials,
            mac_metadata,
            copies,
        })
    }

    #[test]
    fn done_says_what_an_archive_left_out() {
        let jobs = [job("/a/x.tar", "/a/x Extracted", false)];
        let cases = [
            (left_out(1, 0, 0, 0), "1 link left out"),
            (left_out(2, 0, 7, 0), "2 links left out"),
            (left_out(0, 1, 0, 0), "1 special file left out"),
            (left_out(0, 0, 0, 3), "3 repeated files left out"),
            (left_out(1, 1, 0, 1), "3 items left out"),
        ];
        for (note, title) in cases {
            let report = finished(&jobs, &[noting("/a/x Extracted", vec![note])], &settings());
            assert_eq!(report.tone, ToastTone::Done);
            assert_eq!(report.title, title);
            assert_eq!(report.detail.as_deref(), Some("x Extracted"));
        }
        // macOS metadata alone isn't mentioned.
        let only_mac = [noting("/a/x Extracted", vec![left_out(0, 0, 9, 0)])];
        assert_eq!(finished(&jobs, &only_mac, &settings()).title, "Done");
        // Two jobs add up.
        let two = [
            job("/a/1.zip", "/a/1.tar", false),
            job("/a/2.zip", "/a/2.tar", false),
        ];
        let results = [
            noting("/a/1.tar", vec![left_out(1, 0, 0, 0)]),
            noting("/a/2.tar", vec![left_out(2, 0, 0, 0)]),
        ];
        let report = finished(&two, &results, &settings());
        assert_eq!(report.title, "3 links left out");
        assert_eq!(report.detail.as_deref(), Some("2 files"));
    }

    #[test]
    fn done_says_pages_were_drawn_below_300_dpi() {
        let jobs = [job("/a/doc.pdf", "/a/doc.png", false)];
        let report = finished(
            &jobs,
            &[noting("/a/doc", vec![Note::PagesScaled(2)])],
            &settings(),
        );
        assert_eq!(report.title, "2 pages drawn below 300 DPI");
        assert_eq!(report.detail.as_deref(), Some("doc"));
        let one = [noting("/a/doc.png", vec![Note::PagesScaled(1)])];
        assert_eq!(
            finished(&jobs, &one, &settings()).title,
            "1 page drawn below 300 DPI"
        );
    }

    #[test]
    fn done_says_what_remove_metadata_removed() {
        let jobs = [job("/a/doc.pdf", "/a/doc No Metadata.pdf", false)];
        let cases: [(Vec<MetaField>, &str); 5] = [
            (vec![], "No document metadata found"),
            (vec![MetaField::Title], "Removed title"),
            (
                vec![MetaField::Title, MetaField::Author],
                "Removed author and title",
            ),
            (
                vec![
                    MetaField::CreationDate,
                    MetaField::ModDate,
                    MetaField::Producer,
                ],
                "Removed producer and dates",
            ),
            (
                vec![
                    MetaField::Author,
                    MetaField::Title,
                    MetaField::Producer,
                    MetaField::ModDate,
                    MetaField::Xmp,
                ],
                "Removed author, title and 3 more",
            ),
        ];
        for (fields, title) in cases {
            let results = [noting(
                "/a/doc No Metadata.pdf",
                vec![Note::MetadataRemoved(fields)],
            )];
            let report = finished(&jobs, &results, &settings());
            assert_eq!(report.title, title);
            assert_eq!(report.detail.as_deref(), Some("doc No Metadata.pdf"));
        }
    }

    #[test]
    fn remove_metadata_titles_fit_the_toast() {
        // Two names would make 37 characters, wider than the title.
        let fields = [
            MetaField::Keywords,
            MetaField::Producer,
            MetaField::CreationDate,
            MetaField::Xmp,
            MetaField::Other,
        ];
        assert_eq!(removed_title(&fields), "Removed keywords and 4 more");
        assert_eq!(
            removed_title(&[MetaField::Subject, MetaField::Keywords, MetaField::Creator]),
            "Removed subject and 2 more"
        );
        // These fit with two names.
        assert_eq!(
            removed_title(&[MetaField::Author, MetaField::Title, MetaField::Xmp]),
            "Removed author, title and 1 more"
        );
        // Every set of fields: at most 33 characters, about 231 px in the
        // title's font (the title has 248 px).
        let all = [
            MetaField::Author,
            MetaField::Title,
            MetaField::Subject,
            MetaField::Keywords,
            MetaField::Creator,
            MetaField::Producer,
            MetaField::CreationDate,
            MetaField::ModDate,
            MetaField::Xmp,
            MetaField::Other,
        ];
        for mask in 0u32..(1 << all.len()) {
            let fields: Vec<MetaField> = all
                .iter()
                .enumerate()
                .filter(|(bit, _)| mask & (1 << bit) != 0)
                .map(|(_, field)| *field)
                .collect();
            let title = removed_title(&fields);
            assert!(title.chars().count() <= 33, "{title}");
        }
        assert_eq!(
            removed_title(&[MetaField::Keywords, MetaField::Other]),
            "Removed keywords and other fields"
        );
    }

    #[test]
    fn a_fallback_or_a_failure_outranks_the_notes() {
        let jobs = [job(
            "/ro/x.tar",
            "/home/max/Downloads/Grape/x Extracted",
            true,
        )];
        let results = [noting(
            "/home/max/Downloads/Grape/x Extracted",
            vec![left_out(2, 0, 0, 0)],
        )];
        let report = finished(&jobs, &results, &settings());
        assert_eq!(report.title, format!("Saved in {}", grape_folder()));
        let two = [
            job("/a/1.tar", "/a/1.zip", false),
            job("/a/2.tar", "/a/2.zip", false),
        ];
        let results = [
            noting("/a/1.zip", vec![left_out(2, 0, 0, 0)]),
            failed("2.tar isn't a readable TAR file."),
        ];
        assert_eq!(finished(&two, &results, &settings()).title, "1 of 2 done");
    }

    fn folder_job(input: &str, output: &str, used_fallback: bool) -> Job {
        Job {
            inputs: vec![PathBuf::from(input)],
            output: JobOutput::Folder(PathBuf::from(output)),
            op: Operation::tool(ToolKey::ExtractArchive),
            used_fallback,
        }
    }

    fn pages_job(input: &str, stem: &str) -> Job {
        Job {
            inputs: vec![PathBuf::from(input)],
            output: JobOutput::PagesFileOrFolder {
                file: PathBuf::from(format!("/a/{stem}.jpg")),
                folder: PathBuf::from(format!("/a/{stem}")),
                page_stem: stem.to_owned(),
            },
            op: Operation::Convert { to: Format::Jpg },
            used_fallback: false,
        }
    }

    #[test]
    fn when_only_some_fell_back_the_notes_keep_the_title() {
        let jobs = [
            folder_job("/a/1.tar", "/a/1 Extracted", false),
            folder_job("/ro/2.tar", "/home/max/Downloads/Grape/2 Extracted", true),
        ];
        let results = [
            noting("/a/1 Extracted", vec![left_out(1, 0, 0, 0)]),
            noting("/home/max/Downloads/Grape/2 Extracted", Vec::new()),
        ];
        let report = finished(&jobs, &results, &settings());
        assert_eq!(report.title, "1 link left out");
        let expected = format!("2 folders; 1 in {}", grape_folder());
        assert_eq!(report.detail.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn folders_and_page_exports_are_not_called_files() {
        // One PDF → JPG: a file for one page, a folder for more.
        let pdf = [pages_job("/a/doc.pdf", "doc")];
        assert_eq!(working(&pdf, 0, None).title, "Making doc…");
        let report = finished(&pdf, &[wrote("/a/doc")], &settings());
        assert_eq!(report.detail.as_deref(), Some("doc"));

        let extract = [
            folder_job("/a/1.zip", "/a/1 Extracted", false),
            folder_job("/a/2.zip", "/a/2 Extracted", false),
        ];
        assert_eq!(working(&extract, 0, None).title, "Making 2 folders…");
        let results = [wrote("/a/1 Extracted"), wrote("/a/2 Extracted")];
        let report = finished(&extract, &results, &settings());
        assert_eq!(report.detail.as_deref(), Some("2 folders"));

        // Page exports may be files or folders.
        let pdfs = [pages_job("/a/a.pdf", "a"), pages_job("/a/b.pdf", "b")];
        assert_eq!(working(&pdfs, 0, None).title, "Making 2 items…");
        let results = [wrote("/a/a.jpg"), wrote("/a/b")];
        let report = finished(&pdfs, &results, &settings());
        assert_eq!(report.detail.as_deref(), Some("2 items"));
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
