//! Owned subprocess execution for fuzz tooling. Every exit path stops the process
//! group/job, drains bounded output, and reaps the direct child.

use anyhow::{bail, Context, Result};
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use std::collections::VecDeque;
use std::fmt;
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

pub(super) const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) enum OutputMode {
    Inherit,
    Capture,
}

#[derive(Debug)]
pub(super) enum Termination {
    Exited(ExitStatus),
    TimedOut,
    Interrupted(i32),
}

impl Termination {
    pub fn success(&self) -> bool {
        matches!(self, Self::Exited(status) if status.success())
    }

    pub fn code(&self) -> i32 {
        match self {
            Self::Exited(status) => {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    status
                        .code()
                        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
                }
                #[cfg(not(unix))]
                status.code().unwrap_or(1)
            }
            Self::TimedOut => 124,
            Self::Interrupted(signal) => 128 + signal,
        }
    }
}

impl fmt::Display for Termination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exited(status) => write!(f, "{status}"),
            Self::TimedOut => f.write_str("timed out"),
            Self::Interrupted(signal) => write!(f, "interrupted by signal {signal}"),
        }
    }
}

#[derive(Debug)]
pub(super) struct CommandFailure {
    label: String,
    pub termination: Termination,
}

impl fmt::Display for CommandFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.label, self.termination)
    }
}

impl std::error::Error for CommandFailure {}

#[derive(Default, Debug)]
pub(super) struct Captured {
    bytes: VecDeque<u8>,
    discarded: usize,
}

impl Captured {
    fn append(&mut self, bytes: &[u8]) {
        let overflow = self
            .bytes
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(OUTPUT_LIMIT);
        let remove = overflow.min(self.bytes.len());
        self.bytes.drain(..remove);
        self.bytes.extend(&bytes[overflow - remove..]);
        self.discarded = self.discarded.saturating_add(overflow);
    }

    pub fn complete(&self) -> Result<Vec<u8>> {
        if self.discarded != 0 {
            bail!(
                "command output exceeded {OUTPUT_LIMIT} bytes; refusing to parse truncated output"
            );
        }
        Ok(self.bytes.iter().copied().collect())
    }

    pub fn text(&self) -> String {
        let bytes: Vec<_> = self.bytes.iter().copied().collect();
        let text = String::from_utf8_lossy(&bytes);
        if self.discarded == 0 {
            text.into_owned()
        } else {
            format!(
                "[discarded {} output bytes; showing tail]\n{text}",
                self.discarded
            )
        }
    }
}

pub(super) struct Output {
    pub termination: Termination,
    pub stdout: Captured,
    pub stderr: Captured,
}

impl Output {
    pub fn success(self, label: &str) -> Result<Self> {
        if self.termination.success() {
            Ok(self)
        } else {
            self.print_diagnostics();
            Err(CommandFailure {
                label: label.to_string(),
                termination: self.termination,
            }
            .into())
        }
    }

    pub fn print_diagnostics(&self) {
        for (label, output) in [("stdout", &self.stdout), ("stderr", &self.stderr)] {
            if !output.bytes.is_empty() {
                eprintln!("--- {label} ---\n{}", output.text());
            }
        }
    }
}

struct OwnedChild {
    child: Box<dyn ChildWrapper>,
    stopped: bool,
}

