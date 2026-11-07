// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Tokio wrappers around the synchronous core.

use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::thread;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::backend::{ExecControl, ExecIo};
use crate::capabilities::Capabilities;
use crate::client::AciEdgeSandbox;
use crate::error::{Error, Result};
use crate::exec::{Canceller, ExecOutcome, ExecOutput};
use crate::id::SandboxId;
use crate::input::InputSource;
use crate::model::{
    DeprovisionResult, ExecRequest, ProvisionRequest, ProvisionResult, StartResult, StdinMode,
    StopResult,
};
use crate::spec::SandboxSpec;
use crate::stream::{self, QueueReader, QueueWriter};

/// Asynchronous counterpart of [`AciEdgeSandbox`].
///
/// Lifecycle calls run on Tokio's blocking thread pool, so they must be awaited inside a Tokio
/// runtime. Output streams are delivered in-process without extra operating-system pipes.
///
/// # Cancellation
///
/// Tokio cannot interrupt work on its blocking pool, so every call keeps running after its future
/// is dropped. When the future of [`exec`](Self::exec) is dropped before it resolves, nothing else
/// can reach the workload that the call starts, which could then run indefinitely and keep
/// later executions waiting for the sandbox. The call therefore requests the
/// workload's cancellation as soon as the backend starts it, as [`Canceller::cancel`] would. Only
/// backends whose [`ExecCapabilities::cancel`](crate::ExecCapabilities::cancel) is true, such as
/// the agent backend, honor the request; with other backends, the workload runs until it ends
/// on its own, so check [`capabilities`](Self::capabilities) before relying on a dropped future
/// to stop a workload. The other calls run to completion, and the sandbox's state reflects their
/// effects: a dropped `start` or `stop` may still start or stop the sandbox, a dropped
/// `deprovision` may still release it, and a dropped `provision` may still allocate a sandbox
/// whose ID the caller never receives.
///
/// Dropping an [`AsyncExecution`] does not cancel its workload. To bound a running execution,
/// take a [`Canceller`] from [`AsyncExecution::canceller`] before awaiting
/// [`AsyncExecution::wait`].
#[derive(Clone, Debug)]
pub struct AsyncAciEdgeSandbox {
    nvx: AciEdgeSandbox,
}

impl AsyncAciEdgeSandbox {
    /// Wraps a synchronous client.
    pub fn new(nvx: AciEdgeSandbox) -> Self {
        Self { nvx }
    }

    /// Returns the underlying synchronous client.
    pub fn blocking(&self) -> &AciEdgeSandbox {
        &self.nvx
    }

    /// Returns the features the backend honors.
    pub fn capabilities(&self) -> Capabilities {
        self.nvx.capabilities()
    }

    /// Checks that the backend's runtime dependencies are present.
    pub async fn probe(&self) -> Result<()> {
        let nvx = self.nvx.clone();
        blocking(move || nvx.probe()).await
    }

    /// Allocates a sandbox without starting it.
    pub async fn provision(&self, request: ProvisionRequest) -> Result<ProvisionResult> {
        let nvx = self.nvx.clone();
        blocking(move || nvx.provision(&request)).await
    }

    /// Allocates a sandbox with creation settings outside the request, without starting it.
    pub async fn provision_with(
        &self,
        request: ProvisionRequest,
        spec: SandboxSpec,
    ) -> Result<ProvisionResult> {
        let nvx = self.nvx.clone();
        blocking(move || nvx.provision_with(&request, &spec)).await
    }

    /// Moves a provisioned sandbox to the running state.
    pub async fn start(&self, sandbox_id: SandboxId) -> Result<StartResult> {
        let nvx = self.nvx.clone();
        blocking(move || nvx.start(&sandbox_id)).await
    }

    /// Starts a workload in a running sandbox and returns its live streams.
    ///
    /// Dropping the returned future before it resolves requests the workload's cancellation, which
    /// only backends with [`ExecCapabilities::cancel`](crate::ExecCapabilities::cancel) honor; see
    /// [Cancellation](Self#cancellation).
    pub async fn exec(
        &self,
        sandbox_id: SandboxId,
        request: ExecRequest,
    ) -> Result<AsyncExecution> {
        let (stdout_sink, stdout) = stream::queue();
        let (stderr_sink, stderr) = stream::queue();
        let (backend_stdin, stdin) = match request.stdin {
            StdinMode::Null => (None, None),
            StdinMode::Piped => {
                let (writer, reader) = stream::queue();
                (
                    Some(InputSource::from_queue(reader)),
                    Some(InputStream {
                        writer: Some(writer),
                    }),
                )
            }
        };
        let io = ExecIo {
            stdout: Box::new(stdout_sink),
            stderr: Box::new(stderr_sink),
            stdin: backend_stdin,
        };
        let nvx = self.nvx.clone();
        let control = blocking(move || {
            nvx.exec_with_io(&sandbox_id, &request, io)
                .map(CancelOnDrop::new)
        })
        .await?;
        Ok(AsyncExecution {
            stdout: Some(OutputStream { reader: stdout }),
            stderr: Some(OutputStream { reader: stderr }),
            stdin,
            control: Arc::from(control.defuse()),
        })
    }

