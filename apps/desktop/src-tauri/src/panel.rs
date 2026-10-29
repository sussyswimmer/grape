//! The tool panel (`docs/INTERACTION_SPEC.md` §11, DECISIONS D31): a drop on
//! Compress, Audio Channels, Normalize Volume or Merge PDFs opens a small
//! panel beside where the wheel was, and its Run starts the job with the
//! chosen options and order. Rust owns the paths and the window; the page
//! in `ui/panel.html` gets ids and display names (`panel://open`) and
//! answers by seq and ids (`panel_run`, `panel_cancel`).
//!
//! - **Route.** Only a v1 panel tool (`ToolKey::has_v1_panel`) that the
//!   wheel offers for the drop (the multi-OK rule, so a panel never opens
//!   for a run the plan would refuse) and that this build can run opens a
//!   panel ([`route_for`], [`model::route`] with `jobs::check_runnable`);
//!   everything else goes straight to `jobs::submit`. Merge PDFs opens when
//!   Grape's PDF library is found, Compress on JPG, PNG and TIFF always,
//!   and Compress, Audio Channels and Normalize Volume on audio and video
//!   when FFmpeg is (D26, D28). `GRAPE_PREVIEW_PANELS=1` (QA) also opens the four
//!   where they're offered but can't run (Compress on MP3s without
//!   FFmpeg), and their Run meets the same check and shows the same info
//!   toast.
//! - **One at a time.** A new request replaces the open panel, which counts
//!   as cancelled. Run, Cancel, ×, Esc, Alt+F4, Pause and Quit close it.
//!   Losing the focus doesn't.
//! - **Commands** go to the `panel` window only (`capabilities/panel.json`),
//!   and the events only to it ([`PANEL`]).
//! - **The window** is declared hidden in `tauri.conf.json` and hardened in
//!   setup (`ui_windows::prepare_panel`). Every show, move and hide goes
//!   through one main-thread reconcile ([`sync`]), never Tauri's `show`,
//!   `hide` or `set_focus`. On Windows it's shown without activation and
//!   the first click activates it; until then a "grape-panel-esc" thread
//!   closes it on an Esc pressed with no mouse button down, and after it
//!   Esc is the page's; closing an active panel hands the foreground back.
//!   On macOS it's made key and Grape asks to be activated; closing hands
//!   activation back. On both, the page's cancel is refused while a mouse
//!   button is down (that Esc is a drag's). Other systems (Linux dev) use
//!   Tauri's own calls.
//! - **Logs** carry seqs, counts, the tool, reasons and hand-back results,
//!   never a name or a path.

pub mod model;

#[cfg(target_os = "windows")]
use std::thread;
#[cfg(target_os = "windows")]
use std::time::Duration;

use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use grape_core::{SelectionItem, ToolKey, WheelEntry};
use grape_platform::Rect;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};

use crate::controller::RunRequest;
use crate::events::{PanelChoice, PanelClose, PanelOpen, PanelRun};
use crate::{jobs, settings, sidecars};
use model::{PanelModel, PanelRequest, Route};

/// Label of the panel window in tauri.conf.json.
pub const PANEL: &str = "panel";

/// What a malformed `panel_run` is told. The page shows it in its hint.
const BAD_CHOICE: &str = "Grape couldn't use that choice.";

/// What a `panel_cancel` refused during a drag is told. The page doesn't
/// show it.
const DRAGGING: &str = "A mouse button is down.";

/// How often the Esc poll looks (Windows).
#[cfg(target_os = "windows")]
const ESC_POLL: Duration = Duration::from_millis(30);

/// The panel's state, managed by Tauri.
#[derive(Default)]
pub struct PanelState {
    model: Mutex<PanelModel>,
    /// Who gets the keyboard back when the panel closes, as an opaque i64:
    /// the HWND value on Windows, the pid on macOS. Main thread only.
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    back_to: Mutex<Option<i64>>,
    /// `GRAPE_PREVIEW_PANELS=1`: the four panel tools open their panels
    /// wherever the wheel offers them, also where this build can't run
    /// them.
    preview: AtomicBool,
    /// Set while the Esc poll runs.
    #[cfg(target_os = "windows")]
    esc_armed: AtomicBool,
}

