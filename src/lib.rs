//! Local agent session explorer: projects, conversations, resume state,
//! and plugin-free live activity.
//!
//! The library exposes the building blocks (catalog parsing, SQLite store,
//! tmux discovery, TUI) so the binary stays a thin CLI wrapper and the
//! examples/benchmarks can drive the UI headlessly. No provider is ever
//! controlled from here and no task hierarchy is required.
pub mod activity;
pub mod details;
mod formats;
pub mod model;
pub mod notifications;
pub mod opencode;
pub mod paths;
pub mod pi;
pub mod presence;
pub mod service;
pub mod store;
pub mod tmux;
pub mod ui;
