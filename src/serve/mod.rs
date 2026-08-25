//! Process supervisor — orchestrates `opencode serve` + optional `rathole`.

pub mod process;
pub mod rathole;
pub mod supervisor;

pub use process::{ChildProcess, ProcessSpec};
pub use rathole::{default_bin as rathole_default_bin, default_config as rathole_default_config};
pub use supervisor::{ServeStatus, ServeSupervisor};
