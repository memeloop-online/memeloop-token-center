use std::{ffi::OsString, path::PathBuf, process::Stdio, time::Duration};

use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    #[error("native runtime configuration is invalid")]
    Configuration,
    #[error("native runtime could not start")]
    Spawn,
    #[error("native runtime I/O failed")]
    Io,
    #[error("native runtime exceeded its output limit")]
    OutputLimit,
    #[error("native runtime timed out")]
    Timeout,
    #[error("native runtime failed")]
    Failed,
    #[error("native runtime returned an invalid response")]
    Protocol,
    #[error("native runtime does not supply authoritative usage")]
    UsageUnavailable,
}

/// Server-created command only: NEVER deserialize this from a client.
/// Paths must already be provisioned/validated by the sealed-state worker.
/// Running the worker in an OS/container sandbox is a separate required gate.
pub struct RuntimeCommand {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub home: PathBuf,
    pub workspace: PathBuf,
    pub input: Vec<u8>,
    pub timeout: Duration,
}

pub struct RuntimeOutput {
    pub stdout: Vec<u8>,
}

const STDOUT_LIMIT: usize = 4 * 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;
const INPUT_LIMIT: usize = 1024 * 1024;

struct ProcessGroup(Pid);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // Group is created with process_group(0), never inherited. It is killed
        // while the group leader remains unreaped, preventing PID reuse.
        let _ = kill_process_group(self.0, Signal::KILL);
    }
}

async fn bounded_read(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<Vec<u8>, RuntimeError> {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let count = reader
            .read(&mut chunk)
            .await
            .map_err(|_| RuntimeError::Io)?;
        if count == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(count) > limit {
            return Err(RuntimeError::OutputLimit);
        }
        output.extend_from_slice(&chunk[..count]);
    }
}

/// Cancellation drops the group guard (SIGKILL) and kill_on_drop child.
/// Explicit timeout/error paths also reap the child. No stderr, argv, prompt,
/// account path or OS error is included in a returned error.
pub async fn run(spec: RuntimeCommand) -> Result<RuntimeOutput, RuntimeError> {
    if !spec.executable.is_absolute()
        || !spec.home.is_absolute()
        || !spec.workspace.is_absolute()
        || spec.home == spec.workspace
        || spec.input.len() > INPUT_LIMIT
        || spec.timeout.is_zero()
        || spec.timeout > Duration::from_secs(180)
    {
        return Err(RuntimeError::Configuration);
    }
    let mut command = tokio::process::Command::new(&spec.executable);
    command
        .args(&spec.arguments)
        .env_clear()
        .env("HOME", &spec.home)
        .env("NO_OPEN_BROWSER", "1")
        .env("CURSOR_INVOKED_AS", "cursor-agent")
        .env("COPILOT_AUTO_UPDATE", "false")
        .current_dir(&spec.workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn().map_err(|_| RuntimeError::Spawn)?;
    let raw_pid = child.id().ok_or(RuntimeError::Spawn)?;
    let pid = i32::try_from(raw_pid)
        .ok()
        .filter(|pid| *pid > 1)
        .and_then(Pid::from_raw)
        .ok_or(RuntimeError::Spawn)?;
    let group = ProcessGroup(pid);
    let mut stdin = child.stdin.take().ok_or(RuntimeError::Io)?;
    let stdout = child.stdout.take().ok_or(RuntimeError::Io)?;
    let stderr = child.stderr.take().ok_or(RuntimeError::Io)?;
    let operation = async {
        let (output, _, _) = tokio::try_join!(
            bounded_read(stdout, STDOUT_LIMIT),
            bounded_read(stderr, STDERR_LIMIT),
            async {
                stdin
                    .write_all(&spec.input)
                    .await
                    .map_err(|_| RuntimeError::Io)?;
                stdin.shutdown().await.map_err(|_| RuntimeError::Io)?;
                drop(stdin);
                Ok::<_, RuntimeError>(())
            }
        )?;
        // Observe exit without reaping: group PID cannot be recycled before
        // the group guard kills any descendants and child.wait reaps it.
        loop {
            if waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
            )
            .map_err(|_| RuntimeError::Io)?
            .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok::<_, RuntimeError>(output)
    };
    let result = tokio::time::timeout(spec.timeout, operation).await;
    // Kill descendants BEFORE reaping the leader, even after successful EOF.
    // CLI closure of both streams is its completion boundary.
    let result = match result {
        Ok(Ok(stdout)) => {
            drop(group);
            let status = child.wait().await.map_err(|_| RuntimeError::Io)?;
            if status.success() {
                Ok(RuntimeOutput { stdout })
            } else {
                Err(RuntimeError::Failed)
            }
        }
        other => {
            drop(group);
            let _ = child.wait().await;
            match other {
                Err(_) => Err(RuntimeError::Timeout),
                Ok(Err(error)) => Err(error),
                Ok(Ok(_)) => unreachable!(),
            }
        }
    };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(
        binary: &str,
        args: &[&str],
        home: &std::path::Path,
        workspace: &std::path::Path,
    ) -> RuntimeCommand {
        RuntimeCommand {
            executable: binary.into(),
            arguments: args.iter().map(OsString::from).collect(),
            home: home.into(),
            workspace: workspace.into(),
            input: vec![],
            timeout: Duration::from_secs(2),
        }
    }

    // CI-only mock processes; never invoke an installed supplier runtime.
    #[tokio::test]
    async fn subprocess_stdin_is_bounded_and_success_is_reaped() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut spec = fixture("/bin/cat", &[], home.path(), workspace.path());
        spec.input = b"fixture-only".to_vec();
        assert_eq!(run(spec).await.unwrap().stdout, b"fixture-only");
    }

    #[tokio::test]
    async fn subprocess_timeout_and_stderr_are_redacted() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut spec = fixture("/bin/sleep", &["10"], home.path(), workspace.path());
        spec.timeout = Duration::from_millis(30);
        assert!(matches!(run(spec).await, Err(RuntimeError::Timeout)));
        let spec = fixture(
            "/bin/ls",
            &["/nonexistent-native-runtime-fixture-secret"],
            home.path(),
            workspace.path(),
        );
        let error = run(spec).await.err().unwrap();
        assert_eq!(error.to_string(), "native runtime failed");
    }

    #[tokio::test]
    async fn subprocess_does_not_inherit_host_environment() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let output = run(fixture("/usr/bin/env", &[], home.path(), workspace.path()))
            .await
            .unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        let names: Vec<_> = text
            .lines()
            .map(|line| line.split('=').next().unwrap())
            .collect();
        assert_eq!(names.len(), 4);
        for name in names {
            assert!(
                [
                    "HOME",
                    "NO_OPEN_BROWSER",
                    "CURSOR_INVOKED_AS",
                    "COPILOT_AUTO_UPDATE"
                ]
                .contains(&name)
            );
        }
    }

    #[tokio::test]
    async fn bounded_reader_accepts_exact_limit_and_rejects_one_more() {
        assert_eq!(bounded_read(&b"abc"[..], 3).await.unwrap(), b"abc");
        assert_eq!(
            bounded_read(&b"abcd"[..], 3).await.unwrap_err(),
            RuntimeError::OutputLimit
        );
    }

    #[tokio::test]
    async fn rejects_relative_runtime_without_spawning() {
        let result = run(RuntimeCommand {
            executable: "do-not-run".into(),
            arguments: vec![],
            home: "/state/account".into(),
            workspace: "/workspace/request".into(),
            input: vec![],
            timeout: Duration::from_secs(1),
        })
        .await;
        assert!(matches!(result, Err(RuntimeError::Configuration)));
    }
}
