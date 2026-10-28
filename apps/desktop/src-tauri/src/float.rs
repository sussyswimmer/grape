//! The float shelf: Alt-drop (Windows) or Control-drop (macOS) on FLOAT
//! puts the items on a small always-on-top window at the right edge of the
//! screen, and each can be dragged out of it into any app as a copy or a
//! shortcut (an alias on the Mac), never a move (golden rule 2). Rust owns
//! the paths and the window; the page in `ui/float.html` gets ids and names
//! (`float://view`) and asks for removes, reveals and drags by id.
//!
//! - The model ([`Shelf`]) is in memory only: items in drop order, no path
//!   twice, at most [`CAPACITY`]. Quitting clears it. It, the placement,
//!   the snap and the commands are shared by both OSes; only the window's
//!   native calls and the drag out are each OS's own (`window::native`).
//! - Windows (D21): the window is built on first use, off the main thread,
//!   hidden. It's shown, resized and hidden through
//!   `grape_platform::windows::show_shelf` / `hide_shelf` on the main
//!   thread, never Tauri's `show` or `set_focus`, so it opens without
//!   taking focus. Clicking it does activate Grape: the drag out needs that
//!   to take the mouse capture. It refuses drops (D13), at creation and
//!   before every show, so dragging an item back onto the shelf does
//!   nothing.
//! - macOS (D43): the window is built hidden at launch, because wry asks
//!   macOS to activate Grape whenever it builds a webview, which on the
//!   first Float would take the keyboard from the app in front. It's
//!   hardened as `WindowRole::Shelf` (the floating level, 3, under every
//!   other Grape window; the `Closable` bit, so Cmd+W is a close request)
//!   and shown, resized and hidden through `show_shelf` / `hide_shelf`
//!   (`grape_platform::macos`) on the main thread: never key, never
//!   activating Grape, and hiding hands activation back to the app the user
//!   was last in (followed from launch) if a click had made the shelf key.
//!   Its frame is read in points (`window_frame`), never Tauri's backing
//!   pixels. wry takes every drop on it as a copy and ignores it (D18).
//! - Moved by its header and left within reach of a screen edge, it snaps
//!   flush to that edge once the mouse button is up.
//! - It can't be minimized or maximized, and a close request (Alt+F4 on
//!   Windows, Cmd+W on macOS, once a click made it the active window) is
//!   turned into the header's ×: the shelf is emptied and hidden, never
//!   destroyed with its items kept. A shelf minimized anyway counts as
//!   closed, so the next Float places it again.
//! - `float_drag_out` is async: it drops the items that are gone on its
//!   own thread, then queues the drag on the main thread (a sync command
//!   would run it inside the webview's IPC callback); the page hears
//!   `float://drag-ended` when it's over. On Windows the drag blocks the
//!   main thread until the drop; on macOS AppKit's session returns at once
//!   and its source reports the end.
//!
//! Linux never builds the window: Float is "coming" there, and the commands
//! only see an empty shelf.

use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

#[cfg(any(target_os = "windows", target_os = "macos"))]
use anyhow::Context as _;
use grape_core::{ItemKind, SelectionItem};
use grape_platform::{DisplayBounds, DragItem, DragOutcome, PlatformError, Rect};
use tauri::{AppHandle, Emitter, Manager};

use crate::events::{FloatDragEnded, FloatItem, FloatKind, FloatOutcome, FloatView};

/// Label of the shelf window.
pub const FLOAT: &str = "float";
/// How many items the shelf holds.
pub const CAPACITY: usize = 50;

/// The shelf's width in logical pixels. The page lays out to fill the
/// window.
const WIDTH: f64 = 280.0;
/// Room around the panel for its shadow, on every side.
const INSET: f64 = 6.0;
/// The header: title, count and the close button.
const HEADER: f64 = 44.0;
/// One item.
const ROW: f64 = 40.0;
/// The "Drag all" row, there from two items.
const FOOTER: f64 = 40.0;
/// Rows shown before the list scrolls.
const ROWS_SHOWN: usize = 6;
/// Space between the shelf and the screen edge it sits at.
const MARGIN: f64 = 12.0;
/// How near an edge a moved shelf must be left to snap to it.
const SNAP_REACH: f64 = 48.0;
/// How long the look at a drag's items may take before the press that
/// asked for it counts as over: a local disk answers at once.
const STALE_PRESS: Duration = Duration::from_secs(1);

/// One item on the shelf.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Item {
    id: u64,
    path: PathBuf,
    kind: FloatKind,
}

/// What's on the shelf, oldest first.
#[derive(Debug, Clone, Default)]
struct Model {
    items: Vec<Item>,
    /// The last id handed out. Ids are never reused, so a page that's a
    /// view behind can't name a newer item.
    last_id: u64,
}

/// What putting items on the shelf did.
#[cfg_attr(
    not(any(target_os = "windows", target_os = "macos")),
    allow(dead_code, reason = "only Windows and macOS have the shelf window")
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Added {
    /// Put on the shelf.
    pub added: usize,
    /// On it already (the same path).
    pub already: usize,
    /// Didn't fit: the shelf holds [`CAPACITY`] items.
    pub left_out: usize,
}

impl Model {
    /// Puts `items` after what's there, in order, skipping paths already
    /// on the shelf, until it holds [`CAPACITY`].
    #[cfg_attr(
        not(any(target_os = "windows", target_os = "macos")),
        allow(dead_code, reason = "only Windows and macOS have the shelf window")
    )]
    fn add(&mut self, items: &[SelectionItem]) -> Added {
        let mut added = Added::default();
        for item in items {
            if self.items.iter().any(|known| known.path == item.path) {
                added.already += 1;
            } else if self.items.len() >= CAPACITY {
                added.left_out += 1;
            } else {
                self.last_id += 1;
                self.items.push(Item {
                    id: self.last_id,
                    path: item.path.clone(),
                    kind: match item.kind {
                        ItemKind::Folder => FloatKind::Folder,
                        ItemKind::File { .. } => FloatKind::File,
                    },
                });
                added.added += 1;
            }
        }
        added
    }

    /// Takes item `id` off the shelf. `false` if it wasn't there.
    fn remove(&mut self, id: u64) -> bool {
        let before = self.items.len();
        self.items.retain(|item| item.id != id);
        self.items.len() != before
    }

    fn clear(&mut self) {
        self.items.clear();
    }

    fn path(&self, id: u64) -> Option<PathBuf> {
        self.items
            .iter()
            .find(|item| item.id == id)
            .map(|item| item.path.clone())
    }

    /// The items of `ids` as a drag out carries them, in shelf order, each
    /// with whether it's a folder as it was dropped. Unknown ids are
    /// ignored.
    fn drag_items(&self, ids: &[u64]) -> Vec<DragItem> {
        self.items
            .iter()
            .filter(|item| ids.contains(&item.id))
            .map(|item| DragItem {
                path: item.path.clone(),
                folder: item.kind == FloatKind::Folder,
            })
            .collect()
    }

    /// What the page shows. `exists` tells whether an item is still there.
    fn view(&self, exists: impl Fn(&Path) -> bool) -> FloatView {
        FloatView {
            items: self
                .items
                .iter()
                .map(|item| FloatItem {
                    id: item.id,
                    name: display_name(&item.path),
                    kind: item.kind,
                    missing: !exists(&item.path),
                })
                .collect(),
        }
    }
}

