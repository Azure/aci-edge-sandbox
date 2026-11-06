// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Backend over a separately supplied native agent library, `aci_edge_agent`.
//!
//! The library owns the whole sandbox lifecycle: the state root and its records, the VMM
//! processes and their boot consoles, the guest sessions, and the guest images. This backend is a
//! thin client of it: it checks the library's SHA-256, loads it once per process, opens a host
//! with a [`SetupConfig`], and forwards each [`Backend`] operation through the library's sandbox
//! ABI, which passes the JSON of this crate's data model.
//!
//! The setup names the runtime files, the state root, the hypervisor, deadlines, and the defaults
//! that each sandbox inherits; a [`SandboxSpec`] given to
//! [`AciEdgeSandbox::provision_with`](crate::AciEdgeSandbox::provision_with) overrides them for
//! one sandbox. Everything that outlives one call stays in the library for the life of the
//! process, so a caller can create a backend per operation and pay for verifying the runtime
//! files and attaching to a guest only once.
//!
//! With the `agent-bundled` feature, the build script stages an edge agent bundle beside the
//! executables, and [`AgentConfig::bundled`] uses it without any path or digest from the caller.

// The names and pins that the build script shares with this crate.
#[cfg(feature = "agent-bundled")]
#[allow(dead_code)]
mod bundle_contract;
#[cfg(feature = "agent-bundled")]
mod bundled;
mod library;

use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

pub use aci_edge_sandboxes_model::wire::{HostInfo, SandboxDiagnostics};
pub use aci_edge_sandboxes_model::{
    BundleSource, CpuProfile, Diagnostics, DisabledReason, ExecSettings, GuestSessionPolicy,
    Hypervisor, ImageDigest, ImageId, ImageSettings, PlatformSnapshots, PlatformSnapshotsInfo,
    ReferenceSettings, RegisteredImage, RuntimeDigests, RuntimeFiles, RuntimeSource,
    SandboxDefaults, SetupConfig, TemplateInfo, TemplateState, Timeouts,
};

#[cfg(feature = "agent-bundled")]
pub use self::bundled::BundledAgent;

use self::library::{Event, Library};
use crate::backend::{Backend, ExecControl, ExecIo, OutputCloser, OutputSink};
use crate::capabilities::Capabilities;
use crate::error::{Error, Result};
use crate::exec::{Completion, ExecOutcome};
use crate::id::SandboxId;
use crate::input::{InputCloser, InputSource};
use crate::model::{
    DeprovisionResult, ExecRequest, ProvisionRequest, ProvisionResult, StartResult, StopResult,
    duration_millis,
};
use crate::spec::SandboxSpec;
use aci_edge_sandboxes_model::wire::{self, ExecEnd, ImageRegistration, ProvisionReply};

/// Largest standard-input chunk handed to the library at once.
const INPUT_CHUNK: usize = 64 * 1024;

/// How long one write of standard input waits for input credit before the input worker checks
/// whether it was told to stop.
const INPUT_WRITE_POLL_MS: u64 = 100;

/// How long [`AgentBackend::close`] waits for the library to release the executions it holds.
const RELEASE_SETTLE: Duration = Duration::from_secs(5);

/// How long a waiter still waits for an execution's end after its cancellation failed, in case
/// the execution was ending anyway.
const CANCEL_FAILURE_GRACE: Duration = Duration::from_secs(2);

/// The library to load and the host to open.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AgentConfig {
    /// The host's configuration, which the library applies.
    pub setup: SetupConfig,
    /// Absolute path to a separately installed `aci_edge_agent.dll` or `libaci_edge_agent.so`.
    pub library: PathBuf,
    /// SHA-256 of the library, approved by the caller's own artifact policy.
    pub library_sha256: [u8; 32],
}

impl AgentConfig {
    /// Opens a host for `setup` through the library at `library`, which must have the approved
    /// `library_sha256`.
    pub fn new(setup: SetupConfig, library: impl Into<PathBuf>, library_sha256: [u8; 32]) -> Self {
        Self {
            setup,
            library: library.into(),
            library_sha256,
        }
    }

    /// Opens a host for `state_root`, with every other setup field at its default, through the
    /// bundle that the build staged with the `agent-bundled` feature; see
    /// [`BundledAgent::discover`].
    #[cfg(feature = "agent-bundled")]
    pub fn bundled(state_root: impl Into<PathBuf>) -> Result<Self> {
        let bundle = BundledAgent::discover()?;
        Ok(bundle.config(bundle.setup(state_root)))
    }
}

/// [`Backend`] that forwards the lifecycle to a separately supplied agent library, `aci_edge_agent`.
///
/// Dropping the backend closes its host in the library but discards a failure to close it; call
/// [`close`](Self::close) to receive that failure.
#[derive(Debug)]
pub struct AgentBackend {
    config: AgentConfig,
    library: &'static Library,
    host: u64,
    capabilities: Capabilities,
    info: HostInfo,
    /// Whether [`close`](Self::close) already closed the host, which `Drop` then leaves alone.
    closed: bool,
    /// Release failures of this backend's executions that their waiters could not receive.
    cleanup: Arc<CleanupLog>,
}

/// A failure of the agent library to release an execution, which the execution's
/// [`wait`](crate::Execution::wait) could not report: the execution had already failed, so it
/// reported its own failure, or no caller waited for it any more, because its waiter had already
/// received an outcome, as after a failed cancellation, or its caller dropped it.
/// [`AgentBackend::take_cleanup_failures`] and [`AgentBackend::close`] return them.
#[derive(Debug)]
#[non_exhaustive]
pub struct CleanupFailure {
    /// The sandbox that ran the execution.
    pub sandbox_id: SandboxId,
    /// Why the library could not release the execution.
    pub error: Error,
}

/// The cleanup failures of a backend's executions, and how many of its unattended executions the
/// library still holds.
#[derive(Debug, Default)]
struct CleanupLog {
    state: Mutex<CleanupState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct CleanupState {
    failures: Vec<CleanupFailure>,
    /// Executions that no caller waits for any more, whose handles are not released yet: their
    /// waiters received an outcome that did not come from the library, or the callers dropped
    /// them before they ended. Only these leave release failures that no `wait` can report.
    unattended: usize,
}

impl CleanupLog {
    fn lock(&self) -> std::sync::MutexGuard<'_, CleanupState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts an execution that became unattended while the library holds its handle.
    fn leave(&self) {
        self.lock().unattended += 1;
    }

    /// Records that an execution's handle was released, and the failure, if any, that its waiter
    /// could not receive.
    fn released(&self, unattended: bool, failure: Option<CleanupFailure>) {
        let mut state = self.lock();
        if unattended {
            state.unattended = state.unattended.saturating_sub(1);
        }
        state.failures.extend(failure);
        drop(state);
        self.changed.notify_all();
    }

    fn take(&self) -> Vec<CleanupFailure> {
        std::mem::take(&mut self.lock().failures)
    }

    /// Waits until every unattended execution is released or `timeout` passes, then takes the
    /// failures and returns them with the number of unattended executions still held.
    fn settle(&self, timeout: Duration) -> (Vec<CleanupFailure>, usize) {
        let state = self.lock();
        let (mut state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| state.unattended > 0)
            .unwrap_or_else(PoisonError::into_inner);
        (std::mem::take(&mut state.failures), state.unattended)
    }
}

/// Where an execution reports a release failure that its waiter cannot receive.
struct Cleanup {
    sandbox_id: SandboxId,
    log: Arc<CleanupLog>,
}

/// The provision call's input: the model's [`wire::ProvisionInput`], borrowed.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProvisionInput<'a> {
    request: &'a ProvisionRequest,
    #[serde(skip_serializing_if = "SandboxSpec::is_empty")]
    spec: &'a SandboxSpec,
}

