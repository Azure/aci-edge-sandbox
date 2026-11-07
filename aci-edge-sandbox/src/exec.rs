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
        self.collect_output(stdout, stderr)
    }

    fn collect_output(self, stdout: Collector, stderr: Collector) -> Result<ExecOutput> {
        let outcome = self.control.wait_unacknowledged();
        let stdout = join_collector(stdout)?;
        let stderr = join_collector(stderr)?;
        self.control.acknowledge_wait();
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

#[cfg(all(test, feature = "agent"))]
mod tests {
    use std::io::Write;

    use super::*;

    struct SuccessfulControl;

    impl ExecControl for SuccessfulControl {
        fn wait(&self) -> Result<ExecOutcome> {
            Ok(ExecOutcome::Exited(0))
        }

        fn cancel(&self) -> Result<()> {
            unreachable!("waiting must not cancel the execution")
        }
    }

    #[test]
    fn collecting_output_preserves_bytes_and_supports_default_control_hooks() {
        for take_stdout in [false, true] {
            let (stdout, mut stdout_writer) = io::pipe().unwrap();
            let (stderr, mut stderr_writer) = io::pipe().unwrap();
            stdout_writer.write_all(b"out\0\xff").unwrap();
            stderr_writer.write_all(b"err\0\xfe").unwrap();
            drop((stdout_writer, stderr_writer));
            let mut execution = Execution::new(stdout, stderr, None, Arc::new(SuccessfulControl));
            let taken = take_stdout.then(|| execution.take_stdout().unwrap());
            let output = execution.wait_with_output().unwrap();
            assert_eq!(output.outcome, ExecOutcome::Exited(0));
            assert_eq!(
                output.stdout,
                if take_stdout {
                    b"".as_slice()
                } else {
                    b"out\0\xff"
                }
            );
            assert_eq!(output.stderr, b"err\0\xfe");
            if let Some(mut taken) = taken {
                let mut bytes = Vec::new();
                taken.read_to_end(&mut bytes).unwrap();
                assert_eq!(bytes, b"out\0\xff");
            }
        }
    }

    fn failed_release() -> (crate::agent::AgentBackend, Execution) {
        let (backend, control) = crate::agent::tests::failed_release_for_test();
        let execution = Execution {
            stdout: None,
            stderr: None,
            stdin: None,
            control,
        };
        (backend, execution)
    }

    fn failing_collector(panics: bool) -> Collector {
        Some(thread::spawn(move || {
            assert!(!panics, "injected output collector panic");
            Err(io::Error::other("injected output read failure"))
        }))
    }

    #[test]
    fn output_collection_failure_preserves_release_failure_for_cleanup_once() {
        for stderr_fails in [false, true] {
            for panics in [false, true] {
                let (backend, execution) = failed_release();
                let canceller = execution.canceller();
                let collector = failing_collector(panics);
                let (stdout, stderr) = if stderr_fails {
                    (None, collector)
                } else {
                    (collector, None)
                };
                let error = execution.collect_output(stdout, stderr).unwrap_err();
                assert_eq!(
                    error.message(),
                    if panics {
                        "output collector thread panicked"
                    } else {
                        "failed to read execution output"
                    }
                );
                if !panics {
                    assert_eq!(
                        std::error::Error::source(&error).unwrap().to_string(),
                        "injected output read failure"
                    );
                }
                let failures = backend.take_cleanup_failures();
                assert_eq!(failures.len(), 1);
                let error = &failures[0].error;
                assert!(error.message().contains("cannot release"), "{error}");
                assert!(error.message().contains("exited with status 0"), "{error}");
                assert!(
                    std::error::Error::source(error)
                        .unwrap()
                        .to_string()
                        .contains("panicked"),
                    "{error}"
                );
                assert!(backend.take_cleanup_failures().is_empty());
                drop(canceller);
                assert!(backend.take_cleanup_failures().is_empty());
                backend.close().unwrap();
            }
        }
    }

    #[test]
    fn close_reports_release_failure_hidden_by_output_collection_failure() {
        let (backend, execution) = failed_release();
        execution
            .collect_output(None, failing_collector(false))
            .unwrap_err();
        let error = backend.close().unwrap_err();
        assert!(error.message().contains("cannot release"), "{error}");
    }

    #[test]
    fn delivered_sync_wait_results_do_not_report_release_failures_again() {
        for with_output in [false, true] {
            let (backend, execution) = failed_release();
            let canceller = execution.canceller();
            let error = if with_output {
                execution.wait_with_output().unwrap_err()
            } else {
                execution.wait().unwrap_err()
            };
            assert!(error.message().contains("cannot release"), "{error}");
            assert!(backend.take_cleanup_failures().is_empty());
            drop(canceller);
            assert!(backend.take_cleanup_failures().is_empty());
            backend.close().unwrap();
        }
    }
}
