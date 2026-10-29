//! Windows-safe command runner.
//!
//! Every spawned process uses an explicit argument vector — never a shell string
//! — so quoting/interpolation differences across platforms simply don't exist.
//! There is no `sh -c` / `cmd /c` anywhere in PyPilot.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::process::Command;

/// Set once by the LSP entry point. See [`hide_child_consoles`].
static HIDE_CONSOLES: AtomicBool = AtomicBool::new(false);

/// Stop spawned processes from opening console windows (Windows only).
///
/// Editors start the language server without a console, so on Windows every
/// console program it spawns (python, uv, nvidia-smi) would otherwise get a
/// fresh window that flashes up and vanishes. The CLI keeps the default: there
/// the children share the user's terminal, which is what lets Ctrl+C reach them.
pub fn hide_child_consoles() {
    HIDE_CONSOLES.store(true, Ordering::Relaxed);
}

/// Captured result of running a process.
#[derive(Debug, Clone)]
pub struct Output {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    /// First non-empty line of stdout, trimmed (handy for `--version` probes).
    pub fn first_stdout_line(&self) -> Option<&str> {
        self.stdout.lines().map(str::trim).find(|l| !l.is_empty())
    }
}

/// Run `program args...` in `cwd`, capturing stdout/stderr. Returns `Err` only
/// when the process can't be spawned at all (missing binary, permissions); a
/// non-zero exit is a successful *run* with `status != Some(0)`.
pub async fn run<S, A>(program: S, args: &[A], cwd: Option<&Path>) -> crate::Result<Output>
where
    S: AsRef<OsStr>,
    A: AsRef<OsStr>,
{
    let cmd = build(program.as_ref(), args, cwd);
    collect(cmd, program.as_ref()).await
}

/// [`run`], but give up after `limit`. The child is killed rather than left
/// running, so a hung probe (a GPU driver stuck initializing, say) cannot
/// outlive the step that asked for it. A timeout is an `Err`, like a failed
/// spawn: there is no exit status to report.
pub async fn run_with_timeout<S, A>(
    program: S,
    args: &[A],
    cwd: Option<&Path>,
    limit: Duration,
) -> crate::Result<Output>
where
    S: AsRef<OsStr>,
    A: AsRef<OsStr>,
{
    let mut cmd = build(program.as_ref(), args, cwd);
    cmd.kill_on_drop(true);
    match tokio::time::timeout(limit, collect(cmd, program.as_ref())).await {
        Ok(result) => result,
        Err(_) => {
            crate::core::logs::record_step(
                &program.as_ref().to_string_lossy(),
                &args
                    .iter()
                    .map(|a| a.as_ref().to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
                Err("timed out"),
                limit,
            );
            anyhow::bail!(
                "`{}` did not finish within {}s",
                program.as_ref().to_string_lossy(),
                limit.as_secs()
            )
        }
    }
}

fn build<A: AsRef<OsStr>>(program: &OsStr, args: &[A], cwd: Option<&Path>) -> Command {
    let mut cmd = Command::new(program);
    for a in args {
        cmd.arg(a.as_ref());
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    // Keep child stdio off our own; capture instead.
    cmd.stdin(std::process::Stdio::null());
    #[cfg(windows)]
    if HIDE_CONSOLES.load(Ordering::Relaxed) {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

async fn collect(mut cmd: Command, program: &OsStr) -> crate::Result<Output> {
    let started = std::time::Instant::now();
    let result = cmd.output().await;
    record(&cmd, program, &result, started.elapsed());
    let out = result
        .map_err(|e| anyhow::anyhow!("failed to spawn `{}`: {e}", program.to_string_lossy()))?;

    Ok(Output {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Note the run in the current action's log entry ([`crate::core::logs`]).
fn record(
    cmd: &Command,
    program: &OsStr,
    result: &std::io::Result<std::process::Output>,
    elapsed: Duration,
) {
    let args: Vec<String> = cmd
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let program = program.to_string_lossy();
    match result {
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            crate::core::logs::record_step(
                &program,
                &args,
                Ok((out.status.code(), &stderr)),
                elapsed,
            );
        }
        Err(e) => crate::core::logs::record_step(
            &program,
            &args,
            Err(&format!("failed to spawn: {e}")),
            elapsed,
        ),
    }
}

/// Convenience: run a probe and return the trimmed first stdout line on success.
pub async fn probe_version<S, A>(program: S, args: &[A]) -> Option<String>
where
    S: AsRef<OsStr>,
    A: AsRef<OsStr>,
{
    let out = run(program, args, None).await.ok()?;
    if out.success() {
        out.first_stdout_line().map(|s| s.to_string())
    } else {
        None
    }
}