/// The exec call's input: the model's [`wire::ExecInput`], borrowed. It never sends `pty`,
/// because the library does not run commands on a terminal.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecInput<'a> {
    request: &'a ExecRequest,
    stdin: bool,
}

// A field added to a wire input stops these from compiling until the envelope above handles it.
const _: fn(wire::ProvisionInput) = |wire::ProvisionInput {
                                         request: _,
                                         spec: _,
                                     }| {};
const _: fn(wire::ExecInput) = |wire::ExecInput {
                                    request: _,
                                    stdin: _,
                                    pty: _,
                                }| {};

impl AgentBackend {
    /// Loads the library, unless this process already did, and opens the host of the setup.
    ///
    /// The first host that a process opens for a setup verifies the runtime files, or checks the
    /// seals that an earlier process recorded for them; later ones reuse that host.
    pub fn new(config: AgentConfig) -> Result<Self> {
        let library = library::load(&config.library, &config.library_sha256)?;
        let host = library.host_open(&config.setup)?;
        let opened = library
            .capabilities(host)
            .and_then(|capabilities| Ok((capabilities, library.host_info(host)?)));
        match opened {
            Ok((capabilities, info)) => Ok(Self {
                config,
                library,
                host,
                capabilities,
                info,
                closed: false,
                cleanup: Arc::default(),
            }),
            Err(error) => {
                // The open's own failure is the one to report.
                let _ = library.host_close(host);
                Err(error)
            }
        }
    }

    /// Closes the host in the library, and returns the library's failure to close it, which
    /// dropping the backend discards, together with every [`CleanupFailure`] of this backend's
    /// executions that [`take_cleanup_failures`](Self::take_cleanup_failures) has not returned.
    ///
    /// An execution that no caller waits for any more, because its waiter was told that its
    /// outcome is unknown or its caller dropped it before it ended, may still fail to release.
    /// `close` first waits up to five seconds for the library to release such executions, and
    /// also fails if any remain; stop or deprovision their sandboxes first, which ends them. It
    /// does not wait for an execution that a caller still waits for, whose `wait` reports its
    /// own release. The library keeps the host's sandboxes and their guest sessions either way,
    /// and the host is not closed again on drop.
    ///
    /// A backend shared through an [`Arc`], as [`AciEdgeSandbox::from_shared`] takes it, is
    /// closed once its other owners are gone, for example through [`Arc::into_inner`].
    ///
    /// [`AciEdgeSandbox::from_shared`]: crate::AciEdgeSandbox::from_shared
    pub fn close(mut self) -> Result<()> {
        self.closed = true;
        let (failures, unattended) = self.cleanup.settle(RELEASE_SETTLE);
        let closed = self.library.host_close(self.host);
        let mut problems = Vec::new();
        for failure in &failures {
            problems.push(format!(
                "an execution of sandbox {} failed to release: {}",
                failure.sandbox_id, failure.error
            ));
        }
        if unattended > 0 {
            problems.push(format!(
                "{unattended} execution(s) that no caller waits for were still held by the \
                 library after {} s, so whether they release is unknown",
                RELEASE_SETTLE.as_secs()
            ));
        }
        let first = failures.into_iter().next().map(|failure| failure.error);
        match closed {
            Err(error) => {
                let mut message = "cannot close the agent host".to_owned();
                if !problems.is_empty() {
                    message = format!("{message}; also {}", problems.join("; "));
                }
                Err(Error::backend_error(message).with_source(error))
            }
            Ok(()) if problems.is_empty() => Ok(()),
            Ok(()) => {
                let error = Error::backend_error(format!(
                    "the agent host closed, but {}",
                    problems.join("; ")
                ));
                Err(match first {
                    Some(cause) => error.with_source(cause),
                    None => error,
                })
            }
        }
    }

    /// Returns, and forgets, every [`CleanupFailure`] of this backend's executions so far, for a
    /// caller that keeps the backend open; [`close`](Self::close) returns the rest.
    pub fn take_cleanup_failures(&self) -> Vec<CleanupFailure> {
        self.cleanup.take()
    }

    /// Returns the configuration.
    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// Returns facts about the host, as of when this backend opened it.
    pub fn info(&self) -> &HostInfo {
        &self.info
    }

    /// Returns the ID of the default image that the setup names, which the sandboxes whose spec
    /// names no image boot.
    pub fn image_id(&self) -> Option<ImageId> {
        self.info.default_image
    }

    /// Returns the digests of the runtime files that the host verified.
    pub fn runtime_digests(&self) -> RuntimeDigests {
        self.info.runtime_digests
    }

    /// Registers the local image at `path`. The library hashes it once, unless `digest` is
    /// [`ImageDigest::Trusted`], and stages it: sandboxes then attach the library's own read-only
    /// copy, which [`RegisteredImage::path`] names, and later changes to the file at `path` do not
    /// reach them. A registration that the file still matches is reused without reading the file.
    pub fn register_image(
        &self,
        path: impl Into<PathBuf>,
        digest: ImageDigest,
    ) -> Result<RegisteredImage> {
        let path = std::path::absolute(path.into()).map_err(|error| {
            Error::malformed_request("cannot resolve the image path").with_source(error)
        })?;
        self.register(&ImageRegistration::path(path, digest))
    }

    /// Registers the registry image `reference`: the library pulls it with the settings of
    /// [`ReferenceSettings`], then hashes and stages it as
    /// [`register_image`](Self::register_image) does. A reference that is registered already is
    /// reused without pulling it again. A backend that does not honor references, as
    /// [`Capabilities::spec`] reports, refuses it with
    /// [`ErrorCode::PolicyValidation`](crate::ErrorCode::PolicyValidation).
    pub fn register_reference(
        &self,
        reference: impl Into<String>,
        digest: ImageDigest,
    ) -> Result<RegisteredImage> {
        self.register(&ImageRegistration::reference(reference, digest))
    }

    /// Checks the structure of `registration`, then that this backend honors it, and registers
    /// its image.
    fn register(&self, registration: &ImageRegistration) -> Result<RegisteredImage> {
        registration.validate()?;
        if registration.reference.is_some() && !self.capabilities.spec.image_reference {
            return Err(Error::policy_validation(format!(
                "the {} backend cannot register image references",
                self.capabilities.backend
            )));
        }
        self.library.image_register(self.host, registration)
    }

    /// Lists the registered images and whether each file still matches its registration.
    pub fn images(&self) -> Result<Vec<RegisteredImage>> {
        self.library.image_list(self.host)
    }

    /// Hashes a registered image and checks that it still has the content that its ID names: a
    /// diagnostic, since no other operation hashes an image again.
    pub fn verify_image(&self, id: &ImageId) -> Result<()> {
        self.library.image_verify(self.host, &id.to_string())
    }

    /// Removes the registration of an image, and the library's staged copy of it, returning
    /// whether it existed. The registered file stays.
    ///
    /// Fails with [`ErrorCode::PolicyValidation`](crate::ErrorCode::PolicyValidation) while a
    /// provisioned sandbox boots the image.
    pub fn unregister_image(&self, id: &ImageId) -> Result<bool> {
        self.library.image_unregister(self.host, &id.to_string())
    }

    /// Collects the guest's bounded log snapshot of a running sandbox.
    pub fn guest_logs(&self, sandbox_id: &SandboxId) -> Result<Vec<u8>> {
        self.library.guest_logs(self.host, sandbox_id.token())
    }

