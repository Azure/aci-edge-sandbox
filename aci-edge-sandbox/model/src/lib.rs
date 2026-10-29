// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Serializable data model of the edge sandbox lifecycle.
//!
//! These are the requests, results, capabilities, and errors of `aci_edge_sandboxes`, without any
//! backend. A backend that runs in a separate component, such as a native host library, uses the
//! same definitions to read and write the JSON that crosses its boundary, so both sides agree on
//! every field. `aci_edge_sandboxes` re-exports the types, so its users need not depend on this
//! crate directly.

pub mod capabilities;
pub mod error;
pub mod hex;
pub mod id;
pub mod image;
pub mod model;
pub mod outcome;
mod rfc3339;
pub mod setup;
pub mod snapshot;
pub mod spec;
pub mod validate;
pub mod wire;

pub use nvx::net::cidr;

pub use capabilities::{
    Capabilities, ExecCapabilities, FilesystemCapabilities, NetworkCapabilities, SpecCapabilities,
};
pub use error::{Error, ErrorBody, ErrorCode, Result};
pub use id::SandboxId;
pub use image::{ImageDigest, ImageId, RegisteredImage};
pub use model::{
    Access, Command, DeprovisionResult, EgressPolicy, ExecRequest, FilesystemPolicy,
    ForwardProtocol, HostLoopbackForward, IngressPolicy, Metadata, MicrovmConfig, MicrovmProvision,
    NetworkPeer, NetworkPolicy, NetworkPort, NetworkRule, ProcessSpec, Protocol, ProvisionRequest,
    ProvisionResult, RuntimeConfig, StartResult, StdinMode, StopResult,
};
pub use outcome::{ExecFailure, ExecOutcome};
pub use setup::{
    BundleSource, CpuProfile, Diagnostics, ExecSettings, GuestSessionPolicy, Hypervisor,
    ImageSettings, PlatformSnapshots, ReferenceSettings, RuntimeDigests, RuntimeFiles,
    RuntimeSource, SandboxDefaults, SetupConfig, Timeouts,
};
pub use snapshot::{
    BootMode, ColdReason, DisabledReason, PlatformSnapshotsInfo, TemplateClass, TemplateGeneration,
    TemplateInfo, TemplateKey, TemplateState,
};
pub use spec::{ImageSource, Resources, SandboxSpec};
