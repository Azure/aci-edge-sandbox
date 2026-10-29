// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Rust interface to the ACI Edge Sandboxes lifecycle.
//!
//! This crate exposes the five operations of the state-aware sandbox lifecycle:
//!
//! | Operation | Transition | Result |
//! | --- | --- | --- |
//! | [`AciEdgeSandbox::provision`] | (none) → provisioned | opaque [`SandboxId`] and optional metadata |
//! | [`AciEdgeSandbox::start`] | provisioned → running | optional metadata |
//! | [`AciEdgeSandbox::exec`] | running → running | live output streams and an [`ExecOutcome`] |
//! | [`AciEdgeSandbox::stop`] | running → provisioned | optional metadata |
//! | [`AciEdgeSandbox::deprovision`] | provisioned → (none) | optional metadata; the ID becomes stale |
//!
//! Every operation is implemented by a pluggable [`Backend`]. The default backend,
//! [`agent::AgentBackend`], forwards the lifecycle to a separately supplied native library,
//! `aci_edge_agent`, which drives OpenVMM and an image-backed guest. [`AciEdgeSandbox`]
//! validates each
//! request in a fixed order before the backend acts on it: structural errors
//! ([`ErrorCode::MalformedRequest`], [`ErrorCode::MalformedId`]) come first, then requests the
//! backend cannot honor ([`ErrorCode::PolicyValidation`]), then backend-specific failures.
//! Rejected requests never run anything.
//!
//! # Example
//!
//! ```no_run
//! # #[cfg(feature = "agent-bundled")]
//! # fn main() -> aci_edge_sandboxes::Result<()> {
//! use aci_edge_sandboxes::AciEdgeSandbox;
//! use aci_edge_sandboxes::agent::AgentConfig;
//!
//! // Opens a host through the edge agent bundle that the build staged beside the executable.
//! let client = AciEdgeSandbox::agent(AgentConfig::bundled("state-root")?)?;
//! client.probe()?;
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "agent-bundled"))]
//! # fn main() {}
//! ```
//!
//! `examples/agent_lifecycle.rs` runs the whole lifecycle against a guest image.
//!
//! # Features
//!
//! - `agent` (default): the [`agent`] backend, a thin client of a separately supplied native
//!   library that owns the sandbox lifecycle, and the host side of [`nvx`].
//! - `agent-bundled`: `agent`, with an edge agent bundle that the build script downloads and
//!   stages beside the executables; see `agent::BundledAgent`.
//! - `async`: Tokio wrappers ([`AsyncAciEdgeSandbox`], [`AsyncExecution`]) around the synchronous core.
//! - `testing`: an in-memory [`testing::MockBackend`] for consumers' own tests.

// The data model lives in `aci_edge_sandboxes_model`; these keep its modules at their crate paths.
pub(crate) use aci_edge_sandboxes_model::{
    capabilities, error, id, model, snapshot, spec, validate,
};
/// The NVX crate: the data model takes its hypervisors, CIDRs, and hostname and guest-network rules
/// from it, and its `resolve_guest_path` gives the guest paths of mapped host paths.
pub use nvx;

mod backend;
mod client;
mod exec;
mod input;
mod stream;

#[cfg(feature = "agent")]
pub mod agent;
#[cfg(feature = "async")]
mod async_api;
#[cfg(feature = "testing")]
pub mod testing;

#[cfg(feature = "async")]
pub use async_api::{AsyncAciEdgeSandbox, AsyncExecution, InputStream, OutputStream};
pub use backend::{Backend, ExecControl, ExecIo, OutputCloser, OutputSink};
pub use capabilities::{
    Capabilities, ExecCapabilities, FilesystemCapabilities, NetworkCapabilities, SpecCapabilities,
};
pub use client::AciEdgeSandbox;
pub use error::{Error, ErrorBody, ErrorCode, Result};
pub use exec::{Canceller, ExecFailure, ExecOutcome, ExecOutput, Execution};
pub use id::SandboxId;
pub use input::{InputCloser, InputSource};
pub use model::{
    Access, Command, DeprovisionResult, EgressPolicy, ExecRequest, FilesystemPolicy,
    ForwardProtocol, HostLoopbackForward, IngressPolicy, Metadata, MicrovmConfig, MicrovmProvision,
    NetworkPeer, NetworkPolicy, NetworkPort, NetworkRule, ProcessSpec, Protocol, ProvisionRequest,
    ProvisionResult, RuntimeConfig, StartResult, StdinMode, StopResult,
};
pub use snapshot::{BootMode, ColdReason, TemplateClass, TemplateGeneration, TemplateKey};
pub use spec::{ImageSource, Resources, SandboxSpec};
