// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use std::fmt;
use std::io::{self, PipeReader, PipeWriter, Read};
use std::sync::Arc;
#[cfg(any(feature = "agent", feature = "testing"))]
use std::sync::{Condvar, Mutex, PoisonError};
use std::thread;
#[cfg(any(feature = "agent", feature = "testing"))]
use std::time::Instant;

use crate::backend::ExecControl;
use crate::error::{Error, Result};

#[cfg(any(feature = "agent", feature = "testing"))]
#[derive(Default)]
enum CompletionState {
    #[default]
    Running,
    /// The outcome cannot arrive as things stand, as when the execution's cancellation failed:
    /// `error` becomes the outcome unless the real one arrives before `until`.
    #[cfg_attr(not(feature = "agent"), allow(dead_code))]
    Unresolved {
        error: Error,
        until: Instant,
    },
    Finished(Result<ExecOutcome>),
    Collected,
}

#[cfg(any(feature = "agent", feature = "testing"))]
#[derive(Default)]
pub(crate) struct Completion {
    state: Mutex<CompletionState>,
    finished: Condvar,
}

#[cfg(any(feature = "agent", feature = "testing"))]
impl Completion {
    /// Discards an uncollected outcome, returning it so its owner can report it elsewhere.
    #[cfg(feature = "agent")]
    pub(crate) fn discard(&self) -> Option<Result<ExecOutcome>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match std::mem::replace(&mut *state, CompletionState::Collected) {
            CompletionState::Finished(outcome) => Some(outcome),
            _ => None,
        }
    }

    /// Publishes the outcome. An outcome that the waiter already collected stays collected, and
    /// `outcome` is returned instead, so that the caller can report it another way.
    pub(crate) fn finish(&self, outcome: Result<ExecOutcome>) -> Option<Result<ExecOutcome>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let unpublished = if matches!(*state, CompletionState::Collected) {
            Some(outcome)
        } else {
            *state = CompletionState::Finished(outcome);
            None
        };
        drop(state);
        self.finished.notify_all();
        unpublished
    }

    /// Lets a waiter collect `error` if the outcome has not arrived by `until`. Does nothing once
    /// the outcome has arrived or another error is pending.
    #[cfg(feature = "agent")]
    pub(crate) fn unresolve(&self, error: Error, until: Instant) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(*state, CompletionState::Running) {
            *state = CompletionState::Unresolved { error, until };
        }
        drop(state);
        self.finished.notify_all();
    }

    pub(crate) fn wait(&self) -> Result<ExecOutcome> {
        loop {
            if let Some(outcome) = self.collect(None) {
                return outcome;
            }
        }
    }

    /// Returns what [`wait`](Self::wait) returns, or `None` if the outcome has not arrived by
    /// `deadline`.
    #[cfg(feature = "agent")]
    pub(crate) fn wait_until(&self, deadline: Instant) -> Option<Result<ExecOutcome>> {
        self.collect(Some(deadline))
    }

    /// Collects the outcome, waiting for it until `deadline` if there is one.
    fn collect(&self, deadline: Option<Instant>) -> Option<Result<ExecOutcome>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            let now = Instant::now();
            let until = match &*state {
                CompletionState::Running => deadline,
                CompletionState::Unresolved { until, .. } if *until > now => {
                    Some(deadline.map_or(*until, |deadline| deadline.min(*until)))
                }
                CompletionState::Unresolved { .. } | CompletionState::Finished(_) => {
                    return Some(
                        match std::mem::replace(&mut *state, CompletionState::Collected) {
                            CompletionState::Unresolved { error, .. } => Err(error),
                            CompletionState::Finished(outcome) => outcome,
                            CompletionState::Running | CompletionState::Collected => {
                                unreachable!("matched above")
                            }
                        },
                    );
                }
                CompletionState::Collected => {
                    return Some(Err(Error::backend_error(
                        "the execution outcome was already collected",
                    )));
                }
            };
            state = match until {
                None => self
                    .finished
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
                Some(until) => {
                    if deadline.is_some_and(|deadline| deadline <= now) {
                        return None;
                    }
                    self.finished
                        .wait_timeout(state, until.saturating_duration_since(now))
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            };
        }
    }
}

pub use aci_edge_sandboxes_model::outcome::{ExecFailure, ExecOutcome};

/// Collected result of [`Execution::wait_with_output`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    /// Terminal outcome.
    pub outcome: ExecOutcome,
    /// Everything the workload wrote to standard output.
    pub stdout: Vec<u8>,
    /// Everything the workload wrote to standard error.
    pub stderr: Vec<u8>,
}

