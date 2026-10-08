//! Starting the executor, whatever confines it: a command (bubblewrap on Linux, or nothing in
//! development) or the Windows AppContainer launcher, which `tokio::process::Command` cannot
//! express. Both give the same pipes, the same wait and the same kill.

use std::io;

use tokio::io::{AsyncRead, AsyncWrite};

/// How to start the executor.
pub enum Launch {
    /// A command: its standard handles are set here.
    Command(tokio::process::Command),
    /// The Windows sandbox (ADR-0045).
    #[cfg(windows)]
    AppContainer(crate::appcontainer::Spec),
}

/// How an executor ended.
#[derive(Debug)]
pub struct Exit {
    success: bool,
    text: String,
}

impl Exit {
    /// Whether it exited with success.
    pub fn success(&self) -> bool {
        self.success
    }
}

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

enum Inner {
    Tokio(tokio::process::Child),
    #[cfg(windows)]
    AppContainer(crate::appcontainer::Child),
}

/// A running executor; it is killed when dropped.
pub struct Running {
    pub stdin: Option<Box<dyn AsyncWrite + Send + Unpin>>,
    pub stdout: Option<Box<dyn AsyncRead + Send + Unpin>>,
    pub stderr: Option<Box<dyn AsyncRead + Send + Unpin>>,
    inner: Inner,
}

impl Launch {
    /// Set an environment variable of the executor.
    pub fn env(&mut self, key: &str, value: &str) {
        match self {
            Launch::Command(c) => {
                c.env(key, value);
            }
            #[cfg(windows)]
            Launch::AppContainer(s) => {
                s.env.retain(|(k, _)| !k.eq_ignore_ascii_case(key));
                s.env.push((key.to_owned(), value.into()));
            }
        }
    }

    /// Start it, with pipes for its output and, if `stdin`, for its input.
    pub fn spawn(self, stdin: bool) -> io::Result<Running> {
        match self {
            Launch::Command(mut c) => {
                use std::process::Stdio;
                c.stdin(if stdin { Stdio::piped() } else { Stdio::null() })
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);
                let mut child = c.spawn()?;
                Ok(Running {
                    stdin: child
                        .stdin
                        .take()
                        .map(|s| Box::new(s) as Box<dyn AsyncWrite + Send + Unpin>),
                    stdout: child
                        .stdout
                        .take()
                        .map(|s| Box::new(s) as Box<dyn AsyncRead + Send + Unpin>),
                    stderr: child
                        .stderr
                        .take()
                        .map(|s| Box::new(s) as Box<dyn AsyncRead + Send + Unpin>),
                    inner: Inner::Tokio(child),
                })
            }
            #[cfg(windows)]
            Launch::AppContainer(spec) => {
                let mut child = crate::appcontainer::spawn(&spec, stdin)?;
                Ok(Running {
                    stdin: child
                        .stdin
                        .take()
                        .map(|s| Box::new(s) as Box<dyn AsyncWrite + Send + Unpin>),
                    stdout: child
                        .stdout
                        .take()
                        .map(|s| Box::new(s) as Box<dyn AsyncRead + Send + Unpin>),
                    stderr: child
                        .stderr
                        .take()
                        .map(|s| Box::new(s) as Box<dyn AsyncRead + Send + Unpin>),
                    inner: Inner::AppContainer(child),
                })
            }
        }
    }
}

impl Running {
    /// Wait for the executor to end. Cancel-safe.
    pub async fn wait(&mut self) -> io::Result<Exit> {
        match &mut self.inner {
            Inner::Tokio(c) => c.wait().await.map(|s| Exit {
                success: s.success(),
                text: s.to_string(),
            }),
            #[cfg(windows)]
            Inner::AppContainer(c) => c.wait().await.map(|code| Exit {
                success: code == 0,
                text: format!("exit code: {code}"),
            }),
        }
    }

    /// Kill it (and, in the Windows sandbox, anything of its job).
    pub async fn kill(&mut self) {
        match &mut self.inner {
            Inner::Tokio(c) => {
                let _ = c.kill().await;
            }
            #[cfg(windows)]
            Inner::AppContainer(c) => c.kill(),
        }
    }
}
