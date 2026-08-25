//! Domain entities shared across the application.
//!
//! Mirrors the opencode serve HTTP contract (`/project`, `/session`,
//! `/api/session`) and the on-disk `path-list.md` shape.

pub mod path_entry;
pub mod project;
pub mod session;

pub use path_entry::{PathEntry, PathValidator};
pub use project::Project;
pub use session::{CreateSessionRequest, CreateSessionResponse, Location, Session, SessionData};