impl OwnedChild {
    fn stop(&mut self) -> std::io::Result<()> {
        if !self.stopped {
            match self.child.start_kill() {
                #[cfg(unix)]
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
                result => result?,
            }
            self.stopped = true;
        }
        Ok(())
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// One runtime and cancellation listener for the entire fuzz CLI invocation,
/// including filesystem work between subprocesses.
pub(super) struct Runner {
    runtime: tokio::runtime::Runtime,
    interruption: tokio::sync::watch::Receiver<Option<i32>>,
}

impl Runner {
    pub fn new() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .context("create subprocess runtime")?;
        let mut signals = {
            let _entered = runtime.enter();
            Signals::new()?
        };
        let (sender, interruption) = tokio::sync::watch::channel(None);
        runtime.spawn(async move {
            let signal = signals.recv().await;
            let _ = sender.send(Some(signal));
        });
        Ok(Self {
            runtime,
            interruption,
        })
    }

    pub fn check_cancelled(&self) -> Result<()> {
        if let Some(signal) = *self.interruption.borrow() {
            return Err(CommandFailure {
                label: "fuzz command".to_string(),
                termination: Termination::Interrupted(signal),
            }
            .into());
        }
        Ok(())
    }

    pub fn run(
        &self,
        mut command: Command,
        timeout: Option<Duration>,
        mode: OutputMode,
    ) -> Result<Output> {
        self.check_cancelled()?;
        command.stdin(Stdio::null());
        match mode {
            OutputMode::Inherit => {
                command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
            }
            OutputMode::Capture => {
                command.stdout(Stdio::piped()).stderr(Stdio::piped());
            }
        }
        self.runtime.block_on(self.execute(command, timeout))
    }

    async fn execute(&self, command: Command, timeout: Option<Duration>) -> Result<Output> {
        let program = command.get_program().to_string_lossy().into_owned();
        let mut interruption = self.interruption.clone();
        let cancelled = async {
            *interruption
                .wait_for(|signal| signal.is_some())
                .await
                .expect("signal listener remains active until interruption")
        };
        tokio::pin!(cancelled);
        let mut wrapped = CommandWrap::from(tokio::process::Command::from(command));
        wrapped.wrap(KillOnDrop);
        #[cfg(unix)]
        wrapped.wrap(process_wrap::tokio::ProcessSession);
        #[cfg(windows)]
        wrapped.wrap(process_wrap::tokio::JobObject);
        let mut child = OwnedChild {
            child: wrapped
                .spawn()
                .with_context(|| format!("spawn {program}"))?,
            stopped: false,
        };
        let stdout = child.child.stdout().take();
        let stderr = child.child.stderr().take();
        let mut output = Output {
            termination: Termination::TimedOut,
            stdout: Captured::default(),
            stderr: Captured::default(),
        };
        let result = {
            let drain = async {
                tokio::try_join!(
                    capture(stdout, &mut output.stdout),
                    capture(stderr, &mut output.stderr)
                )?;
                Ok::<_, std::io::Error>(())
            };
            tokio::pin!(drain);
            let deadline = deadline(timeout);
            tokio::pin!(deadline);
            let mut drained = false;
            let result = loop {
                tokio::select! {
                    biased;
                    signal = &mut cancelled => break Ok(Termination::Interrupted(signal.expect("interruption received"))),
                    () = &mut deadline => break Ok(Termination::TimedOut),
                    // Wait for the launcher, not the job. Descendants can outlive
                    // their launcher and keep inherited pipes open indefinitely.
                    result = child.child.inner_mut().wait() => break result.map(Termination::Exited),
                    result = &mut drain, if !drained => {
                        drained = true;
                        if let Err(error) = result { break Err(error); }
                    }
                }
            };
            let stop = child.stop();
            let cleanup = async {
                child.child.wait().await?;
                if !drained {
                    drain.await?;
                }
                Ok::<_, std::io::Error>(())
            };
            let cleanup = tokio::time::timeout(CLEANUP_TIMEOUT, cleanup).await;
            stop.with_context(|| format!("stop {program} process tree"))?;
            cleanup
                .with_context(|| format!("cleanup {program} timed out"))?
                .with_context(|| format!("cleanup {program}"))?;
            result
        };
        output.termination = match *self.interruption.borrow() {
            Some(signal) => Termination::Interrupted(signal),
            None => result.with_context(|| format!("wait for {program}"))?,
        };
        Ok(output)
    }
}

async fn deadline(timeout: Option<Duration>) {
    match timeout {
        Some(timeout) => tokio::time::sleep(timeout).await,
        None => std::future::pending().await,
    }
}

async fn capture(
    reader: Option<impl AsyncRead + Unpin>,
    output: &mut Captured,
) -> std::io::Result<()> {
    let Some(mut reader) = reader else {
        return Ok(());
    };
    let mut bytes = [0; 8192];
    loop {
        let count = reader.read(&mut bytes).await?;
        if count == 0 {
            return Ok(());
        }
        output.append(&bytes[..count]);
    }
}

#[cfg(unix)]
struct Signals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Signals {
    fn new() -> std::io::Result<Self> {
        use tokio::signal::unix::{signal, SignalKind};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    async fn recv(&mut self) -> i32 {
        tokio::select! {
            _ = self.interrupt.recv() => libc::SIGINT,
            _ = self.terminate.recv() => libc::SIGTERM,
        }
    }
}

#[cfg(not(unix))]
struct Signals;

#[cfg(not(unix))]
impl Signals {
    fn new() -> std::io::Result<Self> {
        Ok(Self)
    }

    async fn recv(&mut self) -> i32 {
        let _ = tokio::signal::ctrl_c().await;
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_retains_a_bounded_tail_and_rejects_truncated_protocol_output() {
        let mut output = Captured::default();
        output.append(b"prefix");
        output.append(&vec![b'x'; OUTPUT_LIMIT + 3]);
        output.append(b"tail");
        assert_eq!(output.bytes.len(), OUTPUT_LIMIT);
        assert_eq!(output.discarded, 13);
        assert!(output.text().ends_with("tail"));
        assert!(output.complete().is_err());
    }

    #[test]
    fn capture_preserves_complete_binary_output() {
        let mut output = Captured::default();
        output.append(&[0, 255, 10]);
        assert_eq!(output.complete().unwrap(), vec![0, 255, 10]);
    }

    #[cfg(unix)]
    #[test]
    fn captures_both_pipes_without_deadlocking_and_preserves_exit_code() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf output; printf error >&2; exit 7"]);
        let output = Runner::new()
            .unwrap()
            .run(command, Some(QUERY_TIMEOUT), OutputMode::Capture)
            .unwrap();
        assert_eq!(output.termination.code(), 7);
        assert_eq!(output.stdout.text(), "output");
        assert_eq!(output.stderr.text(), "error");
    }

    #[cfg(unix)]
    #[test]
    fn drains_more_than_a_pipe_buffer_from_both_streams_with_bounded_memory() {
        let mut command = Command::new("sh");
        command.args(["-c", "head -c 5000000 /dev/zero; printf stdout-tail; head -c 5000000 /dev/zero >&2; printf stderr-tail >&2"]);
        let output = Runner::new()
            .unwrap()
            .run(command, Some(QUERY_TIMEOUT), OutputMode::Capture)
            .unwrap();
        assert!(output.termination.success());
        assert_eq!(output.stdout.bytes.len(), OUTPUT_LIMIT);
        assert_eq!(output.stderr.bytes.len(), OUTPUT_LIMIT);
        assert!(output.stdout.text().ends_with("stdout-tail"));
        assert!(output.stderr.text().ends_with("stderr-tail"));
        assert!(output.stdout.complete().is_err());
        assert!(output.stderr.complete().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn watchdog_terminates_a_stuck_process_and_keeps_its_output() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf started; exec sleep 60"]);
        let output = Runner::new()
            .unwrap()
            .run(
                command,
                Some(Duration::from_millis(200)),
                OutputMode::Capture,
            )
            .unwrap();
        assert!(matches!(output.termination, Termination::TimedOut));
        assert_eq!(output.termination.code(), 124);
        assert_eq!(output.stdout.text(), "started");
    }
}
