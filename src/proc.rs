//! Bounded subprocess execution shared by lane's adapters (`git`, `op`).
//!
//! Extracted from the git adapter in Slice 4 so the 1Password adapter reuses the
//! identical drain/poll/kill discipline instead of growing a second copy that would
//! drift. Byte-oriented on purpose: the git wrapper decodes lossily (its stderr is
//! matched as text and non-UTF-8 is tolerated), while the secrets adapter must decode
//! STRICTLY — a lossy-decoded secret would be silent corruption.
//!
//! This module is crate-private plumbing: it never decides policy, never touches
//! `$LANE_ROOT`, and is consumed only through the adapter seams.

use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Captured result of one bounded invocation (raw bytes).
#[derive(Debug, Clone)]
pub(crate) struct ProcOutput {
    /// Exit code, or `None` when the process was terminated by a signal (including our kill).
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// The two spawn-layer failure classes. Adapters map these into their own error types.
#[derive(Debug)]
pub(crate) enum ProcError {
    /// The process could not be spawned, or an I/O error occurred while waiting.
    Spawn(io::Error),
    /// The process did not finish within the bounded wait and was killed.
    Timeout { secs: u64 },
}

/// Spawn a command, drain its pipes on threads, and wait no longer than `timeout` before
/// killing it. The drainer threads prevent a full pipe buffer from deadlocking the poll
/// loop, so even a chatty command is captured or cleanly killed.
pub(crate) fn run_bounded(mut cmd: Command, timeout: Duration) -> Result<ProcOutput, ProcError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(ProcError::Spawn)?;

    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = out_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = err_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let start = Instant::now();
    let mut backoff = Duration::from_millis(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Joining after the kill relies on the pipes closing with the child;
                    // a grandchild holding an inherited pipe could delay this, which is
                    // pathological for the tools we spawn (git and op start no long-lived
                    // detached children) and accepted as out of threat-model.
                    let _ = out_handle.join();
                    let _ = err_handle.join();
                    return Err(ProcError::Timeout {
                        secs: timeout.as_secs(),
                    });
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_handle.join();
                let _ = err_handle.join();
                return Err(ProcError::Spawn(e));
            }
        }
    };
    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    Ok(ProcOutput {
        code: status.code(),
        stdout,
        stderr,
    })
}