    /// Returns where a sandbox's host-side diagnostics are: the VMM log, the guest boot console,
    /// and the VMM's report of how its last run ended.
    pub fn diagnostics(&self, sandbox_id: &SandboxId) -> Result<SandboxDiagnostics> {
        self.library.diagnostics(self.host, sandbox_id.token())
    }

    /// Returns the VMM log of a sandbox's latest start, or an empty path if it is unknown.
    pub fn log_path(&self, sandbox_id: &SandboxId) -> PathBuf {
        self.diagnostics(sandbox_id)
            .map(|diagnostics| diagnostics.vmm_log)
            .unwrap_or_default()
    }

    /// Returns the guest boot-console log of a sandbox, or an empty path if it is unknown.
    pub fn console_log_path(&self, sandbox_id: &SandboxId) -> PathBuf {
        self.diagnostics(sandbox_id)
            .map(|diagnostics| diagnostics.console_log)
            .unwrap_or_default()
    }

    /// Returns the VMM's outcome report of a sandbox's latest run, or an empty path if it is
    /// unknown.
    pub fn outcome_report_path(&self, sandbox_id: &SandboxId) -> PathBuf {
        self.diagnostics(sandbox_id)
            .map(|diagnostics| diagnostics.outcome_report)
            .unwrap_or_default()
    }
}

impl Drop for AgentBackend {
    fn drop(&mut self) {
        // The host, its sandboxes, and their guest sessions stay with the library. A failure to
        // close cannot be returned from here; `close` returns it.
        if !self.closed {
            let _ = self.library.host_close(self.host);
        }
    }
}

impl Backend for AgentBackend {
    fn name(&self) -> &str {
        &self.capabilities.backend
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    fn probe(&self) -> Result<()> {
        self.library.probe(self.host)
    }

    fn validate_provision(&self, request: &ProvisionRequest) -> Result<()> {
        self.validate_provision_with(request, &SandboxSpec::new())
    }

    fn validate_provision_with(
        &self,
        request: &ProvisionRequest,
        spec: &SandboxSpec,
    ) -> Result<()> {
        self.library
            .validate_provision(self.host, &ProvisionInput { request, spec })
    }

    fn validate_exec(&self, request: &ExecRequest) -> Result<()> {
        self.library.validate_exec(self.host, request)
    }

    fn provision(&self, request: &ProvisionRequest) -> Result<ProvisionResult> {
        self.provision_with(request, &SandboxSpec::new())
    }

    fn provision_with(
        &self,
        request: &ProvisionRequest,
        spec: &SandboxSpec,
    ) -> Result<ProvisionResult> {
        let reply: ProvisionReply = self
            .library
            .provision(self.host, &ProvisionInput { request, spec })?;
        Ok(ProvisionResult {
            sandbox_id: sandbox_id(&reply.token)?,
            metadata: reply.metadata,
        })
    }

    fn start(&self, sandbox_id: &SandboxId) -> Result<StartResult> {
        self.library.start(self.host, sandbox_id.token())
    }

    fn exec(
        &self,
        sandbox_id: &SandboxId,
        request: &ExecRequest,
        io: ExecIo,
    ) -> Result<Box<dyn ExecControl>> {
        let exec = self.library.exec(
            self.host,
            sandbox_id.token(),
            &ExecInput {
                request,
                stdin: io.stdin.is_some(),
            },
        )?;
        run(
            self.library,
            exec,
            io,
            deadline(request.process.timeout),
            Cleanup {
                sandbox_id: sandbox_id.clone(),
                log: Arc::clone(&self.cleanup),
            },
        )
    }

    fn stop(&self, sandbox_id: &SandboxId) -> Result<StopResult> {
        self.library.stop(self.host, sandbox_id.token())
    }

    fn deprovision(&self, sandbox_id: &SandboxId) -> Result<DeprovisionResult> {
        self.library.deprovision(self.host, sandbox_id.token())
    }
}

/// The sandbox ID of a token that the library returned. A malformed token is the library's
/// fault, not the caller's.
fn sandbox_id(token: &str) -> Result<SandboxId> {
    SandboxId::from_token(token).map_err(|error| {
        Error::backend_error("the agent library returned a malformed sandbox token")
            .with_source(error)
    })
}

/// When an execution that starts now with `timeout` times out, if it has a timeout. The library
/// receives the timeout in whole milliseconds, so this uses the same duration.
fn deadline(timeout: Option<Duration>) -> Option<Instant> {
    let millis = timeout.map(duration_millis).filter(|&millis| millis != 0)?;
    Instant::now().checked_add(Duration::from_millis(millis))
}

/// Runs an execution that the library started: a pump copies its output into the sinks and
/// publishes its outcome, and an input worker feeds it the caller's standard input. `deadline` is
/// when the execution's own timeout elapses, if it has one. A release failure that the waiter
/// cannot receive goes to `cleanup`.
fn run(
    library: &'static Library,
    exec: u64,
    io: ExecIo,
    deadline: Option<Instant>,
    cleanup: Cleanup,
) -> Result<Box<dyn ExecControl>> {
    let ExecIo {
        stdout,
        stderr,
        stdin,
    } = io;
    let shared = Arc::new(ExecShared {
        library,
        handle: Mutex::new(Some(exec)),
        completion: Completion::default(),
        closers: Closers {
            stdout: stdout.close_handle(),
            stderr: stderr.close_handle(),
            stdin: stdin.as_ref().map(InputSource::close_handle),
        },
        deadline,
        expired: AtomicBool::new(false),
        input_stopped: Arc::new(AtomicBool::new(false)),
        settlement: Mutex::new(Settlement::default()),
        cleanup,
    });
    let input = match stdin {
        Some(source) => {
            let stopped = Arc::clone(&shared.input_stopped);
            match thread::Builder::new()
                .name("agent-stdin".to_owned())
                .spawn(move || feed(library, exec, source, &stopped))
            {
                Ok(input) => Some(input),
                Err(error) => {
                    shared.abandon(None);
                    return Err(
                        Error::backend_error("cannot start an input thread").with_source(error)
                    );
                }
            }
        }
        None => None,
    };
    let pumped = Arc::clone(&shared);
    let input = Arc::new(Mutex::new(input));
    let pump_input = Arc::clone(&input);
    let spawned = thread::Builder::new()
        .name("agent-exec".to_owned())
        .spawn(move || {
            let input = pump_input
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            pump(pumped, exec, stdout, stderr, input);
        });
    if let Err(error) = spawned {
        let input = input.lock().unwrap_or_else(PoisonError::into_inner).take();
        shared.abandon(input);
        return Err(Error::backend_error("cannot start an execution thread").with_source(error));
    }
    Ok(Box::new(AgentExecution { shared }))
}

/// The thread that feeds an execution's standard input; it returns a failed read of the source.
type InputWorker = thread::JoinHandle<io::Result<()>>;

/// Ends the streams of an execution early.
struct Closers {
    stdout: Option<OutputCloser>,
    stderr: Option<OutputCloser>,
    stdin: Option<InputCloser>,
}

/// State that an execution's pump, input worker, and control share.
///
/// The library's handle is released only after the pump has seen the end and joined the input
/// worker, so no call can race with its release.
struct ExecShared {
    library: &'static Library,
    handle: Mutex<Option<u64>>,
    completion: Completion,
    closers: Closers,
    /// When the execution's own timeout elapses, if it has one.
    deadline: Option<Instant>,
    /// Whether the timeout elapsed before the outcome arrived and closed the output streams.
    expired: AtomicBool,
    /// Tells the input worker to stop waiting for input credit and drop the caller's source.
    input_stopped: Arc<AtomicBool>,
    /// Serializes abandonment with release accounting and result publication, including while
    /// the pump is releasing a handle that it already took.
    settlement: Mutex<Settlement>,
    /// Where a release failure that the waiter cannot receive goes.
    cleanup: Cleanup,
}

#[derive(Default)]
struct Settlement {
    unattended: bool,
    released: bool,
    release_failure_pending: bool,
}

impl ExecShared {
    /// Stops the input worker: interrupts its read of the caller's source and ends a write that
    /// waits for input credit, after which the worker drops the source, so that a caller still
    /// writing the execution's standard input sees its end.
    fn stop_input(&self) {
        self.input_stopped.store(true, Ordering::Release);
        if let Some(stdin) = &self.closers.stdin {
            stdin.close();
        }
    }

