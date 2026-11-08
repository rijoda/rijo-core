//! Rijo TUI — the terminal frontend of Rijo Code.
//!
//! This crate is deliberately outside `rijo-core`: the open engine must build
//! and publish without a terminal-UI toolchain. Everything here renders core
//! [`rijo_core::ui_events::UiEvent`]s and turns terminal input into answers
//! through the same one-shot channels the desktop uses.
//!
//! **The TUI is frozen** (2026-10-18). Bug fixes only; it is rebuilt after the
//! desktop workbench covers the remaining workflows.

pub mod clipboard;
pub mod tui;

pub use tui::TuiEvent;
pub use tui::{run, snapshot_frames, QUIT_REQUESTED, REQUEST_RESTART};