/// The name the shelf shows: the item's file name; for a drive or share
/// root, which has none, its last part other than the root itself ("D:");
/// the whole path only when there's nothing else ("/").
pub(crate) fn display_name(path: &Path) -> String {
    if let Some(name) = path.file_name() {
        return name.to_string_lossy().into_owned();
    }
    path.components()
        .rev()
        .find(|part| !matches!(part, Component::RootDir))
        .map_or(path.as_os_str(), |part| part.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// Still there. An item whose state can't be read counts as there: the
/// drag decides for itself.
fn exists(path: &Path) -> bool {
    path.try_exists().unwrap_or(true)
}

/// The items a drag out can carry: those still there, in order. One whose
/// state can't be read is left out, as the Windows platform's own check
/// does.
fn draggable<T: AsRef<Path>>(items: Vec<T>) -> Vec<T> {
    items
        .into_iter()
        .filter(|item| item.as_ref().try_exists().unwrap_or(false))
        .collect()
}

/// The page's word for how a drag ended.
#[cfg_attr(
    not(any(target_os = "windows", target_os = "macos")),
    allow(dead_code, reason = "only Windows and macOS have the shelf window")
)]
fn outcome(drag: DragOutcome) -> FloatOutcome {
    match drag {
        DragOutcome::Copied => FloatOutcome::Copied,
        DragOutcome::Linked => FloatOutcome::Linked,
        DragOutcome::Cancelled => FloatOutcome::Cancelled,
        DragOutcome::ButtonUp | DragOutcome::NothingToDrag => FloatOutcome::Nothing,
    }
}

/// The window's height in logical pixels for `count` items: the inset, the
/// header, up to [`ROWS_SHOWN`] rows and, from two items, "Drag all".
fn height(count: usize) -> f64 {
    let rows = count.min(ROWS_SHOWN) as f64;
    let footer = if count >= 2 { FOOTER } else { 0.0 };
    2.0 * INSET + HEADER + rows * ROW + footer
}

/// `logical` in physical pixels at `scale` (a bad scale counts as 1).
fn px(logical: f64, scale: f64) -> i32 {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    (logical * scale).round() as i32
}

/// `rect` moved, not resized, to lie inside `work`. When it's bigger than
/// `work`, its top-left corner stays inside.
fn inside(rect: Rect, work: Rect) -> Rect {
    Rect {
        x: rect
            .x
            .clamp(work.x, (work.x + work.width - rect.width).max(work.x)),
        y: rect
            .y
            .clamp(work.y, (work.y + work.height - rect.height).max(work.y)),
        ..rect
    }
}

/// Where a closed shelf opens with `count` items: at the right edge of
/// `display`'s work area, [`MARGIN`] in, centered top to bottom. Global
/// physical pixels on Windows, points on macOS (where the scale is 1).
#[cfg_attr(
    not(any(target_os = "windows", target_os = "macos")),
    allow(dead_code, reason = "only Windows and macOS have the shelf window")
)]
pub fn place_at_right_edge(display: &DisplayBounds, count: usize) -> Rect {
    let work = display.work_area;
    let width = px(WIDTH, display.scale);
    let height = px(height(count), display.scale);
    let frame = Rect {
        x: work.x + work.width - width - px(MARGIN, display.scale),
        y: work.y + (work.height - height) / 2,
        width,
        height,
    };
    inside(frame, work)
}

/// An open shelf at `frame` with `count` items now: it stays where it is,
/// sized for `count` at `display`'s scale, kept inside the work area.
#[cfg_attr(
    not(any(target_os = "windows", target_os = "macos")),
    allow(dead_code, reason = "only Windows and macOS have the shelf window")
)]
fn resized(frame: Rect, display: &DisplayBounds, count: usize) -> Rect {
    let frame = Rect {
        width: px(WIDTH, display.scale),
        height: px(height(count), display.scale),
        ..frame
    };
    inside(frame, display.work_area)
}

/// Where a shelf the user left at `rect` settles: flush with the work
/// area's left or right edge, [`MARGIN`] in, when its own edge is within
/// [`SNAP_REACH`] of that edge or past it (the nearer edge when both are);
/// otherwise where it is. Always inside `work`. A snapped shelf snaps to
/// where it already is.
#[cfg_attr(
    not(any(target_os = "windows", target_os = "macos")),
    allow(dead_code, reason = "only Windows and macOS have the shelf window")
)]
pub fn snap(rect: Rect, work: Rect, scale: f64) -> Rect {
    let reach = px(SNAP_REACH, scale);
    let margin = px(MARGIN, scale);
    let left_gap = rect.x - work.x;
    let right_gap = work.x + work.width - (rect.x + rect.width);
    let left = work.x + margin;
    let right = work.x + work.width - rect.width - margin;
    let x = match (left_gap <= reach, right_gap <= reach) {
        (true, true) if right_gap < left_gap => right,
        (true, _) => left,
        (false, true) => right,
        (false, false) => rect.x,
    };
    inside(Rect { x, ..rect }, work)
}

/// The float shelf's state, managed by Tauri.
#[derive(Debug, Default)]
pub struct Shelf {
    model: Mutex<Model>,
    /// Held while a view is worked out and sent, so the page gets views
    /// in the order the changes happened.
    publishing: Mutex<()>,
    /// Debounces the snap after the user moves the shelf.
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    snap: std::sync::Arc<window::SnapTimer>,
    /// The app in front at the last show that wasn't Grape. When the shelf
    /// hides after a click made it key, activation goes back to the app the
    /// user was last in (`grape_platform::macos::follow_app_activations`);
    /// this one only when none of those is known. Main thread only.
    #[cfg(target_os = "macos")]
    back_to: Mutex<Option<i32>>,
}

impl Shelf {
    fn lock(&self) -> MutexGuard<'_, Model> {
        // A panic in an earlier holder leaves a whole model behind (every
        // change is one push, retain or clear): keep using it.
        self.model.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The view now. Looks at the disk outside the lock.
    fn view(&self) -> FloatView {
        let model = self.lock().clone();
        model.view(exists)
    }
}