impl PanelState {
    fn model(&self) -> MutexGuard<'_, PanelModel> {
        // Every change is one call on the model, so a panic in an earlier
        // holder can't leave it half-changed: keep using it.
        self.model.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn back_to(&self) -> MutexGuard<'_, Option<i64>> {
        self.back_to.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `GRAPE_PREVIEW_PANELS` is exactly `1`.
pub fn preview_from_env() -> bool {
    preview_value(std::env::var_os("GRAPE_PREVIEW_PANELS").as_deref())
}

/// Whether the preview variable's value turns the preview on: exactly `1`.
fn preview_value(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| value == "1")
}

/// Turns the preview on or off. Call in setup.
pub fn set_preview(app: &AppHandle, on: bool) {
    if let Some(state) = app.try_state::<PanelState>() {
        state.preview.store(on, Ordering::Release);
    }
    if on {
        log::info!(
            "panel: preview on: GRAPE_PREVIEW_PANELS=1 opens the tool panels where they're offered, also where this build can't run them"
        );
    }
}

/// Where a drop on `entry` goes: its panel, or straight to the jobs. A
/// panel opens only where the plan would offer its tool, with the features
/// the jobs are planned with.
pub fn route_for(app: &AppHandle, entry: WheelEntry, items: &[SelectionItem]) -> Route {
    let preview = app
        .try_state::<PanelState>()
        .is_some_and(|state| state.preview.load(Ordering::Acquire));
    let sidecars = sidecars::for_jobs(app);
    let features = settings::planning_features();
    if preview {
        model::route(entry, items, features, |op, items| {
            model::preview_check(op, items, &sidecars)
        })
    } else {
        model::route(entry, items, features, |op, items| {
            jobs::check_runnable(op, items, &sidecars)
        })
    }
}

/// Opens `tool`'s panel for the drop in `request`, replacing any open one.
/// Called from the wheel's controller thread; never waits for the main
/// thread. A request that can't have a panel runs the direct way.
pub fn open(app: &AppHandle, tool: ToolKey, request: &RunRequest) {
    let direct = || {
        jobs::submit(
            app,
            WheelEntry::Tool(tool),
            &request.items,
            request.anchor,
            request.display,
        );
    };
    let Some(state) = app.try_state::<PanelState>() else {
        log::error!(
            "panel: no panel state; {} runs without its panel",
            tool.name()
        );
        direct();
        return;
    };
    let memory = settings::tool_memory(app);
    let panel_request = PanelRequest {
        tool,
        items: request.items.clone(),
        anchor: request.anchor,
        display: request.display,
        wheel_seq: request.seq,
    };
    let opened = {
        let mut model = state.model();
        let opened = model.open(panel_request, memory);
        let line = model
            .pending()
            .map(|pending| model::describe(&pending.view, pending.request.wheel_seq));
        opened.map(|(view, replaced)| (view, replaced, line.unwrap_or_default()))
    };
    let Some((view, replaced, line)) = opened else {
        log::warn!(
            "panel: {} can't have a panel for {} item(s); running it directly",
            tool.name(),
            request.items.len()
        );
        direct();
        return;
    };
    match replaced {
        Some(old) => log::info!("{line}; replaced panel {old} (cancelled)"),
        None => log::info!("{line}"),
    }
    emit(app, "panel://open", view);
    sync(app, None);
    #[cfg(target_os = "windows")]
    watch_escape(app);
}

/// Closes panel `seq` if it's the open one: the page's ×, Cancel or Esc,
/// the Esc poll, or a close request (Alt+F4). Nothing runs.
pub fn cancel_seq(app: &AppHandle, seq: u64, why: &'static str) {
    let Some(state) = app.try_state::<PanelState>() else {
        return;
    };
    let closed = state.model().cancel(seq);
    if closed {
        log::info!("panel {seq}: cancelled ({why})");
        sync(app, Some(seq));
    } else {
        log::info!("panel: cancel for panel {seq} ignored ({why}): it isn't open");
    }
}

/// Closes whatever panel is open, and nothing runs: tray → Pause.
pub fn cancel_open(app: &AppHandle, why: &'static str) {
    let Some(state) = app.try_state::<PanelState>() else {
        return;
    };
    let closed = state.model().close_any();
    if let Some(seq) = closed {
        log::info!("panel {seq}: cancelled ({why})");
        sync(app, closed);
    }
}

/// Grape is quitting: forget the open panel, so a late `panel_run` is
/// ignored. No window work; the window goes with the process.
pub fn shut(app: &AppHandle) {
    let Some(state) = app.try_state::<PanelState>() else {
        return;
    };
    let closed = state.model().close_any();
    if let Some(seq) = closed {
        log::info!("panel {seq}: cancelled (quitting)");
    }
}

