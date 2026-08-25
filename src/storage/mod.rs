//! Local file cache + remote (SilverBullet-compatible) sync for `path-list.md`.
//!
//! - [`cache::FileCache`] — atomic JSON R/W with `.bak` recovery.
//! - [`remote::RemoteClient`] — `/.fs/<path>` PUT/GET with cookie-session auth.
//! - [`sync::PathListStore`] — combined local+remote store with the same
//!   merge semantics as `lib-path-list.sh::path_list_read`.

pub mod cache;
pub mod remote;
pub mod sync;

pub use sync::{PathListStore, RefreshReport};