    /// Moves a running sandbox back to the provisioned state.
    pub async fn stop(&self, sandbox_id: SandboxId) -> Result<StopResult> {
        let nvx = self.nvx.clone();
        blocking(move || nvx.stop(&sandbox_id)).await
    }

    /// Releases a provisioned sandbox. The sandbox ID becomes stale.
    pub async fn deprovision(&self, sandbox_id: SandboxId) -> Result<DeprovisionResult> {
        let nvx = self.nvx.clone();
        blocking(move || nvx.deprovision(&sandbox_id)).await
    }
}

impl From<AciEdgeSandbox> for AsyncAciEdgeSandbox {
    fn from(nvx: AciEdgeSandbox) -> Self {
        Self::new(nvx)
    }
}

async fn blocking<T, F>(operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| Error::backend_error("sandbox blocking task failed").with_source(error))?
}

/// Requests the cancellation of an execution when dropped, unless [`CancelOnDrop::defuse`] hands
/// the execution over first.
///
/// The blocking task of [`AsyncAciEdgeSandbox::exec`] keeps running after its caller drops the
/// future, and Tokio then drops the task's output, so the backend is asked to cancel the workload
/// that the task started, which nothing else could reach.
struct CancelOnDrop(Option<Box<dyn ExecControl>>);

impl CancelOnDrop {
    fn new(control: Box<dyn ExecControl>) -> Self {
        Self(Some(control))
    }

    fn defuse(mut self) -> Box<dyn ExecControl> {
        self.0.take().expect("only defuse takes the execution")
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(control) = self.0.take() {
            // Nobody is left to report a failure to, such as the `Unsupported` error of a backend
            // that cannot cancel.
            let _ = control.cancel();
        }
    }
}

/// A live execution returned by [`AsyncAciEdgeSandbox::exec`].
pub struct AsyncExecution {
    stdout: Option<OutputStream>,
    stderr: Option<OutputStream>,
    stdin: Option<InputStream>,
    control: Arc<dyn ExecControl>,
}

impl AsyncExecution {
    /// Takes the workload's standard output stream.
    pub fn take_stdout(&mut self) -> Option<OutputStream> {
        self.stdout.take()
    }

    /// Takes the workload's standard error stream.
    pub fn take_stderr(&mut self) -> Option<OutputStream> {
        self.stderr.take()
    }

    /// Takes the workload's standard input stream. Present only for
    /// [`StdinMode::Piped`] requests; shut it down or drop it to deliver end-of-file.
    pub fn take_stdin(&mut self) -> Option<InputStream> {
        self.stdin.take()
    }

    /// Returns a handle that cancels this execution.
    pub fn canceller(&self) -> Canceller {
        Canceller::new(Arc::clone(&self.control))
    }

    /// Waits for the terminal outcome, discarding any output stream that was not taken.
    pub async fn wait(mut self) -> Result<ExecOutcome> {
        self.stdin = None;
        self.stdout = None;
        self.stderr = None;
        let control = Arc::clone(&self.control);
        let outcome = blocking(move || Ok(control.wait_unacknowledged())).await?;
        self.control.acknowledge_wait();
        outcome
    }

    /// Collects every untaken output stream and waits for the terminal outcome.
    pub async fn wait_with_output(mut self) -> Result<ExecOutput> {
        self.stdin = None;
        let stdout = self.stdout.take();
        let stderr = self.stderr.take();
        let control = Arc::clone(&self.control);
        let (outcome, stdout, stderr) = blocking(move || {
            // Both streams drain while the outcome is awaited, so a timeout that closes
            // retained output can end either collector instead of waiting for end-of-stream.
            let stdout = spawn_output_collector(stdout)?;
            let stderr = match spawn_output_collector(stderr) {
                Ok(stderr) => stderr,
                Err(error) => {
                    // Cancelling ends the workload's output, so the stdout collector can finish
                    // rather than block on a stream that nothing else will drain.
                    let _ = control.cancel();
                    let _ = control.wait_unacknowledged();
                    let _ = join_output_collector(stdout);
                    return Err(error);
                }
            };
            let outcome = control.wait_unacknowledged();
            let stdout = join_output_collector(stdout);
            let stderr = join_output_collector(stderr);
            Ok((outcome, stdout, stderr))
        })
        .await?;
        let stdout = stdout?;
        let stderr = stderr?;
        self.control.acknowledge_wait();
        Ok(ExecOutput {
            outcome: outcome?,
            stdout,
            stderr,
        })
    }
}

impl Drop for AsyncExecution {
    fn drop(&mut self) {
        self.control.abandon();
    }
}

type OutputCollector = Option<thread::JoinHandle<Vec<u8>>>;

fn spawn_output_collector(stream: Option<OutputStream>) -> Result<OutputCollector> {
    let Some(stream) = stream else {
        return Ok(None);
    };
    thread::Builder::new()
        .name("nvx-exec-collect".to_owned())
        .spawn(move || stream.reader.read_to_end_blocking())
        .map(Some)
        .map_err(|error| {
            Error::backend_error("failed to start an output collector thread").with_source(error)
        })
}