/// Puts `items` on the shelf after what's there and shows it: a closed
/// shelf opens at the right edge of `display`'s work area, an open one
/// stays where it is and only its height changes.
///
/// Builds the window on first use (on macOS, only if building it at launch
/// failed), so call it off the main thread, and it waits for the main
/// thread to show the shelf: never from a thread the main thread is waiting
/// for ("grape-action" is fine).
///
/// # Errors
/// Fails if the window can't be built or shown. Nothing is added when it
/// can't be built.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn add(
    app: &AppHandle,
    items: &[SelectionItem],
    display: &DisplayBounds,
) -> anyhow::Result<Added> {
    window::window(app).context("could not build the shelf window")?;
    let added = app.state::<Shelf>().lock().add(items);
    log::info!(
        "float: {} added, {} already there, {} left out",
        added.added,
        added.already,
        added.left_out
    );
    publish(app);
    window::sync(app, Some(*display))
        .recv()
        .context("the main thread didn't answer")??;
    Ok(added)
}

/// Builds the shelf's window, hidden, at launch (macOS): wry asks macOS to
/// activate Grape whenever it builds a webview, which is part of launching
/// anyway but would take the keyboard from the app in front on the first
/// Float. First it starts following which app the user is in, so the
/// shelf's hide can hand activation back to the one they were last in.
/// Call it from setup, on the main thread. A failure is logged, and the
/// first Float tries again to build the window.
#[cfg(target_os = "macos")]
pub fn build_at_launch(app: &AppHandle) {
    // Before the build below can activate Grape.
    if let Err(err) = grape_platform::macos::follow_app_activations() {
        log::error!("float: not following app activations: {err}");
    }
    if let Err(err) = window::window(app) {
        log::error!("float: could not build the shelf window at launch: {err}");
    }
}

/// Sends the shelf's view to its page.
fn publish(app: &AppHandle) {
    let shelf = app.state::<Shelf>();
    let _ordered = shelf
        .publishing
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Err(err) = app.emit_to(FLOAT, "float://view", shelf.view()) {
        log::error!("could not send float://view: {err}");
    }
}

/// After a remove or a clear: the page gets the view, and the window
/// follows (an empty shelf hides).
fn changed(app: &AppHandle) {
    publish(app);
    // Nobody waits: the main thread logs a failure itself.
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    drop(window::sync(app, None));
}

fn drag_ended(app: &AppHandle, outcome: FloatOutcome) {
    if let Err(err) = app.emit_to(FLOAT, "float://drag-ended", FloatDragEnded { outcome }) {
        log::error!("could not send float://drag-ended: {err}");
    }
}

/// The shelf as it is now. The page asks once when it loads, so it never
/// misses the first view.
#[tauri::command]
pub async fn float_view(app: AppHandle) -> FloatView {
    app.state::<Shelf>().view()
}

/// The × on item `id`: takes it off the shelf (the file stays where it
/// is). Taking the last one off hides the shelf.
#[tauri::command]
pub async fn float_remove(app: AppHandle, id: u64) {
    if !app.state::<Shelf>().lock().remove(id) {
        log::info!("float: remove of an item that isn't on the shelf");
    }
    changed(&app);
}

/// Empties the shelf and hides it: the header's ×, and a close request on
/// its window. Nothing on disk changes. Looks at the disk (the view), so
/// never on the main thread.
fn clear(app: &AppHandle) {
    app.state::<Shelf>().lock().clear();
    log::info!("float: shelf cleared");
    changed(app);
}

/// The header's ×: clears the shelf and hides it. Nothing on disk changes.
#[tauri::command]
pub async fn float_clear(app: AppHandle) {
    clear(&app);
}

/// Double-click or Enter on item `id`: shows it in Explorer or Finder. An
/// item that's gone shows nothing (Explorer would open some other folder);
/// the page gets a view, so its row turns Missing.
#[tauri::command]
pub async fn float_reveal(app: AppHandle, id: u64) {
    let Some(path) = app.state::<Shelf>().lock().path(id) else {
        log::info!("float: reveal of an item that isn't on the shelf");
        return;
    };
    if !exists(&path) {
        log::info!("float: reveal of an item that's gone");
        publish(&app);
        return;
    }
    match grape_platform::file_actions::reveal(&path) {
        Ok(()) => {}
        Err(PlatformError::Unsupported) => log::info!("float: reveal isn't wired on this OS"),
        Err(err) => log::error!("float: could not reveal an item: {err}"),
    }
}

/// A drag out of the shelf with items `ids` (unknown ones are ignored).
/// Returns at once: the drag runs on the main thread, and the page hears
/// `float://drag-ended` when it's over. Async, so it never runs inside the
/// webview's IPC callback.
///
/// The items that are gone are dropped here first, on this thread: a path
/// on a share that stopped answering can take the network's timeout to
/// look at, and the main thread must not stall on it while the button is
/// held. When none is left, no drag is queued. On Windows the platform
/// looks again right before the drag; on macOS nothing on the main thread
/// looks at the disk, and an item gone since just fails at the target.
#[tauri::command]
pub async fn float_drag_out(app: AppHandle, ids: Vec<u64>) {
    // Copied out: the lock is never held across the disk or the drag.
    let items = app.state::<Shelf>().lock().drag_items(&ids);
    if items.is_empty() {
        log::info!("float: nothing to drag out");
        drag_ended(&app, FloatOutcome::Nothing);
        return;
    }
    let asked = items.len();
    let looked = Instant::now();
    let items = draggable(items);
    let gone = asked - items.len();
    if looked.elapsed() >= STALE_PRESS {
        // A share that stopped answering held the check up. The press that
        // asked for this drag may be over, and a new one (on another row,
        // or in another app) would carry these items off: drag nothing.
        log::info!("float: looking at the items took too long; no drag");
        drag_ended(&app, FloatOutcome::Cancelled);
        publish(&app);
        return;
    }
    if items.is_empty() {
        log::info!("float: none of the {asked} item(s) to drag out is there any more");
        drag_ended(&app, FloatOutcome::Nothing);
        // Their rows turn Missing.
        publish(&app);
        return;
    }
    #[cfg(target_os = "windows")]
    window::drag_out(&app, items.into_iter().map(|item| item.path).collect());
    #[cfg(target_os = "macos")]
    window::drag_out(&app, items);
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        log::warn!(
            "float: dragging {} item(s) out isn't available on this OS",
            items.len()
        );
        drag_ended(&app, FloatOutcome::Failed);
    }
    if gone > 0 {
        // The drag is queued: the rows of the ones left out turn Missing.
        log::info!("float: {gone} of {asked} item(s) to drag out are gone");
        publish(&app);
    }
}

/// The shelf window's events: a move arms the snap, and a close request
/// (Alt+F4 on Windows, Cmd+W on macOS, on the active shelf) does what the
/// header's × does instead of destroying the window while its items stay.
/// On Windows wry's drop handler should never fire once Grape's refusing
/// target is registered; if it does, a window was missed.
pub fn on_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    if window.label() != FLOAT {
        return;
    }
    match event {
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        tauri::WindowEvent::Moved(_) => window::moved(window.app_handle()),
        tauri::WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            log::info!("float: close request on the shelf, taken as its ×");
            // This is the main thread, and clearing looks at the disk and
            // queues the hide back here: do it on a worker.
            let app = window.app_handle().clone();
            tauri::async_runtime::spawn_blocking(move || clear(&app));
        }
        // On macOS wry accepts a drop on the shelf as a copy and ignores
        // it (D18): nothing to report, and every Enter and Over would log.
        #[cfg(not(target_os = "macos"))]
        tauri::WindowEvent::DragDrop(_) => {
            log::warn!(
                "float: wry's drop handler saw a drag; Grape's refusing target missed a window"
            );
        }
        _ => {}
    }
}