/// Cancels a live execution. Clones share the same execution.
#[derive(Clone)]
pub struct Canceller {
    control: Arc<dyn ExecControl>,
}

impl Canceller {
    pub(crate) fn new(control: Arc<dyn ExecControl>) -> Self {
        Self { control }
    }

    /// Requests cancellation of the execution.
    ///
    /// Backends that cannot cancel return [`ErrorCode::Unsupported`](crate::ErrorCode::Unsupported);
    /// see [`ExecCapabilities::cancel`](crate::ExecCapabilities::cancel).
    ///
    /// A failed cancellation returns its error and does not end the workload. With the agent
    /// backend, a waiter that the outcome then does not reach within a short grace period fails
    /// with [`ErrorCode::BackendError`](crate::ErrorCode::BackendError), saying that the outcome
    /// is unknown, rather than waiting indefinitely.
    pub fn cancel(&self) -> Result<()> {
        self.control.cancel()
    }
}

impl fmt::Debug for Canceller {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Canceller").finish_non_exhaustive()
    }
}

/// A live execution returned by [`AciEdgeSandbox::exec`](crate::AciEdgeSandbox::exec).
///
/// Output arrives on operating-system pipes, so callers can hand the raw handles to other
/// components. Read both streams concurrently, or use [`Execution::wait_with_output`]; a stream
/// that is never taken is discarded when the execution is waited on.
pub struct Execution {
    stdout: Option<PipeReader>,
    stderr: Option<PipeReader>,
    stdin: Option<PipeWriter>,
    control: Arc<dyn ExecControl>,
}

impl Execution {
    pub(crate) fn new(
        stdout: PipeReader,
        stderr: PipeReader,
        stdin: Option<PipeWriter>,
        control: Arc<dyn ExecControl>,
    ) -> Self {
        Self {
            stdout: Some(stdout),
            stderr: Some(stderr),
            stdin,
            control,
        }
    }

    /// Takes the read end of the workload's standard output.
    pub fn take_stdout(&mut self) -> Option<PipeReader> {
        self.stdout.take()
    }

    /// Takes the read end of the workload's standard error.
    pub fn take_stderr(&mut self) -> Option<PipeReader> {
        self.stderr.take()
    }

    /// Takes the write end of the workload's standard input.
    ///
    /// Present only for [`StdinMode::Piped`](crate::StdinMode::Piped) requests. Drop the writer to
    /// deliver end-of-file.
    pub fn take_stdin(&mut self) -> Option<PipeWriter> {
        self.stdin.take()
    }

    /// Returns a handle that cancels this execution from any thread.
    pub fn canceller(&self) -> Canceller {
        Canceller::new(Arc::clone(&self.control))
    }

    /// Waits for the terminal outcome, discarding any output stream that was not taken.
    pub fn wait(mut self) -> Result<ExecOutcome> {
        self.stdin = None;
        self.stdout = None;
        self.stderr = None;
        self.control.wait()
    }

    /// Collects every untaken output stream and waits for the terminal outcome.
    pub fn wait_with_output(mut self) -> Result<ExecOutput> {
        self.stdin = None;
        let stdout = spawn_collector(self.stdout.take())?;
        let stderr = spawn_collector(self.stderr.take())?;
        let outcome = self.control.wait();
        let stdout = join_collector(stdout)?;
        let stderr = join_collector(stderr)?;
        Ok(ExecOutput {
            outcome: outcome?,
            stdout,
            stderr,
        })
    }
}

impl fmt::Debug for Execution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Execution")
            .field("stdout", &self.stdout.is_some())
            .field("stderr", &self.stderr.is_some())
            .field("stdin", &self.stdin.is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for Execution {
    fn drop(&mut self) {
        self.control.abandon();
    }
}

type Collector = Option<thread::JoinHandle<io::Result<Vec<u8>>>>;

fn spawn_collector(reader: Option<PipeReader>) -> Result<Collector> {
    let Some(mut reader) = reader else {
        return Ok(None);
    };
    thread::Builder::new()
        .name("nvx-exec-collect".to_owned())
        .spawn(move || {
            let mut output = Vec::new();
            reader.read_to_end(&mut output)?;
            Ok(output)
        })
        .map(Some)
        .map_err(|error| {
            Error::backend_error("failed to start an output collector thread").with_source(error)
        })
}

fn join_collector(collector: Collector) -> Result<Vec<u8>> {
    let Some(collector) = collector else {
        return Ok(Vec::new());
    };
    match collector.join() {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => {
            Err(Error::backend_error("failed to read execution output").with_source(error))
        }
        Err(_) => Err(Error::backend_error("output collector thread panicked")),
    }
}