    /// Releases an execution whose pump never started: stops its input worker, kills the
    /// workload, and releases the handle.
    fn abandon(&self, input: Option<InputWorker>) {
        self.stop_input();
        let handle = self
            .handle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(exec) = handle {
            let _ = self.library.exec_cancel(exec);
            if let Some(input) = input {
                let _ = input.join();
            }
            // The caller receives the failure to start, so a release failure goes to the backend.
            let failure = self.library.exec_release(exec).err().map(release_failure);
            self.released(failure);
        }
    }

    /// Records that the handle was released, with a release failure that the waiter cannot
    /// receive, if any.
    fn released(&self, failure: Option<Error>) {
        let mut state = self
            .settlement
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.record_release(&state, failure);
        state.released = true;
    }

    fn record_release(&self, state: &Settlement, failure: Option<Error>) {
        self.cleanup.log.released(
            state.unattended,
            failure.map(|error| CleanupFailure {
                sandbox_id: self.cleanup.sandbox_id.clone(),
                error,
            }),
        );
    }

    /// Records that no caller waits any more. Until release and publication finish, close must
    /// settle this execution. Afterwards, an uncollected release failure goes to the backend.
    /// Returns whether release and publication are still pending.
    fn leave(&self) -> bool {
        let mut state = self
            .settlement
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !state.unattended {
            state.unattended = true;
            if !state.released {
                self.cleanup.log.leave();
            } else if state.release_failure_pending {
                let failure = self.completion.discard().and_then(Result::err);
                self.cleanup.log.released(
                    false,
                    failure.map(|error| CleanupFailure {
                        sandbox_id: self.cleanup.sandbox_id.clone(),
                        error,
                    }),
                );
                state.release_failure_pending = false;
            }
        }
        !state.released
    }
}

/// A failure of the library to release an execution, with what it means.
fn release_failure(error: Error) -> Error {
    Error::backend_error(
        "the agent library cannot release an execution, so the library may keep its resources",
    )
    .with_source(error)
}

/// Copies an execution's output into its sinks until the execution ends, then publishes the
/// outcome. A sink that fails only stops receiving its own stream, and a failed read of the
/// caller's standard input fails the execution. An execution whose timeout cut off output that
/// it had not delivered ends as [`ExecOutcome::TimedOut`].
fn pump(
    shared: Arc<ExecShared>,
    exec: u64,
    stdout: Box<dyn OutputSink>,
    stderr: Box<dyn OutputSink>,
    input: Option<InputWorker>,
) {
    let mut stdout = Some(stdout);
    let mut stderr = Some(stderr);
    let mut cut = false;
    let mut write = |sink: &mut Option<Box<dyn OutputSink>>, data: &[u8]| {
        if let Some(open) = sink
            && open.write(data).is_err()
        {
            *sink = None;
            cut |= shared.expired.load(Ordering::Acquire);
        }
    };
    let mut ended = loop {
        let event = shared.library.exec_next(exec, None, |event| match event {
            Event::Stdout(data) => {
                write(&mut stdout, data);
                None
            }
            Event::Stderr(data) => {
                write(&mut stderr, data);
                None
            }
            Event::Exit(data) => Some(outcome(data)),
        });
        match event {
            Ok(Some(Some(outcome))) => break outcome,
            Ok(_) => {}
            Err(error) => {
                // The workload may still run, and the input worker may wait for input credit that
                // only the workload grants: end the workload before joining the worker.
                let _ = shared.library.exec_cancel(exec);
                break Err(error);
            }
        }
    };
    if cut && matches!(ended, Ok(ExecOutcome::Exited(_) | ExecOutcome::Signaled(_))) {
        ended = Ok(ExecOutcome::TimedOut);
    }
    drop(stdout);
    drop(stderr);
    if let Some(input) = input {
        shared.stop_input();
        if let Ok(Err(error)) = input.join() {
            ended = Err(
                Error::backend_error("cannot read the execution's standard input")
                    .with_source(error),
            );
        }
    }
    let handle = shared
        .handle
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    let Some(exec) = handle else {
        let _ = shared.completion.finish(ended);
        return;
    };
    // A failure that the waiter cannot receive is recorded before the outcome is published, so
    // that a caller whose wait returned finds it.
    let released = shared.library.exec_release(exec);
    let mut state = shared
        .settlement
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    match (released, ended) {
        (Ok(()), ended) => {
            shared.record_release(&state, None);
            // An outcome that arrives after the waiter got another one has nowhere to go; the
            // waiter was told that the outcome is unknown.
            let _ = shared.completion.finish(ended);
        }
        // A successful execution reports the failure to release it through its wait, naming
        // the outcome, unless no caller waits for it any more.
        (Err(error), Ok(outcome)) => {
            let reported = Error::backend_error(format!(
                "the execution {outcome}, but the agent library cannot release it, so the \
                 library may keep its resources"
            ))
            .with_source(error);
            if state.unattended {
                shared.record_release(&state, Some(reported));
                let _ = shared.completion.finish(Ok(outcome));
            } else {
                let unpublished = shared
                    .completion
                    .finish(Err(reported))
                    .and_then(Result::err);
                state.release_failure_pending = unpublished.is_none();
                shared.record_release(&state, unpublished);
            }
        }
        // A failed execution keeps its own failure, and the failure to release it goes to the
        // backend, whether or not the waiter receives the execution's.
        (Err(error), Err(failure)) => {
            shared.record_release(&state, Some(release_failure(error)));
            let _ = shared.completion.finish(Err(failure));
        }
    }
    state.released = true;
}

/// The outcome in an execution's exit event.
fn outcome(data: &[u8]) -> Result<ExecOutcome> {
    match serde_json::from_slice::<ExecEnd>(data) {
        Ok(ExecEnd::Outcome(outcome)) => Ok(outcome),
        Ok(ExecEnd::Error(body)) => Err(Error::new(body.code, body.message)),
        Err(error) => Err(Error::backend_error(
            "the agent library returned a malformed execution end",
        )
        .with_source(error)),
    }
}

/// Copies the caller's standard input to the workload, and closes the workload's input at
/// end-of-file. A failed write stops it, and the execution's outcome then explains why. A failed
/// read kills the workload, which cannot get the rest of its input, and is returned so that the
/// execution fails with it. Once `stopped` is set, a write that the guest does not accept within
/// [`INPUT_WRITE_POLL_MS`] ends it.
fn feed(
    library: &'static Library,
    exec: u64,
    mut source: InputSource,
    stopped: &AtomicBool,
) -> io::Result<()> {
    let mut buffer = vec![0u8; INPUT_CHUNK];
    loop {
        let read = match source.read(&mut buffer) {
            Ok(0) => {
                let _ = library.exec_close_input(exec);
                return Ok(());
            }
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let _ = library.exec_cancel(exec);
                return Err(error);
            }
        };
        let mut pending = &buffer[..read];
        while !pending.is_empty() {
            // A guest that keeps granting a little credit never lets a write time out, so check
            // before each write too.
            if stopped.load(Ordering::Acquire) {
                return Ok(());
            }
            match library.exec_write(exec, pending, Some(INPUT_WRITE_POLL_MS)) {
                Ok(None) if stopped.load(Ordering::Acquire) => return Ok(()),
                Ok(None) => {}
                Ok(Some(0)) | Err(_) => return Ok(()),
                Ok(Some(written)) => pending = &pending[written.min(pending.len())..],
            }
        }
    }
}