/// The shelf's window, on Windows and macOS. Everything here but `native`
/// is the same on both: building the window, matching it to the model,
/// placing it and the snap. `native` holds each OS's calls.
#[cfg(any(target_os = "windows", target_os = "macos"))]
mod window {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, Receiver};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::thread;
    use std::time::{Duration, Instant};

    use anyhow::Context as _;
    use grape_platform::{DisplayBounds, Rect};
    use tauri::utils::config::BackgroundThrottlingPolicy;
    use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

    use super::{height, place_at_right_edge, resized, snap, Shelf, FLOAT, WIDTH};
    use crate::ui_windows::BROWSER_ARGS;

    pub(super) use native::drag_out;

    /// How long the shelf must be still after a move before it snaps.
    const SNAP_DELAY: Duration = Duration::from_millis(300);
    /// How often a held mouse button is looked at again.
    const BUTTON_POLL: Duration = Duration::from_millis(50);

    /// Held while the shelf window is looked up and, if needed, built, so
    /// two quick drops can't both build one (Tauri only rejects a
    /// duplicate label once the first window is registered).
    static CREATING: Mutex<()> = Mutex::new(());

    /// The shelf window, built hidden on first use. On Windows off the main
    /// thread only: building a WebView2 window waits for the main thread.
    /// On macOS it's built at launch, from setup on the main thread, where
    /// Tauri builds it inline; the first Float builds it from
    /// "grape-action" only if that failed.
    pub(super) fn window(app: &AppHandle) -> tauri::Result<WebviewWindow> {
        // The lock guards no data: a panic in an earlier holder leaves
        // nothing broken.
        let _creating = CREATING.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(window) = app.get_webview_window(FLOAT) {
            return Ok(window);
        }
        // Not `focusable(false)`: clicking the shelf must activate Grape,
        // or the drag out can't take the mouse capture on Windows, and the
        // page's keys (Enter, Delete, Tab) need it key on both OSes. Drag
        // and drop stays enabled, so the webview never handles a drop
        // itself: on Windows Grape's refusing target replaces tao's and
        // wry's handlers; on macOS wry takes every drop as a copy and
        // ignores it (D18). Active, it hears the window keys: no minimize
        // or maximize (a minimized tool window has no button to come back
        // from), and Alt+F4 or Cmd+W still gets through as a close
        // request, which `on_window_event` turns into the header's ×.
        // `accept_first_mouse` (macOS only): the first press on an inactive
        // shelf reaches the page, so a drag starts on it.
        // `additional_browser_args` and `skip_taskbar` do nothing on macOS.
        let window = WebviewWindowBuilder::new(app, FLOAT, WebviewUrl::App("float.html".into()))
            .title("Grape Shelf")
            .inner_size(WIDTH, height(1))
            .visible(false)
            .transparent(true)
            .decorations(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .focused(false)
            .resizable(false)
            .minimizable(false)
            .maximizable(false)
            .shadow(false)
            .visible_on_all_workspaces(true)
            .accept_first_mouse(true)
            .background_throttling(BackgroundThrottlingPolicy::Disabled)
            .additional_browser_args(BROWSER_ARGS)
            .build()?;
        log::info!("float: shelf window built");
        prepare(app, &window);
        Ok(window)
    }

    /// Hardens the new window on the main thread, before anything shows
    /// it ([`native::prepare`]).
    fn prepare(app: &AppHandle, window: &WebviewWindow) {
        let window = window.clone();
        let queued = app.run_on_main_thread(move || native::prepare(&window));
        if let Err(err) = queued {
            log::error!("float: could not reach the main thread: {err}");
        }
    }

    /// Makes the window match the model on the main thread: hidden when the
    /// shelf is empty; otherwise sized for its items, and a closed shelf
    /// opens on `open_on` (without one it stays closed). The receiver gets
    /// the result; a failure is also logged there.
    pub(super) fn sync(
        app: &AppHandle,
        open_on: Option<DisplayBounds>,
    ) -> Receiver<anyhow::Result<()>> {
        let (tx, rx) = mpsc::channel();
        let app_main = app.clone();
        // If the task can't be queued it's dropped with `tx`, so a waiting
        // `recv` fails instead of hanging.
        let queued = app.run_on_main_thread(move || {
            let result = apply(&app_main, open_on);
            if let Err(err) = &result {
                log::error!("float: {err:#}");
            }
            // Only `add` waits for this; a command's change doesn't.
            let _ = tx.send(result);
        });
        if let Err(err) = queued {
            log::error!("float: could not reach the main thread: {err}");
        }
        rx
    }

    /// [`sync`]'s work, on the main thread. Never Tauri's show or hide:
    /// those would activate the shelf or lose track of it.
    fn apply(app: &AppHandle, open_on: Option<DisplayBounds>) -> anyhow::Result<()> {
        let Some(window) = app.get_webview_window(FLOAT) else {
            // Never built: nothing to show or hide.
            return Ok(());
        };
        let count = app.state::<Shelf>().lock().items.len();
        let shown = window
            .is_visible()
            .context("could not ask if the shelf is open")?;
        if count == 0 {
            if shown {
                native::hide(app, &window)?;
                log::info!("float: shelf hidden");
            }
            return Ok(());
        }
        // A minimized shelf is still "visible" to Windows, but nobody can
        // see or reach it (a tool window has no taskbar button): it counts
        // as closed, so a drop places it again and `show_shelf` restores
        // it. The Mac's shelf can't be minimized at all.
        let open = shown && !minimized(&window)?;
        let frame = if open {
            let frame = native::frame_of(&window)?;
            match display_under(frame).or(open_on) {
                Some(display) => resized(frame, &display, count),
                None => frame,
            }
        } else {
            let Some(display) = open_on else {
                return Ok(());
            };
            place_at_right_edge(&display, count)
        };
        native::show(app, &window, frame)?;
        if !open {
            log::info!("float: shelf shown with {count} item(s)");
        }
        Ok(())
    }

    /// Whether the shelf is minimized. It can't be through its own styles,
    /// but a stray minimize must never leave it out of reach.
    fn minimized(window: &WebviewWindow) -> anyhow::Result<bool> {
        let minimized = window
            .is_minimized()
            .context("could not ask if the shelf is minimized")?;
        if minimized {
            log::warn!("float: the shelf is minimized; it counts as closed");
        }
        Ok(minimized)
    }

    /// The display under the middle of `frame`.
    fn display_under(frame: Rect) -> Option<DisplayBounds> {
        let x = frame.x.saturating_add(frame.width / 2);
        let y = frame.y.saturating_add(frame.height / 2);
        match grape_platform::display_bounds_at(x, y) {
            Ok(display) => Some(display),
            Err(err) => {
                log::warn!("float: no display under the shelf: {err}");
                None
            }
        }
    }

    /// Debounces the snap. Every `Moved` bumps the generation and the time;
    /// one waiting thread at a time snaps once the shelf has been still for
    /// [`SNAP_DELAY`] with no mouse button down, and only if no move came
    /// after the one it waited for.
    #[derive(Debug, Default)]
    pub(super) struct SnapTimer {
        last_move: Mutex<LastMove>,
        /// A thread is waiting to snap.
        armed: AtomicBool,
    }

    #[derive(Debug, Default, Clone, Copy)]
    struct LastMove {
        generation: u64,
        at: Option<Instant>,
    }

    impl SnapTimer {
        fn last(&self) -> LastMove {
            *self
                .last_move
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        }

        fn generation(&self) -> u64 {
            self.last().generation
        }
    }

    /// What the snap thread does next.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Next {
        /// Sleep this long, then look again.
        Wait(Duration),
        /// Snap now.
        Snap,
    }

    /// The snap is due [`SNAP_DELAY`] after the last move, and only once no
    /// mouse button is down (the user may still be holding the shelf).
    fn next(last_move: Option<Instant>, now: Instant, button_down: bool) -> Next {
        if let Some(at) = last_move {
            let due = at + SNAP_DELAY;
            if now < due {
                return Next::Wait(due - now);
            }
        }
        if button_down {
            Next::Wait(BUTTON_POLL)
        } else {
            Next::Snap
        }
    }

    /// The shelf moved (the user, or Grape placing it): snap it once it's
    /// still. Runs on the main thread, so it only records the move.
    pub(super) fn moved(app: &AppHandle) {
        let timer = Arc::clone(&app.state::<Shelf>().snap);
        {
            let mut last = timer
                .last_move
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            last.generation = last.generation.wrapping_add(1);
            last.at = Some(Instant::now());
        }
        if timer.armed.swap(true, Ordering::AcqRel) {
            // The waiting thread sees this move.
            return;
        }
        let app = app.clone();
        let for_thread = Arc::clone(&timer);
        let spawned = thread::Builder::new()
            .name("grape-float-snap".to_owned())
            .spawn(move || wait_and_snap(&app, &for_thread));
        if let Err(err) = spawned {
            log::error!("float: could not start the snap timer: {err}");
            timer.armed.store(false, Ordering::Release);
        }
    }

    fn wait_and_snap(app: &AppHandle, timer: &SnapTimer) {
        loop {
            let last = timer.last();
            match next(last.at, Instant::now(), native::button_down()) {
                Next::Wait(wait) => thread::sleep(wait),
                Next::Snap => {
                    queue_snap(app, last.generation);
                    timer.armed.store(false, Ordering::Release);
                    // A move that came meanwhile found the timer armed and
                    // left it to this thread, unless another one took over.
                    if timer.generation() == last.generation
                        || timer.armed.swap(true, Ordering::AcqRel)
                    {
                        return;
                    }
                }
            }
        }
    }

    /// Snaps the shelf on the main thread if nothing moved it after move
    /// `generation`. A snap that changes nothing doesn't touch the window,
    /// so it can't set off another one.
    fn queue_snap(app: &AppHandle, generation: u64) {
        let app_main = app.clone();
        let queued = app.run_on_main_thread(move || {
            if app_main.state::<Shelf>().snap.generation() != generation {
                // Moved again: a later timer snaps.
                return;
            }
            if let Err(err) = snap_here(&app_main) {
                log::warn!("float: could not snap the shelf: {err:#}");
            }
        });
        if let Err(err) = queued {
            log::error!("float: could not reach the main thread to snap: {err}");
        }
    }

    fn snap_here(app: &AppHandle) -> anyhow::Result<()> {
        let Some(window) = app.get_webview_window(FLOAT) else {
            return Ok(());
        };
        let shown = window
            .is_visible()
            .context("could not ask if the shelf is open")?;
        if !shown || app.state::<Shelf>().lock().items.is_empty() {
            return Ok(());
        }
        // Minimizing moves the window off screen: snapping it from there
        // would bring it back somewhere else. The next Float places it.
        if minimized(&window)? {
            return Ok(());
        }
        let frame = native::frame_of(&window)?;
        let Some(display) = display_under(frame) else {
            return Ok(());
        };
        let snapped = snap(frame, display.work_area, display.scale);
        if snapped == frame {
            return Ok(());
        }
        native::move_to(app, &window, snapped)?;
        log::info!("float: shelf snapped");
        Ok(())
    }

    /// The Windows calls (D21): the shelf is a tool window that a click
    /// activates, shown topmost without activation, and it refuses drops.
    #[cfg(target_os = "windows")]
    mod native {
        use std::path::PathBuf;

        use anyhow::Context as _;
        use grape_platform::windows as platform;
        use grape_platform::Rect;
        use tauri::{AppHandle, Manager, WebviewWindow};

        use super::super::{drag_ended, outcome, publish, FLOAT};
        use crate::events::FloatOutcome;
        use crate::ui_windows::refuse_drops_here;

        /// The window's raw handle.
        fn hwnd(window: &WebviewWindow) -> anyhow::Result<isize> {
            Ok(window.hwnd().context("the shelf has no window handle")?.0 as isize)
        }

        /// Hardens the new window (tool window: no taskbar button, not in
        /// Alt-Tab; still activatable) and puts the refusing drop target on
        /// it. On the main thread.
        pub(super) fn prepare(window: &WebviewWindow) {
            let hwnd = match window.hwnd() {
                Ok(hwnd) => hwnd.0 as isize,
                Err(err) => {
                    log::error!("float: no window handle: {err}");
                    return;
                }
            };
            #[cfg(feature = "overlay-subclass")]
            match platform::harden_shelf(hwnd) {
                Ok(()) => log::info!("float: style subclass installed"),
                Err(err) => log::error!("float: could not install the style subclass: {err}"),
            }
            #[cfg(not(feature = "overlay-subclass"))]
            log::warn!("float: the overlay-subclass feature is off; tao's styles only");
            match platform::ex_style(hwnd) {
                Ok(style) => log::info!(
                    "float ex-style (built): {}",
                    platform::describe_ex_style(style)
                ),
                Err(err) => log::warn!("float ex-style (built): unreadable: {err}"),
            }
            refuse_drops_here(window);
        }

        /// Shows the shelf at `frame` (physical pixels) without activating
        /// it. On the main thread.
        pub(super) fn show(
            _app: &AppHandle,
            window: &WebviewWindow,
            frame: Rect,
        ) -> anyhow::Result<()> {
            let hwnd = hwnd(window)?;
            // Before every show: WebView2 may have made new child windows.
            refuse_drops_here(window);
            platform::show_shelf(hwnd, frame).context("could not show the shelf")?;
            // Landing on a monitor with another DPI makes tao rescale the
            // window it just moved (WM_DPICHANGED), as for the overlay: set
            // the frame again, now that the DPI is the monitor's.
            if frame_of(window)? != frame {
                platform::show_shelf(hwnd, frame).context("could not size the shelf")?;
            }
            Ok(())
        }

        /// Moves an open shelf to `frame` (the snap). On the main thread.
        pub(super) fn move_to(
            _app: &AppHandle,
            window: &WebviewWindow,
            frame: Rect,
        ) -> anyhow::Result<()> {
            let hwnd = hwnd(window)?;
            platform::show_shelf(hwnd, frame).context("could not move the shelf")?;
            Ok(())
        }

        /// Hides the shelf. On the main thread.
        pub(super) fn hide(_app: &AppHandle, window: &WebviewWindow) -> anyhow::Result<()> {
            let hwnd = hwnd(window)?;
            platform::hide_shelf(hwnd).context("could not hide the shelf")?;
            Ok(())
        }

        /// The window's frame now, in global physical pixels.
        pub(super) fn frame_of(window: &WebviewWindow) -> anyhow::Result<Rect> {
            let position = window
                .outer_position()
                .context("could not read where the shelf is")?;
            let size = window
                .outer_size()
                .context("could not read the shelf's size")?;
            Ok(Rect {
                x: position.x,
                y: position.y,
                width: i32::try_from(size.width).unwrap_or(i32::MAX),
                height: i32::try_from(size.height).unwrap_or(i32::MAX),
            })
        }

        /// Whether a mouse button is down. Any thread.
        pub(super) fn button_down() -> bool {
            platform::primary_button_down()
        }

        /// Queues the drag of `paths` on the main thread, which owns the
        /// window and tao's OLE apartment, and tells the page when it's
        /// over. Tauri's other main-thread work waits for the drag.
        pub(in super::super) fn drag_out(app: &AppHandle, paths: Vec<PathBuf>) {
            let Some(window) = app.get_webview_window(FLOAT) else {
                log::error!("float: drag out without a shelf window");
                drag_ended(app, FloatOutcome::Failed);
                return;
            };
            let app_main = app.clone();
            let queued = app.run_on_main_thread(move || {
                let (outcome, skipped) = drag_here(&window, &paths);
                if skipped > 0 {
                    // They went missing since the view was built: show it,
                    // off this thread (the view looks at the disk).
                    let app = app_main.clone();
                    tauri::async_runtime::spawn_blocking(move || publish(&app));
                }
                drag_ended(&app_main, outcome);
            });
            if let Err(err) = queued {
                log::error!("float: could not reach the main thread for a drag: {err}");
                drag_ended(app, FloatOutcome::Failed);
            }
        }

        /// Runs the drag on this, the main thread. Returns how it ended and
        /// how many items were skipped as missing.
        fn drag_here(window: &WebviewWindow, paths: &[PathBuf]) -> (FloatOutcome, usize) {
            let hwnd = match window.hwnd() {
                Ok(hwnd) => hwnd.0 as isize,
                Err(err) => {
                    log::error!("float: drag out without a window handle: {err}");
                    return (FloatOutcome::Failed, 0);
                }
            };
            match platform::drag_out(hwnd, paths) {
                Ok(drag) => {
                    log::info!(
                        "float: drag out of {} item(s): {:?}, {} missing",
                        paths.len(),
                        drag.outcome,
                        drag.skipped.len()
                    );
                    (outcome(drag.outcome), drag.skipped.len())
                }
                Err(err) => {
                    log::error!("float: drag out failed: {err}");
                    (FloatOutcome::Failed, 0)
                }
            }
        }
    }

    /// The macOS calls (D43): level 3, never key or activating on show,
    /// activation handed back on hide, the frame in points, and AppKit's
    /// dragging session for the drag out.
    #[cfg(target_os = "macos")]
    mod native {
        use std::ffi::c_void;
        use std::sync::PoisonError;

        use anyhow::Context as _;
        use grape_platform::macos::{self as platform, DragStart, WindowRole};
        use grape_platform::{DragItem, DragOutcome, Rect};
        use tauri::{AppHandle, Manager, WebviewWindow};

        use super::super::{drag_ended, outcome, Shelf, FLOAT};
        use crate::events::FloatOutcome;

        /// The window's `NSWindow` pointer. Only good on the main thread
        /// until the autorelease pool drains: use it at once.
        fn ns_window(window: &WebviewWindow) -> anyhow::Result<*mut c_void> {
            window.ns_window().context("the shelf has no NSWindow")
        }

        /// Hardens the new window: the floating level (3), under every
        /// other Grape window; on every Space and over full-screen apps,
        /// out of Cmd-`; not hidden when Grape isn't active; and the
        /// `Closable` bit, so Cmd+W is a close request. On the main thread.
        pub(super) fn prepare(window: &WebviewWindow) {
            let ns_window = match ns_window(window) {
                Ok(ns_window) => ns_window,
                Err(err) => {
                    log::error!("float: {err:#}");
                    return;
                }
            };
            // SAFETY: this runs on the main thread, and `ns_window` is the
            // shelf's NSWindow, which Tauri just returned and keeps alive
            // while `window` is.
            match unsafe { platform::harden_window(ns_window, WindowRole::Shelf) } {
                // SAFETY: as above.
                Ok(()) => log::info!("float: {}", unsafe { platform::describe_window(ns_window) }),
                Err(err) => log::error!("float: could not harden the shelf: {err}"),
            }
        }

        /// Shows the shelf at `frame` (points) without making it key or
        /// activating Grape, and remembers the app in front, the hide's
        /// fallback: a show while Grape is in front keeps the earlier one.
        /// On the main thread.
        pub(super) fn show(
            app: &AppHandle,
            window: &WebviewWindow,
            frame: Rect,
        ) -> anyhow::Result<()> {
            let ns_window = ns_window(window)?;
            // SAFETY: as in `prepare`.
            let shown = unsafe { platform::show_shelf(ns_window, frame) }
                .context("could not show the shelf")?;
            let shelf = app.state::<Shelf>();
            let mut back_to = shelf.back_to.lock().unwrap_or_else(PoisonError::into_inner);
            *back_to = shown.front_pid.or(*back_to);
            Ok(())
        }

        /// Moves an open shelf to `frame` (the snap): the same as a show.
        pub(super) fn move_to(
            app: &AppHandle,
            window: &WebviewWindow,
            frame: Rect,
        ) -> anyhow::Result<()> {
            show(app, window, frame).context("could not move the shelf")
        }

        /// Hides the shelf and, if a click had made it key, hands
        /// activation back to the app the user was last in, or else to the
        /// one recorded at the last show. On the main thread.
        pub(super) fn hide(app: &AppHandle, window: &WebviewWindow) -> anyhow::Result<()> {
            let ns_window = ns_window(window)?;
            let back_to = app
                .state::<Shelf>()
                .back_to
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            // SAFETY: as in `prepare`.
            let handed = unsafe { platform::hide_shelf(ns_window, back_to) }
                .context("could not hide the shelf")?;
            log::info!("float: activation handed back: {handed}");
            Ok(())
        }

        /// The window's frame now, in Grape's top-left points: never
        /// Tauri's `outer_position` / `outer_size`, which are backing
        /// pixels on the Mac. On the main thread.
        pub(super) fn frame_of(window: &WebviewWindow) -> anyhow::Result<Rect> {
            let ns_window = ns_window(window)?;
            // SAFETY: as in `prepare`.
            unsafe { platform::window_frame(ns_window) }
                .context("could not read where the shelf is")
        }

        /// Whether a mouse button is down. Any thread.
        pub(super) fn button_down() -> bool {
            platform::mouse_button_down()
        }

        /// Queues the drag of `items` on the main thread. AppKit's session
        /// returns at once and doesn't hold the main thread: the page hears
        /// `float://drag-ended` from the source's end callback, or at once
        /// when no drag started. Logs counts and outcomes, never a path or
        /// a name. No Esc watchdog: Esc is AppKit's own.
        pub(in super::super) fn drag_out(app: &AppHandle, items: Vec<DragItem>) {
            let Some(window) = app.get_webview_window(FLOAT) else {
                log::error!("float: drag out without a shelf window");
                drag_ended(app, FloatOutcome::Failed);
                return;
            };
            let app_main = app.clone();
            let queued = app.run_on_main_thread(move || {
                let count = items.len();
                let ns_window = match ns_window(&window) {
                    Ok(ns_window) => ns_window,
                    Err(err) => {
                        log::error!("float: drag out: {err:#}");
                        drag_ended(&app_main, FloatOutcome::Failed);
                        return;
                    }
                };
                let app_end = app_main.clone();
                let ended = move |drag: DragOutcome| {
                    log::info!("float: drag out of {count} item(s): {drag:?}");
                    drag_ended(&app_end, outcome(drag));
                };
                // SAFETY: as in `prepare`.
                match unsafe { platform::drag_out(ns_window, &items, ended) } {
                    Ok(DragStart::Started) => {
                        log::info!("float: drag out of {count} item(s) started");
                    }
                    Ok(DragStart::NotStarted(drag)) => {
                        log::info!("float: drag out of {count} item(s): {drag:?}");
                        drag_ended(&app_main, outcome(drag));
                    }
                    Err(err) => {
                        log::error!("float: drag out failed: {err}");
                        drag_ended(&app_main, FloatOutcome::Failed);
                    }
                }
            });
            if let Err(err) = queued {
                log::error!("float: could not reach the main thread for a drag: {err}");
                drag_ended(app, FloatOutcome::Failed);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_snap_waits_for_stillness_and_a_released_button() {
            let moved = Instant::now();
            assert_eq!(
                next(Some(moved), moved + Duration::from_millis(100), false),
                Next::Wait(Duration::from_millis(200))
            );
            assert_eq!(next(Some(moved), moved + SNAP_DELAY, false), Next::Snap);
            assert_eq!(
                next(Some(moved), moved + Duration::from_secs(2), true),
                Next::Wait(BUTTON_POLL)
            );
            assert_eq!(next(None, moved, false), Next::Snap);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(paths: &[&str]) -> Vec<SelectionItem> {
        paths
            .iter()
            .map(|path| SelectionItem::file(*path))
            .collect()
    }

    fn names(model: &Model) -> Vec<String> {
        model
            .view(|_| true)
            .items
            .into_iter()
            .map(|item| item.name)
            .collect()
    }

    fn display(scale: f64) -> DisplayBounds {
        DisplayBounds {
            monitor: Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            work_area: Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1032,
            },
            scale,
        }
    }

    #[test]
    fn items_go_after_whats_there_in_drop_order() {
        let mut model = Model::default();
        let first = model.add(&items(&["/a/one.txt", "/b/two.txt"]));
        assert_eq!(
            first,
            Added {
                added: 2,
                already: 0,
                left_out: 0
            }
        );
        model.add(&items(&["/c/three.txt"]));
        assert_eq!(names(&model), ["one.txt", "two.txt", "three.txt"]);
        let ids: Vec<u64> = model.items.iter().map(|item| item.id).collect();
        assert_eq!(ids, [1, 2, 3]);
    }

    #[test]
    fn a_path_on_the_shelf_isnt_added_twice() {
        let mut model = Model::default();
        model.add(&items(&["/a/one.txt"]));
        let again = model.add(&items(&["/a/one.txt", "/a/two.txt", "/a/two.txt"]));
        assert_eq!(
            again,
            Added {
                added: 1,
                already: 2,
                left_out: 0
            }
        );
        assert_eq!(names(&model), ["one.txt", "two.txt"]);
    }

    #[test]
    fn the_shelf_holds_fifty() {
        let mut model = Model::default();
        let many: Vec<String> = (0..60).map(|n| format!("/a/{n}.txt")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let added = model.add(&items(&many));
        assert_eq!(
            added,
            Added {
                added: CAPACITY,
                already: 0,
                left_out: 10
            }
        );
        assert_eq!(model.items.len(), CAPACITY);
        assert_eq!(model.items.last().map(|item| item.id), Some(50));
        // Already there beats full.
        assert_eq!(model.add(&items(&["/a/0.txt"])).already, 1);
        assert_eq!(model.add(&items(&["/a/new.txt"])).left_out, 1);
    }

    #[test]
    fn removing_and_clearing_take_items_off_and_ids_stay_fresh() {
        let mut model = Model::default();
        model.add(&items(&["/a/one.txt", "/a/two.txt"]));
        assert!(model.remove(1));
        assert!(!model.remove(1));
        assert!(!model.remove(99));
        assert_eq!(names(&model), ["two.txt"]);
        model.clear();
        assert!(model.items.is_empty());
        model.add(&items(&["/a/one.txt"]));
        assert_eq!(model.items[0].id, 3);
    }

    #[test]
    fn drag_items_come_in_shelf_order_and_unknown_ids_are_ignored() {
        let mut model = Model::default();
        model.add(&[
            SelectionItem::file("/a/one.txt"),
            SelectionItem::folder("/b/Invoices"),
            SelectionItem::file("/c/three.txt"),
        ]);
        assert_eq!(
            model.drag_items(&[3, 1, 42, 1]),
            [
                DragItem {
                    path: PathBuf::from("/a/one.txt"),
                    folder: false
                },
                DragItem {
                    path: PathBuf::from("/c/three.txt"),
                    folder: false
                },
            ]
        );
        assert_eq!(
            model.drag_items(&[2]),
            [DragItem {
                path: PathBuf::from("/b/Invoices"),
                folder: true
            }]
        );
        assert!(model.drag_items(&[7]).is_empty());
        assert_eq!(model.path(2), Some(PathBuf::from("/b/Invoices")));
        assert_eq!(model.path(9), None);
    }

    #[test]
    fn drag_outcomes_map_to_the_pages_words() {
        assert_eq!(outcome(DragOutcome::Copied), FloatOutcome::Copied);
        assert_eq!(outcome(DragOutcome::Linked), FloatOutcome::Linked);
        assert_eq!(outcome(DragOutcome::Cancelled), FloatOutcome::Cancelled);
        assert_eq!(outcome(DragOutcome::ButtonUp), FloatOutcome::Nothing);
        assert_eq!(outcome(DragOutcome::NothingToDrag), FloatOutcome::Nothing);
    }

    #[test]
    fn the_view_names_items_and_marks_the_missing_ones() {
        let mut model = Model::default();
        model.add(&[
            SelectionItem::file("/a/photo.jpg"),
            SelectionItem::folder("/a/Invoices"),
            SelectionItem::file("/a/notes.txt"),
        ]);
        let view = model.view(|path| path != Path::new("/a/notes.txt"));
        assert_eq!(
            view,
            FloatView {
                items: vec![
                    FloatItem {
                        id: 1,
                        name: "photo.jpg".to_owned(),
                        kind: FloatKind::File,
                        missing: false
                    },
                    FloatItem {
                        id: 2,
                        name: "Invoices".to_owned(),
                        kind: FloatKind::Folder,
                        missing: false
                    },
                    FloatItem {
                        id: 3,
                        name: "notes.txt".to_owned(),
                        kind: FloatKind::File,
                        missing: true
                    },
                ]
            }
        );
        assert_eq!(Model::default().view(|_| true), FloatView { items: vec![] });
    }

    #[test]
    fn a_drag_out_carries_only_the_items_still_there() {
        let dir = std::env::temp_dir().join(format!("grape-float-drag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp folder");
        let here = dir.join("here.txt");
        std::fs::write(&here, b"x").expect("a temp file");
        let gone = dir.join("gone.txt");
        assert_eq!(
            draggable(vec![gone.clone(), here.clone(), dir.clone()]),
            [here, dir.clone()]
        );
        assert!(draggable(vec![gone]).is_empty());
        std::fs::remove_dir_all(&dir).expect("the temp folder removed");
    }

    #[test]
    fn names_never_carry_the_folder() {
        assert_eq!(display_name(Path::new("/a/b/photo.jpg")), "photo.jpg");
        assert_eq!(display_name(Path::new("/a/Invoices/")), "Invoices");
        assert_eq!(display_name(Path::new("/")), "/");
    }

    #[cfg(unix)]
    #[test]
    fn a_name_that_isnt_unicode_is_shown_lossily() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new("/a").join(OsStr::from_bytes(b"caf\xe9.txt"));
        assert_eq!(display_name(&path), "caf\u{fffd}.txt");
    }

    #[cfg(windows)]
    #[test]
    fn a_drive_or_share_root_is_named_without_its_root() {
        assert_eq!(display_name(Path::new(r"D:\")), "D:");
        assert_eq!(
            display_name(Path::new(r"\\server\share\")),
            r"\\server\share"
        );
        assert_eq!(display_name(Path::new(r"C:\Users\max\a.png")), "a.png");
    }

    #[test]
    fn the_height_follows_the_count_up_to_six_rows() {
        assert_eq!(height(1), 12.0 + 44.0 + 40.0);
        assert_eq!(height(2), 12.0 + 44.0 + 80.0 + 40.0);
        assert_eq!(height(6), 12.0 + 44.0 + 240.0 + 40.0);
        assert_eq!(height(50), height(6));
    }

    #[test]
    fn a_closed_shelf_opens_at_the_right_edge_centered() {
        // 280 × 96 at 100 %, 12 px in from the right of the work area.
        assert_eq!(
            place_at_right_edge(&display(1.0), 1),
            Rect {
                x: 1920 - 280 - 12,
                y: (1032 - 96) / 2,
                width: 280,
                height: 96
            }
        );
        // At 150 % on a monitor to the left of the primary one.
        let mut left = display(1.5);
        left.work_area.x = -1920;
        let frame = place_at_right_edge(&left, 3);
        assert_eq!(frame.width, 420);
        assert_eq!(frame.height, ((12.0 + 44.0 + 120.0 + 40.0) * 1.5) as i32);
        assert_eq!(frame.x, -280 * 3 / 2 - 18);
    }

    #[test]
    fn an_open_shelf_grows_where_it_is_and_stays_inside() {
        let frame = Rect {
            x: 100,
            y: 200,
            width: 280,
            height: 96,
        };
        assert_eq!(
            resized(frame, &display(1.0), 2),
            Rect {
                height: 176,
                ..frame
            }
        );
        let low = Rect { y: 1000, ..frame };
        let grown = resized(low, &display(1.0), 6);
        assert_eq!(grown.y + grown.height, 1032);
    }

    #[test]
    fn a_shelf_left_near_an_edge_snaps_to_it() {
        let work = display(1.0).work_area;
        let rect = |x: i32| Rect {
            x,
            y: 300,
            width: 280,
            height: 136,
        };
        assert_eq!(snap(rect(40), work, 1.0).x, 12);
        assert_eq!(snap(rect(-90), work, 1.0).x, 12);
        assert_eq!(snap(rect(1920 - 280 - 30), work, 1.0).x, 1920 - 280 - 12);
        assert_eq!(snap(rect(1700), work, 1.0).x, 1920 - 280 - 12);
        // Out of reach: left where it is.
        assert_eq!(snap(rect(49), work, 1.0), rect(49));
        assert_eq!(snap(rect(800), work, 1.0), rect(800));
        // Reach and margin scale: 72 and 18 px at 150 %.
        assert_eq!(snap(rect(72), work, 1.5).x, 18);
        assert_eq!(snap(rect(73), work, 1.5).x, 73);
    }

    #[test]
    fn a_snap_keeps_the_shelf_inside_the_work_area() {
        let work = display(1.0).work_area;
        let off = Rect {
            x: 600,
            y: 1000,
            width: 280,
            height: 136,
        };
        assert_eq!(snap(off, work, 1.0).y, 1032 - 136);
        assert_eq!(snap(Rect { y: -50, ..off }, work, 1.0).y, 0);
    }

    #[test]
    fn snapping_a_snapped_shelf_changes_nothing() {
        // Also on work areas barely wider than the shelf, where both edges
        // are in reach.
        for width in [1920, 400, 330, 280, 200] {
            let work = Rect {
                x: -width,
                y: 0,
                width,
                height: 900,
            };
            for x in (-width - 400..400).step_by(7) {
                for scale in [1.0, 1.25, 2.0] {
                    let rect = Rect {
                        x,
                        y: 100,
                        width: 280,
                        height: 136,
                    };
                    let once = snap(rect, work, scale);
                    assert_eq!(
                        snap(once, work, scale),
                        once,
                        "{rect:?} in {work:?} at {scale}"
                    );
                }
            }
        }
    }
}
