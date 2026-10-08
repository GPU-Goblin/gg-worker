//! Executor sessions (ADR-0034): one sandboxed executor kept alive across the tasks of one
//! training run on a GPU engine, instead of a fresh process per task. Starting a CUDA executor
//! (loading LibTorch, creating the context) cost about 0.8 s of a 1.2 s ViT-tiny task.
//!
//! The executor restricts itself once, before reading any task (`gg-executor serve`), then takes
//! one attempt directory name per line of standard input and answers one JSON line. The worker
//! ends a session when the run changes, after a bounded number of tasks, and after any failure,
//! timeout or lost lease.

use std::sync::{Arc, Mutex};

use gg_core::{ExperimentId, ProjectId};
use serde::Deserialize;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, Lines,
};

use crate::launch::{Launch, Running};

/// What a session serves: one training run of one project.
pub type Scope = (ProjectId, ExperimentId);

/// A running executor session.
pub struct Session {
    child: Running,
    stdin: Box<dyn AsyncWrite + Send + Unpin>,
    lines: Lines<BufReader<Box<dyn AsyncRead + Send + Unpin>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    /// The run it serves.
    pub scope: Scope,
    /// Tasks sent so far.
    pub tasks: u32,
}

/// The session's answer to one task.
pub enum Answer {
    /// The report is in the bundle.
    Done,
    /// The task failed; the session is still alive but is not reused.
    Failed(String),
    /// The session ended (crash, kill); the end of its error output.
    Exited(String),
}

#[derive(Deserialize)]
struct Reply {
    ok: bool,
    #[serde(default)]
    error: String,
}

/// Bytes of the session's error output kept for the log.
const STDERR_TAIL: usize = 64 << 10;

impl Session {
    /// Start `command` (a `gg-executor serve` inside its sandbox) for `scope`.
    pub fn spawn(command: Launch, scope: Scope) -> Result<Self, String> {
        let mut child = command
            .spawn(true)
            .map_err(|e| format!("spawn executor session: {e}"))?;
        let stdin = child.stdin.take().ok_or("session stdin")?;
        let stdout = child.stdout.take().ok_or("session stdout")?;
        let mut err = child.stderr.take().ok_or("session stderr")?;
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let tail = stderr.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            while let Ok(n) = err.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                t.extend_from_slice(&buf[..n]);
                if t.len() > STDERR_TAIL {
                    let cut = t.len() - STDERR_TAIL;
                    t.drain(..cut);
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            stderr,
            scope,
            tasks: 0,
        })
    }

    /// Ask the session to run the bundle named `task` under its root.
    pub async fn send(&mut self, task: &str) -> Result<(), String> {
        let line = format!("{}\n", serde_json::json!({ "task": task }));
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("executor session: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("executor session: {e}"))?;
        self.tasks += 1;
        Ok(())
    }

    /// The answer to the last request. Cancel-safe (`select!`).
    pub async fn answer(&mut self) -> Answer {
        match self.lines.next_line().await {
            Ok(Some(line)) => match serde_json::from_str::<Reply>(&line) {
                Ok(r) if r.ok => Answer::Done,
                Ok(r) => Answer::Failed(r.error),
                Err(_) => Answer::Failed(format!(
                    "unreadable answer from the executor session: {}",
                    line.chars().take(200).collect::<String>()
                )),
            },
            _ => Answer::Exited(self.stderr_tail()),
        }
    }

    /// The end of the session's error output.
    pub fn stderr_tail(&self) -> String {
        let t = self.stderr.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&t).trim().to_owned()
    }

    /// End the session.
    pub async fn kill(mut self) {
        self.child.kill().await;
    }
}