/// Control of one execution in the library.
struct AgentExecution {
    shared: Arc<ExecShared>,
}

impl ExecControl for AgentExecution {
    fn abandon(&self) {
        self.shared.leave();
    }

    fn wait(&self) -> Result<ExecOutcome> {
        let shared = &self.shared;
        if let Some(deadline) = shared.deadline {
            if let Some(outcome) = shared.completion.wait_until(deadline) {
                return self.collected(outcome);
            }
            // The library ends a workload that outlives its timeout, but the verdict queues
            // behind the output, where a stream that the caller retains without draining would
            // hold the pump. Close the streams, as cancellation does, so that the verdict arrives.
            shared.expired.store(true, Ordering::Release);
            for closer in [&shared.closers.stdout, &shared.closers.stderr]
                .into_iter()
                .flatten()
            {
                closer.close();
            }
        }
        self.collected(shared.completion.wait())
    }

    fn cancel(&self) -> Result<()> {
        {
            let handle = self
                .shared
                .handle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            // An execution that ended keeps its outcome.
            let Some(exec) = *handle else {
                return Ok(());
            };
            if let Err(error) = self.shared.library.exec_cancel(exec) {
                // Without a cancellation, the end that the waiter needs may never arrive. Unless
                // it arrives within the grace period, the waiter receives this failure instead;
                // the workload is not known to have stopped, so it is not reported as cancelled.
                let unresolved = Error::backend_error(
                    "the execution could not be cancelled, so its outcome is unknown and its \
                     workload may still be running; stop or deprovision the sandbox to end it",
                )
                .with_source(error.to_string());
                self.shared
                    .completion
                    .unresolve(unresolved, Instant::now() + CANCEL_FAILURE_GRACE);
                return Err(error);
            }
        }
        let closers = &self.shared.closers;
        for closer in [&closers.stdout, &closers.stderr].into_iter().flatten() {
            closer.close();
        }
        if let Some(stdin) = &closers.stdin {
            stdin.close();
        }
        Ok(())
    }
}

impl AgentExecution {
    /// Returns `outcome` to the waiter. An outcome collected while the pump still holds the
    /// library's handle did not come from the library, so the pump may still be copying output:
    /// end the streams so that their readers see the end. The pump releases the handle once the
    /// library ends the execution.
    fn collected(&self, outcome: Result<ExecOutcome>) -> Result<ExecOutcome> {
        let shared = &self.shared;
        // The waiter now has an outcome, so a later release failure has to go to the backend.
        let pumping = shared.leave();
        if pumping {
            for closer in [&shared.closers.stdout, &shared.closers.stderr]
                .into_iter()
                .flatten()
            {
                closer.close();
            }
            shared.stop_input();
        }
        outcome
    }
}

impl Drop for AgentExecution {
    /// An execution dropped before it ended has no waiter to report its release failure.
    fn drop(&mut self) {
        self.shared.leave();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::library::fake::{self, Next, Write};
    use super::*;
    use crate::ErrorCode;
    use crate::stream;

    struct Discard;

    impl OutputSink for Discard {
        fn write(&mut self, _: &[u8]) -> io::Result<()> {
            Ok(())
        }
    }

    /// Runs a registered fake execution with `input` as its standard input.
    fn run_with(exec: u64, input: impl Read + Send + 'static) -> Box<dyn ExecControl> {
        let io = ExecIo {
            stdout: Box::new(Discard),
            stderr: Box::new(Discard),
            stdin: Some(InputSource::new(input, InputCloser::new(|| {}))),
        };
        run(fake::library(), exec, io, None, detached()).unwrap()
    }

    /// Runs a registered fake execution that has `timeout` and writes its output to `stdout`.
    fn run_timed(
        exec: u64,
        stdout: Box<dyn OutputSink>,
        timeout: Duration,
    ) -> Box<dyn ExecControl> {
        let io = ExecIo {
            stdout,
            stderr: Box::new(Discard),
            stdin: None,
        };
        run(
            fake::library(),
            exec,
            io,
            Some(Instant::now() + timeout),
            detached(),
        )
        .unwrap()
    }

    /// Where an execution that no backend runs reports its release failures.
    fn detached() -> Cleanup {
        Cleanup {
            sandbox_id: SandboxId::generate().unwrap(),
            log: Arc::default(),
        }
    }

    /// Waits for an execution, failing the test instead of hanging when it never ends.
    fn finish(control: Box<dyn ExecControl>) -> Result<ExecOutcome> {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || sender.send(control.wait()));
        receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("the execution never ended")
    }

    /// Runs a registered fake execution without standard input or a timeout.
    fn run_plain(exec: u64) -> Arc<dyn ExecControl> {
        let io = ExecIo {
            stdout: Box::new(Discard),
            stderr: Box::new(Discard),
            stdin: None,
        };
        Arc::from(run(fake::library(), exec, io, None, detached()).unwrap())
    }