/// The open request, for a page that just loaded (or reloaded).
#[tauri::command]
pub async fn panel_current(app: AppHandle) -> Option<PanelOpen> {
    current(&app)
}

/// The page's Run: checked against the open request, then the panel closes
/// and the job starts. A stale or second run is ignored; a malformed one is
/// refused with a sentence and the panel stays open.
#[tauri::command]
pub async fn panel_run(
    app: AppHandle,
    seq: u64,
    order: Vec<u64>,
    choice: PanelChoice,
) -> Result<(), String> {
    let Some(state) = app.try_state::<PanelState>() else {
        return Ok(());
    };
    let run = PanelRun { seq, order, choice };
    let result = state.model().run(&run);
    match result {
        Err(why) if why.ignored() => {
            log::info!("panel: run for panel {seq} ignored ({})", why.why());
            Ok(())
        }
        Err(why) => {
            log::warn!("panel: run for panel {seq} refused ({})", why.why());
            Err(BAD_CHOICE.to_owned())
        }
        Ok(accepted) => {
            if let Some(choice) = accepted.remember {
                settings::remember(&app, choice);
            }
            sync(&app, Some(accepted.seq));
            let count = accepted.items.len();
            jobs::submit_op(
                &app,
                accepted.op,
                &accepted.items,
                accepted.anchor,
                accepted.display,
            );
            log::info!("panel {}: run, {count} item(s)", accepted.seq);
            Ok(())
        }
    }
}

/// The page's ×, Cancel or Esc. Refused while a mouse button is down: that
/// Esc belongs to a drag (on macOS a file dragged out of a Finder window
/// behind the key panel leaves Grape active, so the panel's page gets the
/// key, D31), and the page then takes input again. A click on × or Cancel
/// arrives after the button came up.
#[tauri::command]
pub async fn panel_cancel(app: AppHandle, seq: u64) -> Result<(), String> {
    if button_down() {
        log::info!(
            "panel: cancel for panel {seq} refused: a mouse button is down, so its Esc belongs to a drag"
        );
        return Err(DRAGGING.to_owned());
    }
    cancel_seq(&app, seq, "page");
    Ok(())
}

/// Whether a mouse button is down right now. Other systems (Linux dev)
/// can't tell, and say no.
fn button_down() -> bool {
    #[cfg(target_os = "windows")]
    let down = grape_platform::windows::primary_button_down();
    #[cfg(target_os = "macos")]
    let down = grape_platform::macos::mouse_button_down();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let down = false;
    down
}

/// The panel window's events: a close request (Alt+F4 on the active
/// panel) cancels it as × does instead of destroying the window; a wry
/// drop on Windows means Grape's refusing target missed a window; focus
/// changes are logged for QA.
pub fn on_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    if window.label() != PANEL {
        return;
    }
    match event {
        tauri::WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            let app = window.app_handle();
            match open_seq(app) {
                Some(seq) => cancel_seq(app, seq, "closed"),
                None => log::info!("panel: close request with no panel open"),
            }
        }
        // On macOS wry accepts a drop on the panel as a copy and ignores
        // it (D18): nothing to report.
        #[cfg(not(target_os = "macos"))]
        tauri::WindowEvent::DragDrop(_) => {
            log::warn!(
                "panel: wry's drop handler saw a drag; Grape's refusing target missed a window"
            );
        }
        tauri::WindowEvent::Focused(focused) => {
            log::info!("panel: focused {focused}");
            let Some(state) = window.app_handle().try_state::<PanelState>() else {
                return;
            };
            let first = state.model().focus_changed(*focused);
            if let Some(seq) = first {
                log::info!("panel {seq}: focused for the first time; Esc is the page's now");
            }
        }
        _ => {}
    }
}

/// A copy of the open request.
fn current(app: &AppHandle) -> Option<PanelOpen> {
    let state = app.try_state::<PanelState>()?;
    let model = state.model();
    model.current().cloned()
}

/// The open panel's seq.
fn open_seq(app: &AppHandle) -> Option<u64> {
    let state = app.try_state::<PanelState>()?;
    let model = state.model();
    model.current().map(|view| view.seq)
}

fn emit<P: Serialize + Clone>(app: &AppHandle, event: &str, payload: P) {
    if let Err(err) = app.emit_to(PANEL, event, payload) {
        log::error!("panel: could not send {event}: {err}");
    }
}