fn join_output_collector(collector: OutputCollector) -> Result<Vec<u8>> {
    match collector {
        Some(collector) => collector
            .join()
            .map_err(|_| Error::backend_error("output collector thread panicked")),
        None => Ok(Vec::new()),
    }
}

impl fmt::Debug for AsyncExecution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AsyncExecution")
            .field("stdout", &self.stdout.is_some())
            .field("stderr", &self.stderr.is_some())
            .field("stdin", &self.stdin.is_some())
            .finish_non_exhaustive()
    }
}

/// Asynchronous output stream of an [`AsyncExecution`].
pub struct OutputStream {
    reader: QueueReader,
}

impl AsyncRead for OutputStream {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let unfilled = buffer.initialize_unfilled();
        match self.reader.poll_read(context, unfilled) {
            Poll::Ready(count) => {
                buffer.advance(count);
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl fmt::Debug for OutputStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutputStream")
            .finish_non_exhaustive()
    }
}

/// Asynchronous standard input stream of an [`AsyncExecution`].
pub struct InputStream {
    writer: Option<QueueWriter>,
}

impl AsyncWrite for InputStream {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &self.writer {
            Some(writer) => writer.poll_write(context, buffer),
            None => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.writer = None;
        Poll::Ready(Ok(()))
    }
}

impl fmt::Debug for InputStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InputStream")
            .field("open", &self.writer.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    struct AbandonmentReporter(mpsc::Sender<()>);

    #[cfg(feature = "agent")]
    struct WaitReporter {
        control: Arc<dyn ExecControl>,
        returned: mpsc::SyncSender<()>,
    }

    #[cfg(feature = "agent")]
    impl ExecControl for WaitReporter {
        fn wait(&self) -> Result<ExecOutcome> {
            self.control.wait()
        }

        fn wait_unacknowledged(&self) -> Result<ExecOutcome> {
            let outcome = self.control.wait_unacknowledged();
            self.returned.send(()).unwrap();
            outcome
        }

        fn acknowledge_wait(&self) {
            self.control.acknowledge_wait();
        }

        fn cancel(&self) -> Result<()> {
            self.control.cancel()
        }

        fn abandon(&self) {
            self.control.abandon();
        }
    }

    #[cfg(feature = "agent")]
    #[test]
    fn dropping_an_async_wait_after_its_worker_returns_reports_release_failure_once() {
        use std::future::Future;
        use std::task::Waker;
        use std::time::Duration;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        for with_output in [false, true] {
            runtime.block_on(async {
                let (backend, control) = crate::agent::tests::failed_release_for_test();
                // The worker cannot return until the first poll has completed.
                let (returned, collected) = mpsc::sync_channel(0);
                let execution = AsyncExecution {
                    stdout: None,
                    stderr: None,
                    stdin: None,
                    control: Arc::new(WaitReporter { control, returned }),
                };
                let canceller = execution.canceller();
                let mut waiting: Pin<Box<dyn Future<Output = Result<ExecOutcome>>>> = if with_output
                {
                    Box::pin(async move {
                        execution
                            .wait_with_output()
                            .await
                            .map(|output| output.outcome)
                    })
                } else {
                    Box::pin(execution.wait())
                };
                let mut context = Context::from_waker(Waker::noop());
                assert!(waiting.as_mut().poll(&mut context).is_pending());
                collected.recv_timeout(Duration::from_secs(30)).unwrap();
                assert!(backend.take_cleanup_failures().is_empty());
                // The worker has the error, but the awaiting caller has never received it.
                drop(waiting);
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
            });
        }
    }

    #[cfg(feature = "agent")]
    #[test]
    fn delivered_async_wait_results_do_not_report_release_failures_again() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        for with_output in [false, true] {
            runtime.block_on(async {
                let (backend, control) = crate::agent::tests::failed_release_for_test();
                let execution = AsyncExecution {
                    stdout: None,
                    stderr: None,
                    stdin: None,
                    control,
                };
                let error = if with_output {
                    execution.wait_with_output().await.unwrap_err()
                } else {
                    execution.wait().await.unwrap_err()
                };
                assert!(error.message().contains("cannot release"), "{error}");
                assert!(backend.take_cleanup_failures().is_empty());
                backend.close().unwrap();
            });
        }
    }

    impl ExecControl for AbandonmentReporter {
        fn wait(&self) -> Result<ExecOutcome> {
            panic!("this execution is dropped without waiting")
        }

        fn cancel(&self) -> Result<()> {
            panic!("dropping an execution must not cancel it")
        }

        fn abandon(&self) {
            self.0.send(()).unwrap();
        }
    }

    #[test]
    fn dropping_an_async_execution_notifies_the_backend_despite_a_retained_canceller() {
        let (sender, receiver) = mpsc::channel();
        let execution = AsyncExecution {
            stdout: None,
            stderr: None,
            stdin: None,
            control: Arc::new(AbandonmentReporter(sender)),
        };
        let canceller = execution.canceller();
        drop(execution);
        receiver.try_recv().expect("the backend was not notified");
        assert!(receiver.try_recv().is_err());
        drop(canceller);
    }
}