    /// Waits until the fake has recorded `calls` for `exec`.
    fn await_calls(exec: u64, calls: &[&str]) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while fake::calls(exec) != calls {
            assert!(
                Instant::now() < deadline,
                "calls are {:?}, not {calls:?}",
                fake::calls(exec)
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_failed_cancellation_ends_a_waiting_caller_without_claiming_the_workload_stopped() {
        let faults = fake::Faults {
            cancel: true,
            ..fake::Faults::default()
        };
        let exec = fake::execution_with(Next::ExitWhenCancelled, Write::Accept, faults);
        let control = run_plain(exec);
        let (sender, receiver) = mpsc::channel();
        let waiting = Arc::clone(&control);
        thread::spawn(move || sender.send(waiting.wait()));
        thread::sleep(Duration::from_millis(100));

        let refused = control.cancel().unwrap_err();
        assert_eq!(
            refused.message(),
            "the agent library panicked; the operation's effect is unknown"
        );
        let started = Instant::now();
        let error = receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("the wait never ended after the cancellation failed")
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(error.code(), ErrorCode::BackendError, "{error}");
        assert!(error.message().contains("may still be running"), "{error}");
        assert_eq!(
            std::error::Error::source(&error).unwrap().to_string(),
            refused.to_string()
        );

        // The library still owns the execution, so its handle stays unreleased until the
        // library ends it, and is then released once.
        assert_eq!(fake::calls(exec), ["cancel"]);
        fake::end(exec);
        await_calls(exec, &["cancel", "release"]);
    }

    #[test]
    fn a_failed_cancellation_ends_the_output_streams_that_the_waiter_reads() {
        let faults = fake::Faults {
            cancel: true,
            ..fake::Faults::default()
        };
        let exec = fake::execution_with(Next::ExitWhenCancelled, Write::Accept, faults);
        let (stdout, retained) = stream::queue();
        let io = ExecIo {
            stdout: Box::new(stdout),
            stderr: Box::new(Discard),
            stdin: None,
        };
        let control = run(fake::library(), exec, io, None, detached()).unwrap();
        control.cancel().unwrap_err();
        finish(control).unwrap_err();
        // A reader of the stream sees its end instead of waiting for output that never comes.
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || sender.send(retained.read_to_end_blocking()));
        assert!(
            receiver
                .recv_timeout(Duration::from_secs(30))
                .expect("the output stream never ended")
                .is_empty()
        );
        fake::end(exec);
        await_calls(exec, &["cancel", "release"]);
    }

    #[test]
    fn a_failed_cancellation_stops_the_input_worker_so_a_stdin_writer_returns() {
        for write in [Write::BlockUntilCancelled, Write::Trickle] {
            assert_a_stopped_input_worker_releases_the_stdin_writer(write);
        }
    }

    /// Checks that, after a failed cancellation ends the wait, a caller writing the stdin that it
    /// took returns, whether the guest grants no input credit or keeps granting a little.
    fn assert_a_stopped_input_worker_releases_the_stdin_writer(write: Write) {
        use std::io::Write as _;
        let faults = fake::Faults {
            cancel: true,
            ..fake::Faults::default()
        };
        let exec = fake::execution_with(Next::ExitWhenCancelled, write, faults);
        let (reader, mut writer) = io::pipe().unwrap();
        let io = ExecIo {
            stdout: Box::new(Discard),
            stderr: Box::new(Discard),
            stdin: Some(InputSource::from_pipe(reader).unwrap()),
        };
        let control = run(fake::library(), exec, io, None, detached()).unwrap();
        // The caller keeps writing the stdin that it took, which the guest never accepts.
        let (sender, written) = mpsc::channel();
        thread::spawn(move || {
            let chunk = [b'x'; 64 * 1024];
            let kind = loop {
                if let Err(error) = writer.write_all(&chunk) {
                    break error.kind();
                }
            };
            sender.send(kind)
        });
        thread::sleep(Duration::from_millis(100));
        control.cancel().unwrap_err();
        finish(control).unwrap_err();
        let kind = written
            .recv_timeout(Duration::from_secs(30))
            .expect("a writer of the execution's standard input never returned");
        assert_eq!(kind, io::ErrorKind::BrokenPipe);
        fake::end(exec);
        await_calls(exec, &["cancel", "release"]);
    }

    #[test]
    fn an_outcome_that_arrives_after_a_failed_cancellation_is_kept() {
        let faults = fake::Faults {
            cancel: true,
            ..fake::Faults::default()
        };
        let exec = fake::execution_with(Next::ExitWhenCancelled, Write::Accept, faults);
        let control = run_plain(exec);
        control.cancel().unwrap_err();
        fake::end(exec);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || sender.send(control.wait()));
        let outcome = receiver.recv_timeout(Duration::from_secs(30)).unwrap();
        assert_eq!(outcome.unwrap(), ExecOutcome::Cancelled);
        assert_eq!(fake::calls(exec), ["cancel", "release"]);
    }

