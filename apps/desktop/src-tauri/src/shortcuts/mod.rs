//! The Shortcuts wheel (`docs/design/m10-shortcuts-wheel.md`): a wheel the
//! user opens with a hot key, whose segments are shortcuts made in
//! Settings.
//!
//! - [`model`] is the pure part: menus, shortcuts, steps, their limits and
//!   validation, which menu shows for which app, the text variables, the
//!   stored form and the Settings view.
//! - [`store`] reads and writes `shortcuts.json` in the app data folder.
//! - [`session`] is the open wheel's state, driven by the controller.
//! - [`run`] runs a picked shortcut's steps in order.
//!
//! Names, text, scripts, links, paths and app ids are the user's: none of
//! them is ever logged, and every type that holds one prints kinds and
//! counts only.

pub mod model;
pub mod run;
pub mod session;
pub mod store;
