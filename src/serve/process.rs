//! Thin async wrapper around `tokio::process::Command`.

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::error::AppError;

/// Declarative description of a child process.
#[derive(Debug, Clone)]
pub struct ProcessSpec {
    /// Executable name (resolved via `PATH`).
    pub program: String,
    /// Command-line arguments.
    pub args: Vec<String>,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
    /// Optional working directory.
    pub working_dir: Option<String>,
}

impl ProcessSpec {
    /// Create a spec for `program`.
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            working_dir: None,
        }
    }

    /// Append a single argument.
    #[must_use]
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    /// Append a slice of arguments.
    #[must_use]
    pub fn args(mut self, args: &[&str]) -> Self {
        for a in args {
            self.args.push((*a).to_string());
        }
        self
    }

    /// Set an environment variable.
    #[must_use]
    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }

    /// Set the working directory.
    #[must_use]
    pub fn cwd(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }
}

/// A spawned child process handle.
pub struct ChildProcess {
    /// The underlying tokio Child.
    pub child: Child,
    /// The spec that created this process.
    pub spec: ProcessSpec,
    /// OS pid.
    pub pid: u32,
}

impl ChildProcess {
    /// Spawn the process with stdout/stderr discarded. Returns its pid.
    ///
    /// # Errors
    /// Returns [`AppError::Io`] on `spawn` failure, or [`AppError::Internal`]
    /// if the OS did not report a pid.
    pub async fn spawn(spec: ProcessSpec) -> Result<Self, AppError> {
        let mut cmd = build_command(&spec);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = cmd.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| AppError::Internal("failed to get child pid".to_string()))?;
        Ok(Self { child, spec, pid })
    }

    /// Wait for the child to exit.
    ///
    /// # Errors
    /// Returns [`AppError::Io`] on wait failure.
    pub async fn wait(&mut self) -> Result<std::process::ExitStatus, AppError> {
        Ok(self.child.wait().await?)
    }

    /// Kill the child (SIGKILL on Unix, TerminateProcess on Windows).
    ///
    /// # Errors
    /// Returns [`AppError::Io`] on kill failure.
    pub async fn kill(&mut self) -> Result<(), AppError> {
        self.child.kill().await?;
        Ok(())
    }

    /// `true` if the child is still running.
    #[must_use]
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

/// Spawn the process with stdout/stderr piped through tracing.
///
/// Returns a [`ChildProcess`] whose `wait()` will resolve when the process exits.
#[tracing::instrument(skip(spec), fields(program = %spec.program))]
pub async fn spawn_traced(spec: ProcessSpec) -> Result<ChildProcess, AppError> {
    let program = spec.program.clone();
    let mut cmd = build_command(&spec);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| AppError::Internal("failed to get child pid".to_string()))?;

    if let Some(stdout) = child.stdout.take() {
        let label = program.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target: "child", program = %label, "{line}");
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let label = program;
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(target: "child", program = %label, "{line}");
            }
        });
    }

    Ok(ChildProcess { child, spec, pid })
}

fn build_command(spec: &ProcessSpec) -> Command {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    if let Some(dir) = &spec.working_dir {
        cmd.current_dir(dir);
    }
    cmd
}