    #[test]
    fn a_failed_release_fails_the_wait_and_names_the_outcome() {
        let faults = fake::Faults {
            release: true,
            ..fake::Faults::default()
        };
        let exec = fake::execution_with(Next::OutputThenExit(0), Write::Accept, faults);
        let control = run_plain(exec);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || sender.send(control.wait()));
        let error = receiver
            .recv_timeout(Duration::from_secs(30))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::BackendError, "{error}");
        assert!(error.message().contains("cannot release"), "{error}");
        assert!(error.message().contains("exited with status 0"), "{error}");
        assert_eq!(fake::calls(exec), ["release"]);
    }

    #[test]
    fn a_failed_input_read_kills_the_workload_and_fails_the_execution() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("injected input failure"))
            }
        }
        let exec = fake::execution(Next::ExitWhenCancelled, Write::Accept);
        let error = finish(run_with(exec, Failing)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::BackendError);
        assert_eq!(
            error.message(),
            "cannot read the execution's standard input"
        );
        assert_eq!(
            std::error::Error::source(&error).unwrap().to_string(),
            "injected input failure"
        );
        assert_eq!(fake::calls(exec), ["cancel", "release"]);
    }

    #[test]
    fn a_failed_event_kills_the_workload_before_its_input_worker_is_joined() {
        struct Endless;
        impl Read for Endless {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                buffer.fill(b'x');
                Ok(buffer.len())
            }
        }
        let exec = fake::execution(Next::Fail, Write::BlockUntilCancelled);
        let error = finish(run_with(exec, Endless)).unwrap_err();
        assert_eq!(error.message(), "the agent library rejected an argument");
        assert_eq!(fake::calls(exec), ["cancel", "release"]);
    }

    #[test]
    fn input_that_ends_closes_the_workload_input() {
        let exec = fake::execution(Next::ExitWhenCancelled, Write::Accept);
        let control = run_with(exec, &b"input"[..]);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !fake::calls(exec).contains(&"close_input") {
            assert!(
                std::time::Instant::now() < deadline,
                "the input was never closed"
            );
            thread::sleep(Duration::from_millis(10));
        }
        control.cancel().unwrap();
        assert_eq!(finish(control).unwrap(), ExecOutcome::Cancelled);
        assert_eq!(fake::calls(exec), ["close_input", "cancel", "release"]);
    }

    #[test]
    fn the_timeout_closes_a_retained_stream_that_is_not_drained() {
        let exec = fake::execution(Next::OutputThenExit(2 * stream::QUEUE_LIMIT), Write::Accept);
        let (stdout, retained) = stream::queue();
        let started = Instant::now();
        let control = run_timed(exec, Box::new(stdout), Duration::from_millis(200));
        assert_eq!(finish(control).unwrap(), ExecOutcome::TimedOut);
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert_eq!(retained.read_to_end_blocking().len(), stream::QUEUE_LIMIT);
        assert_eq!(fake::calls(exec), ["release"]);
    }

    #[test]
    fn a_timed_execution_that_delivers_its_output_keeps_its_outcome() {
        let exec = fake::execution(Next::OutputThenExit(2 * stream::QUEUE_LIMIT), Write::Accept);
        let control = run_timed(exec, Box::new(Discard), Duration::from_secs(30));
        assert_eq!(finish(control).unwrap(), ExecOutcome::Exited(0));
        assert_eq!(fake::calls(exec), ["release"]);
    }

    #[test]
    fn a_malformed_token_from_the_library_is_a_backend_error() {
        let error = sandbox_id("not a token").unwrap_err();
        assert_eq!(error.code(), ErrorCode::BackendError);
        assert_eq!(
            error.message(),
            "the agent library returned a malformed sandbox token"
        );
        let generated = SandboxId::generate().unwrap();
        assert_eq!(sandbox_id(generated.token()).unwrap(), generated);
    }

    #[test]
    fn the_deadline_follows_the_timeout_that_the_library_receives() {
        assert_eq!(deadline(None), None);
        assert_eq!(deadline(Some(Duration::ZERO)), None);
        let before = Instant::now();
        let rounded = deadline(Some(Duration::from_micros(500))).unwrap();
        assert!(rounded >= before + Duration::from_millis(1));
    }

    #[test]
    fn the_inputs_are_the_model_wire_inputs() {
        let request = ProvisionRequest::new().with_network_proxy("http://127.0.0.1:3128");
        for spec in [SandboxSpec::new(), SandboxSpec::new().with_memory_mib(512)] {
            let input = ProvisionInput {
                request: &request,
                spec: &spec,
            };
            let parsed: wire::ProvisionInput =
                serde_json::from_slice(&serde_json::to_vec(&input).unwrap()).unwrap();
            assert_eq!(
                parsed,
                wire::ProvisionInput {
                    request: request.clone(),
                    spec,
                }
            );
        }
        // The request's stdin mode is a transport option that travels as the input's `stdin`.
        let request = ExecRequest::argv(["cat"])
            .with_cwd("/tmp")
            .with_timeout(Duration::from_millis(1500));
        let input = ExecInput {
            request: &request,
            stdin: true,
        };
        let parsed: wire::ExecInput =
            serde_json::from_slice(&serde_json::to_vec(&input).unwrap()).unwrap();
        assert_eq!(
            parsed,
            wire::ExecInput {
                request,
                stdin: true,
                pty: None,
            }
        );
    }

    /// Returns a backend over the fake library, which refuses every image call as one for an
    /// unknown handle, that honors image references or not.
    fn fake_backend(image_reference: bool) -> AgentBackend {
        fake_backend_on(image_reference, 0)
    }

    /// Returns a backend as [ake_backend] does, whose host is `host`.
    fn fake_backend_on(image_reference: bool, host: u64) -> AgentBackend {
        let runtime = RuntimeSource::Files(RuntimeFiles {
            openvmm: PathBuf::from("openvmm"),
            kernel: PathBuf::from("vmlinux"),
            initrd: PathBuf::from("initramfs"),
            approved_sha256: None,
        });
        let mut capabilities = Capabilities::new("agent");
        capabilities.spec.image_reference = image_reference;
        let digests = RuntimeDigests {
            openvmm: [0; 32],
            kernel: [0; 32],
            initrd: [0; 32],
        };
        AgentBackend {
            config: AgentConfig::new(SetupConfig::new("state", runtime), "library", [0; 32]),
            library: fake::library(),
            host,
            capabilities,
            info: HostInfo::new(None, digests),
            closed: false,
            cleanup: Arc::default(),
        }
    }

    #[test]
    fn only_a_backend_that_honors_references_registers_them() {
        // A malformed reference fails first, then one that the backend does not honor, before
        // either reaches the library.
        let refusing = fake_backend(false);
        let error = refusing
            .register_reference("registry.example/a :1", ImageDigest::Compute)
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::MalformedRequest, "{error}");
        let error = refusing
            .register_reference("registry.example/a:1", ImageDigest::Compute)
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::PolicyValidation, "{error}");
        assert_eq!(
            error.message(),
            "the agent backend cannot register image references"
        );

        // A backend that honors references passes them to the library.
        let error = fake_backend(true)
            .register_reference("registry.example/a:1", ImageDigest::Compute)
            .unwrap_err();
        assert!(error.message().contains("unknown handle"), "{error}");
    }

    #[test]
    fn close_closes_the_host_once_and_drop_does_not_close_it_again() {
        let host = fake::host();
        fake_backend_on(false, host).close().unwrap();
        assert_eq!(fake::host_closes(host), 1);
    }

    #[test]
    fn close_returns_the_failure_that_drop_discards() {
        // A host that the library refuses to close, as a released one.
        let host = fake::host();
        fake_backend_on(false, host).close().unwrap();
        let error = fake_backend_on(false, host).close().unwrap_err();
        assert_eq!(error.code(), ErrorCode::BackendError, "{error}");
        assert_eq!(error.message(), "cannot close the agent host");
        assert_eq!(
            std::error::Error::source(&error).unwrap().to_string(),
            "backend_error: the agent library rejected a released or unknown handle"
        );
        assert_eq!(fake::host_closes(host), 2);
    }

    #[test]
    fn a_dropped_backend_closes_its_host() {
        let host = fake::host();
        drop(fake_backend_on(false, host));
        assert_eq!(fake::host_closes(host), 1);
    }

    /// Starts an execution through `backend`, whose fake `host` starts it as `next` and `faults`
    /// say, and returns its control and the library's handle.
    fn exec_on(
        backend: &AgentBackend,
        host: u64,
        next: Next,
        faults: fake::Faults,
    ) -> (Box<dyn ExecControl>, u64) {
        fake::host_executes(host, next, Write::Accept, faults);
        let io = ExecIo {
            stdout: Box::new(Discard),
            stderr: Box::new(Discard),
            stdin: None,
        };
        let control = backend
            .exec(
                &SandboxId::generate().unwrap(),
                &ExecRequest::command_line("true"),
                io,
            )
            .unwrap();
        let exec = *fake::started(host).last().unwrap();
        (control, exec)
    }

    /// The messages of `error` and its causes.
    fn chain(error: &Error) -> String {
        let mut messages = vec![error.to_string()];
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            messages.push(cause.to_string());
            source = cause.source();
        }
        messages.join(" <- ")
    }

    #[test]
    fn a_failed_execution_keeps_its_error_and_close_reports_its_failed_release() {
        let host = fake::host();
        let backend = fake_backend_on(false, host);
        let faults = fake::Faults {
            release: true,
            ..fake::Faults::default()
        };
        let (control, exec) = exec_on(&backend, host, Next::Fail, faults);
        // The waiter receives the execution's own failure, unchanged.
        let error = finish(control).unwrap_err();
        assert_eq!(error.message(), "the agent library rejected an argument");
        assert_eq!(fake::calls(exec), ["cancel", "release"]);
        // The release failure is not lost: close reports it.
        let error = backend.close().unwrap_err();
        let chain = chain(&error);
        assert!(chain.contains("cannot release"), "{chain}");
        assert!(chain.contains("panicked"), "{chain}");
        assert_eq!(fake::host_closes(host), 1);
    }

    #[test]
    fn a_release_after_an_unknown_outcome_is_reported_by_close() {
        let host = fake::host();
        let backend = fake_backend_on(false, host);
        let faults = fake::Faults {
            cancel: true,
            release: true,
        };
        let (control, exec) = exec_on(&backend, host, Next::ExitWhenCancelled, faults);
        control.cancel().unwrap_err();
        // The waiter is told promptly that the outcome is unknown, before the release happens.
        let error = finish(control).unwrap_err();
        assert!(error.message().contains("may still be running"), "{error}");
        assert_eq!(fake::calls(exec), ["cancel"]);
        // The library ends the execution later, and its release fails.
        fake::end(exec);
        let error = backend.close().unwrap_err();
        let chain = chain(&error);
        assert!(chain.contains("cannot release"), "{chain}");
        assert_eq!(fake::calls(exec), ["cancel", "release"]);
    }

    #[test]
    fn a_successful_execution_whose_release_fails_reports_it_once_through_wait() {
        let host = fake::host();
        let backend = fake_backend_on(false, host);
        let faults = fake::Faults {
            release: true,
            ..fake::Faults::default()
        };
        let (control, exec) = exec_on(&backend, host, Next::OutputThenExit(0), faults);
        let error = finish(control).unwrap_err();
        assert!(error.message().contains("cannot release"), "{error}");
        assert!(error.message().contains("exited with status 0"), "{error}");
        assert_eq!(fake::calls(exec), ["release"]);
        // Already reported by wait, so close does not report it again, and returns at once.
        let started = Instant::now();
        backend.close().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cleanup_failures_can_be_taken_while_the_backend_stays_open() {
        let host = fake::host();
        let backend = fake_backend_on(false, host);
        let faults = fake::Faults {
            release: true,
            ..fake::Faults::default()
        };
        let (control, exec) = exec_on(&backend, host, Next::Fail, faults);
        finish(control).unwrap_err();
        // Recorded before the waiter was woken, so it is there as soon as wait returns.
        let failures = backend.take_cleanup_failures();
        assert_eq!(failures.len(), 1);
        let chain = chain(&failures[0].error);
        assert!(chain.contains("cannot release"), "{chain}");
        assert!(chain.contains("panicked"), "{chain}");
        assert!(backend.take_cleanup_failures().is_empty());
        // Taken already, so close does not report it again.
        backend.close().unwrap();
        assert_eq!(fake::calls(exec), ["cancel", "release"]);
    }

    #[test]
    fn close_reports_an_execution_that_the_library_still_holds() {
        let host = fake::host();
        let backend = fake_backend_on(false, host);
        let faults = fake::Faults {
            cancel: true,
            ..fake::Faults::default()
        };
        let (control, exec) = exec_on(&backend, host, Next::ExitWhenCancelled, faults);
        control.cancel().unwrap_err();
        finish(control).unwrap_err();
        // The library never ends the execution, so close waits, then reports it.
        let started = Instant::now();
        let error = backend.close().unwrap_err();
        assert!(started.elapsed() >= RELEASE_SETTLE);
        assert!(started.elapsed() < RELEASE_SETTLE + Duration::from_secs(5));
        assert!(
            error.message().contains("still held by the library"),
            "{error}"
        );
        assert_eq!(fake::host_closes(host), 1);
        // Released once the library ends it.
        fake::end(exec);
        await_calls(exec, &["cancel", "release"]);
    }

    #[test]
    fn close_does_not_wait_for_an_execution_that_a_caller_still_waits_for() {
        let host = fake::host();
        let backend = fake_backend_on(false, host);
        let faults = fake::Faults {
            release: true,
            ..fake::Faults::default()
        };
        let (control, exec) = exec_on(&backend, host, Next::ExitWhenCancelled, faults);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || sender.send(control.wait()));
        // The execution runs on, as in 0.1.1: close neither waits for it nor fails because of it.
        let started = Instant::now();
        backend.close().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(fake::host_closes(host), 1);
        // Its waiter still receives its outcome and the failure to release it, once.
        fake::end(exec);
        let error = receiver
            .recv_timeout(Duration::from_secs(30))
            .unwrap()
            .unwrap_err();
        assert!(error.message().contains("cannot release"), "{error}");
        assert!(error.message().contains("cancelled"), "{error}");
        assert_eq!(fake::calls(exec), ["release"]);
    }

    #[test]
    fn a_dropped_execution_whose_release_fails_is_reported_by_close() {
        let host = fake::host();
        let backend = fake_backend_on(false, host);
        let faults = fake::Faults {
            release: true,
            ..fake::Faults::default()
        };
        let (control, exec) = exec_on(&backend, host, Next::ExitWhenCancelled, faults);
        drop(control);
        // Nobody waits for it, so close waits for its release, which the library allows once it
        // ends the execution.
        let ender = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            fake::end(exec);
        });
        let started = Instant::now();
        let error = backend.close().unwrap_err();
        assert!(started.elapsed() < RELEASE_SETTLE, "{error}");
        let chain = chain(&error);
        assert!(chain.contains("cannot release"), "{chain}");
        assert!(chain.contains("cancelled"), "{chain}");
        assert!(!chain.contains("still held"), "{chain}");
        ender.join().unwrap();
        assert_eq!(fake::calls(exec), ["release"]);
    }

    #[test]
    fn dropping_the_control_during_release_keeps_close_waiting_for_settlement() {
        for release_fails in [false, true] {
            let host = fake::host();
            let backend = fake_backend_on(false, host);
            let faults = fake::Faults {
                release: release_fails,
                ..fake::Faults::default()
            };
            let (control, exec) = exec_on(&backend, host, Next::ExitWhenCancelled, faults);
            fake::hold_release(exec, true);
            fake::end(exec);
            await_calls(exec, &["release"]);
            drop(control);
            assert_eq!(backend.cleanup.lock().unattended, 1);
            let log = Arc::clone(&backend.cleanup);
            let started = Instant::now();
            let releaser = thread::spawn(move || {
                thread::sleep(Duration::from_millis(200));
                fake::hold_release(exec, false);
            });
            let closed = backend.close();
            assert!(started.elapsed() >= Duration::from_millis(200));
            assert!(started.elapsed() < RELEASE_SETTLE);
            if release_fails {
                let chain = chain(&closed.unwrap_err());
                assert!(chain.contains("cannot release"), "{chain}");
                assert!(!chain.contains("still held"), "{chain}");
            } else {
                closed.unwrap();
            }
            releaser.join().unwrap();
            assert!(log.take().is_empty());
            assert_eq!(log.lock().unattended, 0);
            assert_eq!(fake::calls(exec), ["release"]);
            assert_eq!(fake::host_closes(host), 1);
        }
    }

    #[test]
    fn dropping_an_execution_with_a_retained_canceller_reports_its_failed_release() {
        let host = fake::host();
        let backend = fake_backend_on(false, host);
        let faults = fake::Faults {
            release: true,
            ..fake::Faults::default()
        };
        let (control, exec) = exec_on(&backend, host, Next::ExitWhenCancelled, faults);
        let (stdout, _) = io::pipe().unwrap();
        let (stderr, _) = io::pipe().unwrap();
        let execution = crate::Execution::new(stdout, stderr, None, Arc::from(control));
        let canceller = execution.canceller();
        drop(execution);
        assert_eq!(backend.cleanup.lock().unattended, 1);
        assert!(fake::calls(exec).is_empty(), "drop must not cancel");
        // A retained cancellation handle still works, but is no longer treated as a waiter.
        canceller.cancel().unwrap();
        let error = backend.close().unwrap_err();
        assert!(chain(&error).contains("cannot release"), "{error}");
        assert_eq!(fake::calls(exec), ["cancel", "release"]);
        drop(canceller);
    }

    #[test]
    fn dropping_after_release_reports_an_uncollected_release_failure_once() {
        let host = fake::host();
        let backend = fake_backend_on(false, host);
        let sandbox_id = SandboxId::generate().unwrap();
        let exec = fake::execution_with(
            Next::OutputThenExit(0),
            Write::Accept,
            fake::Faults {
                release: true,
                ..fake::Faults::default()
            },
        );
        let shared = Arc::new(ExecShared {
            library: fake::library(),
            handle: Mutex::new(Some(exec)),
            completion: Completion::default(),
            closers: Closers {
                stdout: None,
                stderr: None,
                stdin: None,
            },
            deadline: None,
            expired: AtomicBool::new(false),
            input_stopped: Arc::new(AtomicBool::new(false)),
            settlement: Mutex::default(),
            cleanup: Cleanup {
                sandbox_id: sandbox_id.clone(),
                log: Arc::clone(&backend.cleanup),
            },
        });
        // Run the pump inline so release and publication have both finished before dropping.
        pump(
            Arc::clone(&shared),
            exec,
            Box::new(Discard),
            Box::new(Discard),
            None,
        );
        assert!(backend.take_cleanup_failures().is_empty());
        drop(AgentExecution { shared });
        let failures = backend.take_cleanup_failures();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].sandbox_id, sandbox_id);
        let chain = chain(&failures[0].error);
        assert!(chain.contains("cannot release"), "{chain}");
        assert!(chain.contains("exited with status 0"), "{chain}");
        assert!(chain.contains("panicked"), "{chain}");
        assert!(backend.take_cleanup_failures().is_empty());
        backend.close().unwrap();
        assert_eq!(fake::calls(exec), ["release"]);
    }
}