/// Queues one main-thread task that makes the window match the model:
/// shown at the open panel's frame, or hidden (handing the keyboard back)
/// with `panel://close` for `closed` once it is. Every change goes through
/// this one reconcile, so queued shows and hides can't leave the window in
/// the wrong state.
fn sync(app: &AppHandle, closed: Option<u64>) {
    let Some(window) = app.get_webview_window(PANEL) else {
        log::error!("panel: the panel window from tauri.conf.json is missing");
        return;
    };
    let main = app.clone();
    let queued = app.run_on_main_thread(move || reconcile(&main, &window, closed));
    if let Err(err) = queued {
        log::error!("panel: could not reach the main thread: {err}");
    }
}

/// [`sync`]'s work, on the main thread.
fn reconcile(app: &AppHandle, window: &WebviewWindow, closed: Option<u64>) {
    let Some(state) = app.try_state::<PanelState>() else {
        return;
    };
    let frame = state.model().pending().map(|pending| pending.frame);
    match frame {
        Some(frame) => show(&state, window, frame),
        None => {
            hide(&state, window);
            if let Some(seq) = closed {
                emit(app, "panel://close", PanelClose { seq });
            }
        }
    }
}

/// Shows the panel at `frame` without activating it, after refusing drops
/// on whatever windows WebView2 made since. Remembers the window in front
/// to hand the keyboard back to; a replacement while the panel itself is
/// in front keeps the old one.
#[cfg(target_os = "windows")]
fn show(state: &PanelState, window: &WebviewWindow, frame: Rect) {
    let hwnd = match window.hwnd() {
        Ok(hwnd) => hwnd.0 as isize,
        Err(err) => {
            log::error!("panel: no window handle: {err}");
            return;
        }
    };
    crate::ui_windows::refuse_drops_here(window);
    if let Some(front) = grape_platform::windows::other_app_foreground() {
        *state.back_to() = Some(front as i64);
    }
    match grape_platform::windows::show_panel(hwnd, frame) {
        Ok(()) => log::info!(
            "panel: shown at ({}, {}), {} x {}",
            frame.x,
            frame.y,
            frame.width,
            frame.height
        ),
        Err(err) => log::error!("panel: could not show the panel: {err}"),
    }
}

/// Hides the panel, handing the foreground back first if it's active.
#[cfg(target_os = "windows")]
fn hide(state: &PanelState, window: &WebviewWindow) {
    let hwnd = match window.hwnd() {
        Ok(hwnd) => hwnd.0 as isize,
        Err(err) => {
            log::error!("panel: no window handle: {err}");
            return;
        }
    };
    let back_to = state.back_to().take().map(|raw| raw as isize);
    match grape_platform::windows::hide_panel(hwnd, back_to) {
        Ok(handed) => log::info!("panel: hidden; foreground {handed:?}"),
        Err(err) => log::error!("panel: could not hide the panel: {err}"),
    }
}

/// Shows the panel at `frame`, makes it key and asks macOS to activate
/// Grape. Remembers the app in front to hand activation back to; a
/// replacement while Grape is in front keeps the old one.
#[cfg(target_os = "macos")]
fn show(state: &PanelState, window: &WebviewWindow, frame: Rect) {
    let ns_window = match window.ns_window() {
        Ok(ns_window) => ns_window,
        Err(err) => {
            log::error!("panel: no NSWindow: {err}");
            return;
        }
    };
    // SAFETY: this runs on the main thread, and `ns_window` is the panel's
    // NSWindow, which Tauri just returned and keeps alive while `window`
    // is.
    match unsafe { grape_platform::macos::show_panel(ns_window, frame) } {
        Ok(shown) => {
            let mut back_to = state.back_to();
            *back_to = shown.front_pid.map(i64::from).or(*back_to);
            log::info!(
                "panel: shown at ({}, {}), {} x {}; app in front recorded: {}",
                frame.x,
                frame.y,
                frame.width,
                frame.height,
                shown.front_pid.is_some()
            );
        }
        Err(err) => log::error!("panel: could not show the panel: {err}"),
    }
}

/// Hides the panel, handing activation back if it was key.
#[cfg(target_os = "macos")]
fn hide(state: &PanelState, window: &WebviewWindow) {
    let ns_window = match window.ns_window() {
        Ok(ns_window) => ns_window,
        Err(err) => {
            log::error!("panel: no NSWindow: {err}");
            return;
        }
    };
    let back_to = state
        .back_to()
        .take()
        .and_then(|pid| i32::try_from(pid).ok());
    // SAFETY: as in `show`.
    match unsafe { grape_platform::macos::hide_panel(ns_window, back_to) } {
        Ok(handed) => log::info!("panel: hidden; activation handed back: {handed}"),
        Err(err) => log::error!("panel: could not hide the panel: {err}"),
    }
}

