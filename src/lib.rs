//! `mini-oc-gui-serve` — Rust re-implementation of the `oc-serve-tui-actuator`
//! workflow using **Axum 0.7** (HTTP) and **ratatui** (TUI).
//!
//! Module map:
//! - [`account`]  — Account login config + remote user info types.
//! - [`auth`]      — HTTP Basic + Cookie session auth.
//! - [`domain`]    — `Project`, `Session`, `PathEntry`, path validation.
//! - [`error`]     — Unified [`AppError`] with `IntoResponse` mapping.
//! - [`handlers`]  — Axum router + handlers.
//! - [`storage`]   — `path-list.md` atomic cache + SilverBullet sync.
//! - [`serve`]     — `opencode serve` / `rathole` supervisor.
//! - [`upgrade`]   — `opencode upgrade` + omo update.
//! - [`ui`]        — ratatui TUI.

#![warn(rust_2018_idioms)]

pub mod account;
pub mod auth;
pub mod attach;
pub mod config;
pub mod domain;
pub mod error;
pub mod handlers;
pub mod icons;
pub mod serve;
pub mod storage;
pub mod ui;
pub mod upgrade;

pub use error::AppError;
