//! Process supervisor — orchestrates `opencode serve` + optional `rathole`.

pub mod process;
pub mod supervisor;

pub use process::{ChildProcess, ProcessSpec};
pub use supervisor::{ServeStatus, ServeSupervisor};