/// Other systems (Linux dev machines): Tauri's own calls. Never reached in
/// practice, since no drag opens a wheel there.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn show(_state: &PanelState, window: &WebviewWindow, frame: Rect) {
    let position = tauri::PhysicalPosition::new(frame.x, frame.y);
    let size = tauri::PhysicalSize::new(frame.width.unsigned_abs(), frame.height.unsigned_abs());
    let shown = window
        .set_position(position)
        .and_then(|()| window.set_size(size))
        .and_then(|()| window.show());
    if let Err(err) = shown {
        log::error!("panel: could not show the panel: {err}");
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn hide(_state: &PanelState, window: &WebviewWindow) {
    if let Err(err) = window.hide() {
        log::error!("panel: could not hide the panel: {err}");
    }
}

/// Starts the Esc poll unless it runs already.
#[cfg(target_os = "windows")]
fn watch_escape(app: &AppHandle) {
    let Some(state) = app.try_state::<PanelState>() else {
        return;
    };
    if state.esc_armed.swap(true, Ordering::AcqRel) {
        return;
    }
    let poll_app = app.clone();
    let spawned = thread::Builder::new()
        .name("grape-panel-esc".to_owned())
        .spawn(move || poll_escape(&poll_app));
    if let Err(err) = spawned {
        log::error!("panel: could not start the Esc poll: {err}");
        state.esc_armed.store(false, Ordering::Release);
    }
}

/// While a panel is open, closes it on an Esc press made with no mouse
/// button down, in this sample or the one before (an Esc during a drag
/// belongs to the drag: the next wheel's, or the page's own reorder), and
/// only while the panel's window hasn't had the focus since it opened
/// (then Esc is the page's, and an Esc in another app is that app's,
/// [`model::PanelModel::esc_target`]). Reads the key, never consumes it.
/// Stops when no panel is open, unless one opened just as it stopped.
#[cfg(target_os = "windows")]
fn poll_escape(app: &AppHandle) {
    use grape_platform::windows::{escape_down, primary_button_down};

    let Some(state) = app.try_state::<PanelState>() else {
        return;
    };
    let mut esc = model::EscPoll::new(escape_down(), primary_button_down());
    loop {
        thread::sleep(ESC_POLL);
        if open_seq(app).is_none() {
            state.esc_armed.store(false, Ordering::Release);
            if open_seq(app).is_some() && !state.esc_armed.swap(true, Ordering::AcqRel) {
                continue;
            }
            return;
        }
        if !esc.sample(escape_down(), primary_button_down()) {
            continue;
        }
        let target = state.model().esc_target();
        if let Some(seq) = target {
            cancel_seq(app, seq, "esc");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use serde_json::Value;

    const COMMANDS: [&str; 3] = ["panel_current", "panel_run", "panel_cancel"];

    fn capabilities() -> Vec<(String, String)> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities");
        let mut files: Vec<(String, String)> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                (name, fs::read_to_string(&path).unwrap())
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn only_the_panel_window_gets_the_panel_commands() {
        let panel: Value =
            serde_json::from_str(include_str!("../capabilities/panel.json")).unwrap();
        assert_eq!(panel["windows"], serde_json::json!(["panel"]));
        assert_eq!(
            panel["permissions"],
            serde_json::json!([
                "core:event:allow-listen",
                "core:event:allow-unlisten",
                "allow-panel-current",
                "allow-panel-run",
                "allow-panel-cancel"
            ])
        );
        let files = capabilities();
        assert!(files.iter().any(|(name, _)| name == "panel.json"));
        for (name, text) in files {
            if name != "panel.json" {
                assert!(!text.contains("allow-panel-"), "{name}");
            }
        }
    }

    #[test]
    fn build_rs_lists_the_panel_commands() {
        let build = include_str!("../build.rs");
        for command in COMMANDS {
            assert!(build.contains(&format!("\"{command}\"")), "{command}");
        }
    }

    #[test]
    fn the_preview_needs_exactly_one() {
        use std::ffi::OsStr;

        use super::preview_value;

        assert!(preview_value(Some(OsStr::new("1"))));
        for off in ["", "0", "true", "yes", " 1", "1 ", "01"] {
            assert!(!preview_value(Some(OsStr::new(off))), "{off:?}");
        }
        assert!(!preview_value(None));
    }
}
