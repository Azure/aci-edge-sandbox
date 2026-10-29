// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Configuration that a native host takes once, when it opens.
//!
//! [`SetupConfig`] names the runtime bundle, the state root, and the hypervisor, together with
//! defaults that every sandbox inherits unless its [`SandboxSpec`](crate::SandboxSpec) overrides
//! them. Nothing here is specific to one sandbox.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::hex;
use crate::image::ImageDigest;
use crate::spec::{ImageSource, Resources};

/// Hypervisor that runs the sandbox VMs: NVX's, whose serde form is its OpenVMM spelling.
pub use nvx::Hypervisor;

/// Configuration that a native host takes once, when it opens.
///
/// Every field except [`state_root`](Self::state_root) and [`runtime`](Self::runtime) has a
/// default, so `{"stateRoot": …, "runtime": …}` is a complete configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct SetupConfig {
    /// Format of this configuration; [`SetupConfig::SCHEMA_VERSION`].
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    /// Directory that holds every sandbox's state. No mapped host path may expose it.
    pub state_root: PathBuf,
    /// The VMM and guest artifacts.
    pub runtime: RuntimeSource,
    /// Hypervisor; `None` selects the platform default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hypervisor: Option<Hypervisor>,
    /// CPU profile of every guest.
    #[serde(default)]
    pub cpu_profile: CpuProfile,
    /// Operation deadlines.
    #[serde(default)]
    pub timeouts: Timeouts,
    /// How long the host keeps a guest session.
    #[serde(default)]
    pub guest_session: GuestSessionPolicy,
    /// Values that sandboxes inherit unless their spec sets them.
    #[serde(default)]
    pub defaults: SandboxDefaults,
    /// Image cache settings.
    #[serde(default)]
    pub images: ImageSettings,
    /// Flow control of streamed executions.
    #[serde(default)]
    pub exec: ExecSettings,
    /// Diagnostic switches.
    #[serde(default)]
    pub diagnostics: Diagnostics,
    /// Platform snapshots: templates that sandboxes start as clones of. `None` disables them, as
    /// `enabled: false` does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_snapshots: Option<PlatformSnapshots>,
}

fn schema_version() -> u32 {
    SetupConfig::SCHEMA_VERSION
}

impl SetupConfig {
    /// The configuration format this crate reads and writes.
    pub const SCHEMA_VERSION: u32 = 1;

    /// Creates a configuration with the default value of every optional field.
    pub fn new(state_root: impl Into<PathBuf>, runtime: RuntimeSource) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            state_root: state_root.into(),
            runtime,
            hypervisor: None,
            cpu_profile: CpuProfile::default(),
            timeouts: Timeouts::default(),
            guest_session: GuestSessionPolicy::default(),
            defaults: SandboxDefaults::default(),
            images: ImageSettings::default(),
            exec: ExecSettings::default(),
            diagnostics: Diagnostics::default(),
            platform_snapshots: None,
        }
    }

    /// Selects the hypervisor.
    #[must_use]
    pub fn with_hypervisor(mut self, hypervisor: Hypervisor) -> Self {
        self.hypervisor = Some(hypervisor);
        self
    }

    /// Selects the CPU profile.
    #[must_use]
    pub fn with_cpu_profile(mut self, profile: CpuProfile) -> Self {
        self.cpu_profile = profile;
        self
    }

    /// Sets the defaults that sandboxes inherit.
    #[must_use]
    pub fn with_defaults(mut self, defaults: SandboxDefaults) -> Self {
        self.defaults = defaults;
        self
    }

    /// Sets the image settings.
    #[must_use]
    pub fn with_images(mut self, images: ImageSettings) -> Self {
        self.images = images;
        self
    }

    /// Sets the diagnostic switches.
    #[must_use]
    pub fn with_diagnostics(mut self, diagnostics: Diagnostics) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    /// Sets the platform snapshots.
    #[must_use]
    pub fn with_platform_snapshots(mut self, platform_snapshots: PlatformSnapshots) -> Self {
        self.platform_snapshots = Some(platform_snapshots);
        self
    }

    /// Checks the configuration's structure without touching the host, reporting
    /// [`ErrorCode::MalformedRequest`](crate::ErrorCode::MalformedRequest).
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != Self::SCHEMA_VERSION {
            return Err(Error::malformed_request(format!(
                "setup schemaVersion {} is not supported; use {}",
                self.schema_version,
                Self::SCHEMA_VERSION
            )));
        }
        if self.state_root.as_os_str().is_empty() {
            return Err(Error::malformed_request("setup stateRoot is empty"));
        }
        self.runtime.validate()?;
        self.timeouts.validate()?;
        self.guest_session.validate()?;
        self.defaults.validate()?;
        self.images.validate()?;
        self.exec.validate()?;
        if let Some(platform_snapshots) = &self.platform_snapshots {
            platform_snapshots.validate(&self.defaults)?;
        }
        Ok(())
    }
}

/// Where the VMM and guest artifacts come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub enum RuntimeSource {
    /// A release bundle: a directory with a `SOURCE-MANIFEST.json`, trusted through the
    /// manifest's SHA-256 digest.
    Bundle(BundleSource),
    /// Explicit files, each optionally pinned to an approved SHA-256 digest.
    Files(RuntimeFiles),
}

impl RuntimeSource {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Bundle(bundle) => {
                if bundle.path.as_os_str().is_empty() {
                    return Err(Error::malformed_request(
                        "setup runtime.bundle.path is empty",
                    ));
                }
                Ok(())
            }
            Self::Files(files) => {
                for (path, field) in [
                    (&files.openvmm, "openvmm"),
                    (&files.kernel, "kernel"),
                    (&files.initrd, "initrd"),
                ] {
                    if path.as_os_str().is_empty() {
                        return Err(Error::malformed_request(format!(
                            "setup runtime.files.{field} is empty"
                        )));
                    }
                }
                Ok(())
            }
        }
    }
}

/// A release bundle and the digest that makes it trusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BundleSource {
    /// Directory that holds `SOURCE-MANIFEST.json`, `bin/`, and `guest/`.
    pub path: PathBuf,
    /// SHA-256 of `SOURCE-MANIFEST.json`, written as 64 lowercase hexadecimal digits.
    #[serde(with = "hex::sha256")]
    pub manifest_sha256: [u8; 32],
}

/// Explicit VMM and guest files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeFiles {
    /// The OpenVMM executable.
    pub openvmm: PathBuf,
    /// The guest kernel.
    pub kernel: PathBuf,
    /// The guest initramfs.
    pub initrd: PathBuf,
    /// Approved digests; the host refuses files that differ. `None` accepts the files as found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_sha256: Option<RuntimeDigests>,
}

/// SHA-256 digests of the runtime files, written as 64 lowercase hexadecimal digits each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeDigests {
    /// Digest of the OpenVMM executable.
    #[serde(with = "hex::sha256")]
    pub openvmm: [u8; 32],
    /// Digest of the guest kernel.
    #[serde(with = "hex::sha256")]
    pub kernel: [u8; 32],
    /// Digest of the guest initramfs.
    #[serde(with = "hex::sha256")]
    pub initrd: [u8; 32],
}

/// CPU profile of the guests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CpuProfile {
    /// The built-in profile of the host's CPU model.
    #[default]
    Auto,
    /// A profile derived from the host CPU, for hosts that no built-in profile serves.
    Host,
}

/// Operation deadlines, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Timeouts {
    /// From launch until the guest is ready.
    #[serde(default = "default_start_ms")]
    pub start_ms: u64,
    /// A control operation on a running guest.
    #[serde(default = "default_control_ms")]
    pub control_ms: u64,
    /// From the shutdown request until the VM is gone, before it is forced off.
    #[serde(default = "default_stop_ms")]
    pub stop_ms: u64,
    /// How long after an execution's own timeout the host waits for the guest's verdict.
    #[serde(default = "default_exec_grace_ms")]
    pub exec_grace_ms: u64,
}

fn default_start_ms() -> u64 {
    60_000
}

fn default_control_ms() -> u64 {
    60_000
}

fn default_stop_ms() -> u64 {
    30_000
}

fn default_exec_grace_ms() -> u64 {
    30_000
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            start_ms: default_start_ms(),
            control_ms: default_control_ms(),
            stop_ms: default_stop_ms(),
            exec_grace_ms: default_exec_grace_ms(),
        }
    }
}

impl Timeouts {
    /// Longest accepted deadline: one day.
    pub const MAX_MS: u64 = 24 * 60 * 60 * 1000;

    fn validate(&self) -> Result<()> {
        for (value, field) in [
            (self.start_ms, "startMs"),
            (self.control_ms, "controlMs"),
            (self.stop_ms, "stopMs"),
            (self.exec_grace_ms, "execGraceMs"),
        ] {
            if value == 0 || value > Self::MAX_MS {
                return Err(Error::malformed_request(format!(
                    "setup timeouts.{field} must be between 1 and {}",
                    Self::MAX_MS
                )));
            }
        }
        Ok(())
    }
}

/// How long the host keeps a running sandbox's guest session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GuestSessionPolicy {
    /// Keeps the session while the sandbox runs. The VM accepts one host session at a time, so
    /// another process's operations on the sandbox wait until this process releases it.
    #[serde(default = "default_hold")]
    pub hold: bool,
    /// With `hold` off, releases a session after it has been idle this long.
    #[serde(default = "default_linger_ms")]
    pub linger_ms: u64,
}

fn default_hold() -> bool {
    true
}

fn default_linger_ms() -> u64 {
    25
}

impl Default for GuestSessionPolicy {
    fn default() -> Self {
        Self {
            hold: default_hold(),
            linger_ms: default_linger_ms(),
        }
    }
}

impl GuestSessionPolicy {
    fn validate(&self) -> Result<()> {
        if self.linger_ms > Timeouts::MAX_MS {
            return Err(Error::malformed_request(format!(
                "setup guestSession.lingerMs must not exceed {}",
                Timeouts::MAX_MS
            )));
        }
        Ok(())
    }
}

/// Values that a sandbox inherits unless its spec sets them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SandboxDefaults {
    /// Image of sandboxes whose spec names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSource>,
    /// How the host registers [`image`](Self::image) when it is a local path. The host registers
    /// it when it opens, reading the file only if no registration that the file still matches
    /// exists and the digest is not [`ImageDigest::Trusted`].
    #[serde(default, skip_serializing_if = "ImageDigest::is_compute")]
    pub image_digest: ImageDigest,
    /// Guest resources.
    #[serde(default = "default_resources")]
    pub resources: Resources,
    /// The guest's `ADDRESS/PREFIX` on its private network.
    #[serde(default = "default_guest_network")]
    pub guest_network: String,
    /// The guest's hostname.
    #[serde(default = "default_hostname")]
    pub hostname: String,
}

/// Default guest memory, in MiB.
pub const DEFAULT_MEMORY_MIB: u32 = 256;
/// Default number of virtual processors.
pub const DEFAULT_VCPUS: u32 = 1;
/// Default guest network.
pub const DEFAULT_GUEST_NETWORK: &str = "10.0.0.2/24";
/// Default guest hostname.
pub const DEFAULT_HOSTNAME: &str = "nvx-sandbox";

fn default_resources() -> Resources {
    Resources {
        vcpus: Some(DEFAULT_VCPUS),
        memory_mib: Some(DEFAULT_MEMORY_MIB),
    }
}

fn default_guest_network() -> String {
    DEFAULT_GUEST_NETWORK.to_owned()
}

fn default_hostname() -> String {
    DEFAULT_HOSTNAME.to_owned()
}

impl Default for SandboxDefaults {
    fn default() -> Self {
        Self {
            image: None,
            image_digest: ImageDigest::Compute,
            resources: default_resources(),
            guest_network: default_guest_network(),
            hostname: default_hostname(),
        }
    }
}

impl SandboxDefaults {
    /// Sets the default image.
    #[must_use]
    pub fn with_image(mut self, image: ImageSource) -> Self {
        self.image = Some(image);
        self
    }

    /// Sets how the host registers a default image that is a local path.
    #[must_use]
    pub fn with_image_digest(mut self, digest: ImageDigest) -> Self {
        self.image_digest = digest;
        self
    }

    fn validate(&self) -> Result<()> {
        if let Some(image) = &self.image {
            image.validate("setup defaults.image")?;
        }
        if self.resources.vcpus.is_none() || self.resources.memory_mib.is_none() {
            return Err(Error::malformed_request(
                "setup defaults.resources must set vcpus and memoryMib",
            ));
        }
        self.resources.validate("setup defaults.resources")?;
        crate::spec::guest_network(&self.guest_network, "setup defaults.guestNetwork")?;
        crate::spec::hostname(&self.hostname, "setup defaults.hostname")
    }
}

/// Image cache settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageSettings {
    /// Images to make available when the host opens: the host registers local paths and checks
    /// registered digests. Registry references are reserved for a later version.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prefetch: Vec<ImageSource>,
    /// How a host that pulls registry references materializes them.
    #[serde(default, skip_serializing_if = "ReferenceSettings::is_default")]
    pub references: ReferenceSettings,
    /// Maximum combined length of retained VHDs in the managed image cache, in bytes.
    /// Temporary conversion data is excluded. Omission or `None` leaves the cache unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cache_capacity_bytes: Option<u64>,
}

impl ImageSettings {
    /// Sets the maximum combined length of retained image-cache VHDs, in bytes.
    #[must_use]
    pub fn with_max_cache_capacity_bytes(mut self, max_cache_capacity_bytes: u64) -> Self {
        self.max_cache_capacity_bytes = Some(max_cache_capacity_bytes);
        self
    }

    /// Sets how the host materializes registry references.
    #[must_use]
    pub fn with_references(mut self, references: ReferenceSettings) -> Self {
        self.references = references;
        self
    }

    fn validate(&self) -> Result<()> {
        if self.max_cache_capacity_bytes == Some(0) {
            return Err(Error::malformed_request(
                "setup images.maxCacheCapacityBytes must be greater than zero",
            ));
        }

        for (index, image) in self.prefetch.iter().enumerate() {
            image.validate(&format!("setup images.prefetch[{index}]"))?;
        }
        self.references.validate()
    }
}

/// How a host that pulls registry references materializes them.
///
/// Such a host registers an [`ImageSource::Reference`] when the caller registers it, or when the
/// host opens with it as the default image: it converts the reference into a local disk image
/// with an external tool, once, and keeps the result by the reference's text. Sandboxes that name
/// the reference boot that image, provisioning never pulls, and the host does not check a tag
/// again. A moving tag such as `latest` therefore keeps naming the content that was first pulled;
/// use a version tag or a digest (`repository@sha256:…`) to control what a sandbox boots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReferenceSettings {
    /// Longest time that materializing one reference may take, in milliseconds. The host abandons
    /// a pull that takes longer.
    #[serde(default = "default_pull_timeout_ms")]
    pub pull_timeout_ms: u64,
    /// Host-side Docker-compatible JSON credentials file. `None` keeps anonymous pulls.
    /// The host reads it only when it must pull a reference, not for local images or registered
    /// references.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_file: Option<PathBuf>,
}

/// Default limit on materializing one reference: 30 minutes.
pub const DEFAULT_PULL_TIMEOUT_MS: u64 = 30 * 60 * 1000;

fn default_pull_timeout_ms() -> u64 {
    DEFAULT_PULL_TIMEOUT_MS
}

impl Default for ReferenceSettings {
    fn default() -> Self {
        Self {
            pull_timeout_ms: default_pull_timeout_ms(),
            auth_file: None,
        }
    }
}

impl ReferenceSettings {
    /// Sets the longest time that materializing one reference may take, in milliseconds.
    #[must_use]
    pub fn with_pull_timeout_ms(mut self, pull_timeout_ms: u64) -> Self {
        self.pull_timeout_ms = pull_timeout_ms;
        self
    }

    /// Selects the host-side registry credentials file.
    #[must_use]
    pub fn with_auth_file(mut self, auth_file: impl Into<PathBuf>) -> Self {
        self.auth_file = Some(auth_file.into());
        self
    }

    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    fn validate(&self) -> Result<()> {
        if self.pull_timeout_ms == 0 || self.pull_timeout_ms > Timeouts::MAX_MS {
            return Err(Error::malformed_request(format!(
                "setup images.references.pullTimeoutMs must be between 1 and {}",
                Timeouts::MAX_MS
            )));
        }
        if self
            .auth_file
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(Error::malformed_request(
                "setup images.references.authFile is empty",
            ));
        }
        Ok(())
    }
}

/// Flow control of streamed executions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecSettings {
    /// Output frames that the guest may send ahead of the consumer.
    #[serde(default = "default_output_window_frames")]
    pub output_window_frames: u32,
    /// Input frames that the host may send ahead of the guest.
    #[serde(default = "default_input_window_frames")]
    pub input_window_frames: u32,
}

/// Most frames that one execution may have in flight in both directions. The transport buffers at
/// most 13 frames per stream, and four more are reserved for the start, input-closed, terminal,
/// and close frames.
pub const MAX_EXEC_WINDOW_FRAMES: u32 = 9;

fn default_output_window_frames() -> u32 {
    4
}

fn default_input_window_frames() -> u32 {
    2
}

impl Default for ExecSettings {
    fn default() -> Self {
        Self {
            output_window_frames: default_output_window_frames(),
            input_window_frames: default_input_window_frames(),
        }
    }
}

impl ExecSettings {
    fn validate(&self) -> Result<()> {
        if self.output_window_frames == 0 || self.input_window_frames == 0 {
            return Err(Error::malformed_request(
                "setup exec windows must be at least one frame",
            ));
        }
        if self
            .output_window_frames
            .saturating_add(self.input_window_frames)
            > MAX_EXEC_WINDOW_FRAMES
        {
            return Err(Error::malformed_request(format!(
                "setup exec windows may total at most {MAX_EXEC_WINDOW_FRAMES} frames"
            )));
        }
        Ok(())
    }
}

/// Diagnostic switches.
///
/// None of them makes an operation hash an image or a runtime file: `verify_image` of the agent
/// backend hashes an image again, as a diagnostic of its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Diagnostics {
    /// Boots guests with verbose kernel and agent logging.
    #[serde(default)]
    pub guest_debug: bool,
}

/// Platform snapshots: templates that the host captures when it first opens the setup, so that
/// sandboxes start as clones of them instead of cold booting.
///
/// The host captures two templates, microVM snapshots that hold no tenant data: one without a
/// NIC, and one with a deny-all NIC on the default guest network,
/// [`SandboxDefaults::guest_network`]. They boot with the default memory of
/// [`SandboxDefaults::resources`], so while platform snapshots are enabled, that memory must be a
/// whole number of [`MEMORY_BLOCK_MIB`](Self::MEMORY_BLOCK_MIB) blocks, and no more than
/// [`max_memory_mib`](Self::max_memory_mib). Every start that a template fits starts as a clone
/// of it, and every other start cold boots and says why; see [`crate::snapshot`].
///
/// It serializes as `{"enabled": true, "maxVcpus": 4, "maxMemoryMib": 8192}`, with an optional
/// `maxImageMib`. [`SetupConfig::validate`] checks each field that is present, whether or not
/// `enabled` is true.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct PlatformSnapshots {
    /// Whether the host keeps templates. When it is false, as when this object is absent, every
    /// sandbox cold boots.
    pub enabled: bool,
    /// The processor capacity of the templates: 1, 2, 4, or 8. A clone may have any of 1, 2, 4,
    /// or 8 virtual processors up to it. Required when [`enabled`](Self::enabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_vcpus: Option<u32>,
    /// The memory capacity of the templates, in MiB: a positive multiple of 128. A clone's memory
    /// is whole 128 MiB blocks, from the default memory up to it. Required when
    /// [`enabled`](Self::enabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_mib: Option<u32>,
    /// The longest image that a clone can use, in MiB; longer images cold boot. `None` selects
    /// [`DEFAULT_MAX_IMAGE_MIB`](Self::DEFAULT_MAX_IMAGE_MIB), as
    /// [`max_image_mib()`](Self::max_image_mib()) returns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_image_mib: Option<u32>,
}

impl PlatformSnapshots {
    /// The numbers of virtual processors that a template's capacity, and a clone, may have.
    pub const VCPU_COUNTS: [u32; 4] = [1, 2, 4, 8];
    /// The block, in MiB, of which a template's memory and a clone's are whole numbers.
    pub const MEMORY_BLOCK_MIB: u32 = 128;
    /// The longest image that a clone can use by default, in MiB: 64 GiB.
    pub const DEFAULT_MAX_IMAGE_MIB: u32 = 64 * 1024;

    /// Enables platform snapshots with these capacities and the default image capacity.
    pub fn new(max_vcpus: u32, max_memory_mib: u32) -> Self {
        Self {
            enabled: true,
            max_vcpus: Some(max_vcpus),
            max_memory_mib: Some(max_memory_mib),
            max_image_mib: None,
        }
    }

    /// Disables platform snapshots, as an absent object does.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            max_vcpus: None,
            max_memory_mib: None,
            max_image_mib: None,
        }
    }

    /// Sets the longest image that a clone can use, in MiB.
    #[must_use]
    pub fn with_max_image_mib(mut self, max_image_mib: u32) -> Self {
        self.max_image_mib = Some(max_image_mib);
        self
    }

    /// Returns the longest image that a clone can use, in MiB: the field's value, or
    /// [`DEFAULT_MAX_IMAGE_MIB`](Self::DEFAULT_MAX_IMAGE_MIB) when it is unset.
    pub fn max_image_mib(&self) -> u32 {
        self.max_image_mib.unwrap_or(Self::DEFAULT_MAX_IMAGE_MIB)
    }

    fn validate(&self, defaults: &SandboxDefaults) -> Result<()> {
        if let Some(vcpus) = self.max_vcpus
            && !Self::VCPU_COUNTS.contains(&vcpus)
        {
            return Err(Error::malformed_request(format!(
                "setup platformSnapshots.maxVcpus must be 1, 2, 4, or 8, not {vcpus}"
            )));
        }
        if let Some(memory_mib) = self.max_memory_mib
            && (memory_mib == 0 || !memory_mib.is_multiple_of(Self::MEMORY_BLOCK_MIB))
        {
            return Err(Error::malformed_request(format!(
                "setup platformSnapshots.maxMemoryMib must be a positive multiple of {}, not \
                 {memory_mib}",
                Self::MEMORY_BLOCK_MIB
            )));
        }
        if self.max_image_mib == Some(0) {
            return Err(Error::malformed_request(
                "setup platformSnapshots.maxImageMib must be positive",
            ));
        }
        if !self.enabled {
            return Ok(());
        }
        let (Some(_), Some(max_memory_mib)) = (self.max_vcpus, self.max_memory_mib) else {
            return Err(Error::malformed_request(
                "setup platformSnapshots must set maxVcpus and maxMemoryMib when enabled",
            ));
        };
        // The templates boot with the default memory.
        let memory_mib = defaults.resources.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB);
        if !memory_mib.is_multiple_of(Self::MEMORY_BLOCK_MIB) {
            return Err(Error::malformed_request(format!(
                "setup defaults.resources.memoryMib must be a multiple of {} when platform \
                 snapshots are enabled, not {memory_mib}",
                Self::MEMORY_BLOCK_MIB
            )));
        }
        if memory_mib > max_memory_mib {
            return Err(Error::malformed_request(format!(
                "setup defaults.resources.memoryMib, {memory_mib}, must not exceed \
                 platformSnapshots.maxMemoryMib, {max_memory_mib}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;

    fn files() -> RuntimeSource {
        RuntimeSource::Files(RuntimeFiles {
            openvmm: "/opt/nvx/bin/openvmm".into(),
            kernel: "/opt/nvx/guest/vmlinux".into(),
            initrd: "/opt/nvx/guest/initramfs-edge.cpio.gz".into(),
            approved_sha256: None,
        })
    }

    #[test]
    fn minimal_configuration_takes_every_default() {
        let config: SetupConfig = serde_json::from_str(
            r#"{"stateRoot":"/var/lib/nvx","runtime":{"bundle":{"path":"/opt/nvx","manifestSha256":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}}}"#,
        )
        .unwrap();
        assert_eq!(config.schema_version, SetupConfig::SCHEMA_VERSION);
        assert_eq!(config.cpu_profile, CpuProfile::Auto);
        assert_eq!(config.timeouts, Timeouts::default());
        assert!(config.guest_session.hold);
        assert_eq!(config.defaults, SandboxDefaults::default());
        assert_eq!(config.exec, ExecSettings::default());
        assert_eq!(config.images, ImageSettings::default());
        assert_eq!(config.images.max_cache_capacity_bytes, None);
        assert_eq!(
            config.images.references.pull_timeout_ms,
            DEFAULT_PULL_TIMEOUT_MS
        );
        assert!(config.images.references.auth_file.is_none());
        config.validate().unwrap();
    }

    #[test]
    fn image_cache_capacity_defaults_for_omitted_settings_and_null() {
        for json in [
            "{}",
            r#"{"references":{}}"#,
            r#"{"maxCacheCapacityBytes":null}"#,
        ] {
            let images: ImageSettings = serde_json::from_str(json).unwrap();
            assert_eq!(images.max_cache_capacity_bytes, None);
            let config = SetupConfig::new("/var/lib/nvx", files()).with_images(images);
            config.validate().unwrap();
            let encoded = serde_json::to_string(&config).unwrap();
            assert!(!encoded.contains("maxCacheCapacityBytes"), "{encoded}");
            assert_eq!(
                serde_json::from_str::<SetupConfig>(&encoded).unwrap(),
                config
            );
        }
    }

    #[test]
    fn image_cache_capacity_builder_and_json_round_trip() {
        for capacity in [1, 10 * 1024 * 1024 * 1024, u64::MAX] {
            let config = SetupConfig::new("/var/lib/nvx", files()).with_images(
                ImageSettings::default()
                    .with_max_cache_capacity_bytes(capacity)
                    .with_references(ReferenceSettings::default().with_pull_timeout_ms(60_000)),
            );
            config.validate().unwrap();
            assert_eq!(config.images.max_cache_capacity_bytes, Some(capacity));
            let json = serde_json::to_value(&config).unwrap();
            assert_eq!(json["images"]["maxCacheCapacityBytes"], capacity);
            assert_eq!(json["images"]["references"]["pullTimeoutMs"], 60_000);
            assert_eq!(serde_json::from_value::<SetupConfig>(json).unwrap(), config);
        }
    }

    #[test]
    fn image_cache_capacity_rejects_zero_and_invalid_json_types() {
        let config = SetupConfig::new("/var/lib/nvx", files())
            .with_images(ImageSettings::default().with_max_cache_capacity_bytes(0));
        let error = config.validate().unwrap_err();
        assert_eq!(error.code(), ErrorCode::MalformedRequest);
        assert!(error.message().contains("images.maxCacheCapacityBytes"));

        for value in ["-1", "1.5", r#""1024""#, "true", "18446744073709551616"] {
            let json = format!(r#"{{"maxCacheCapacityBytes":{value}}}"#);
            assert!(
                serde_json::from_str::<ImageSettings>(&json).is_err(),
                "{json}"
            );
        }
    }

    #[test]
    fn reference_settings_are_omitted_until_set_and_round_trip() {
        let config = SetupConfig::new("/var/lib/nvx", files());
        let json = serde_json::to_string(&config).unwrap();
        assert!(!json.contains("references"), "{json}");
        assert!(!json.contains("authFile"), "{json}");

        let config = config.with_images(
            ImageSettings::default()
                .with_references(ReferenceSettings::default().with_pull_timeout_ms(60_000)),
        );
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains(r#""references":{"pullTimeoutMs":60000}"#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<SetupConfig>(&json).unwrap(), config);
        config.validate().unwrap();
    }

    #[test]
    fn registry_auth_file_round_trips_without_inline_credentials() {
        let config = SetupConfig::new("/var/lib/nvx", files())
            .with_images(ImageSettings::default().with_references(
                ReferenceSettings::default().with_auth_file("registry-auth.json"),
            ));
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains(r#""authFile":"registry-auth.json""#),
            "{json}"
        );
        assert!(!json.contains("password"), "{json}");
        assert_eq!(serde_json::from_str::<SetupConfig>(&json).unwrap(), config);
        config.validate().unwrap();

        let old: ReferenceSettings = serde_json::from_str(r#"{"pullTimeoutMs":60000}"#).unwrap();
        assert_eq!(old.pull_timeout_ms, 60_000);
        assert!(old.auth_file.is_none());
    }

    #[test]
    fn configurations_round_trip_and_reject_unknown_fields() {
        let config = SetupConfig::new("/var/lib/nvx", files())
            .with_hypervisor(Hypervisor::Mshv)
            .with_cpu_profile(CpuProfile::Host);
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains(r#""hypervisor":"mshv""#), "{json}");
        assert!(json.contains(r#""cpuProfile":"host""#), "{json}");
        assert_eq!(serde_json::from_str::<SetupConfig>(&json).unwrap(), config);
        let unknown = json.replace(r#""cpuProfile""#, r#""cpuProfiles""#);
        assert!(serde_json::from_str::<SetupConfig>(&unknown).is_err());
    }

    #[test]
    fn validation_rejects_malformed_configurations() {
        let mut bad = Vec::new();
        let mut config = SetupConfig::new("/var/lib/nvx", files());
        config.schema_version = 2;
        bad.push(config);
        bad.push(SetupConfig::new("", files()));
        bad.push(SetupConfig::new(
            "/var/lib/nvx",
            RuntimeSource::Bundle(BundleSource {
                path: "".into(),
                manifest_sha256: [1; 32],
            }),
        ));
        let mut config = SetupConfig::new("/var/lib/nvx", files());
        config.timeouts.control_ms = 0;
        bad.push(config);
        let mut config = SetupConfig::new("/var/lib/nvx", files());
        config.exec.output_window_frames = 8;
        bad.push(config);
        // Windows whose sum overflows a u32 are too large, not small.
        let mut config = SetupConfig::new("/var/lib/nvx", files());
        config.exec.output_window_frames = u32::MAX;
        config.exec.input_window_frames = 2;
        bad.push(config);
        let mut config = SetupConfig::new("/var/lib/nvx", files());
        config.defaults.guest_network = "10.0.0.2".into();
        bad.push(config);
        let mut config = SetupConfig::new("/var/lib/nvx", files());
        config.defaults.resources.memory_mib = None;
        bad.push(config);
        for references in [
            ReferenceSettings::default().with_pull_timeout_ms(0),
            ReferenceSettings::default().with_pull_timeout_ms(Timeouts::MAX_MS + 1),
            ReferenceSettings::default().with_auth_file(""),
        ] {
            bad.push(
                SetupConfig::new("/var/lib/nvx", files())
                    .with_images(ImageSettings::default().with_references(references)),
            );
        }
        for config in bad {
            let error = config.validate().unwrap_err();
            assert_eq!(error.code(), ErrorCode::MalformedRequest, "{config:?}");
        }
    }

    #[test]
    fn digests_are_hexadecimal_on_the_wire() {
        let json = r#"{"stateRoot":"/s","runtime":{"bundle":{"path":"/b","manifestSha256":"0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef"}}}"#;
        assert!(serde_json::from_str::<SetupConfig>(json).is_err());
        let digests = RuntimeDigests {
            openvmm: [1; 32],
            kernel: [2; 32],
            initrd: [3; 32],
        };
        let text = serde_json::to_string(&digests).unwrap();
        assert!(text.contains(&"02".repeat(32)), "{text}");
        assert_eq!(
            serde_json::from_str::<RuntimeDigests>(&text).unwrap(),
            digests
        );
    }

    #[test]
    fn hypervisors_keep_their_json_and_error_codes() {
        for hypervisor in [Hypervisor::Kvm, Hypervisor::Mshv, Hypervisor::Whp] {
            assert_eq!(
                serde_json::to_string(&hypervisor).unwrap(),
                format!("\"{}\"", hypervisor.as_str())
            );
            let config = SetupConfig::new("/var/lib/nvx", files()).with_hypervisor(hypervisor);
            let json = serde_json::to_value(&config).unwrap();
            assert_eq!(json["hypervisor"], hypervisor.as_str());
        }
        let error = Error::from("hyperv".parse::<Hypervisor>().unwrap_err());
        assert_eq!(error.code(), ErrorCode::BackendUnavailable);
        assert_eq!(
            error.message(),
            "unknown hypervisor \"hyperv\"; choose kvm, mshv, or whp"
        );
    }

    #[test]
    fn platform_snapshots_are_omitted_until_set_and_round_trip() {
        let config = SetupConfig::new("/var/lib/nvx", files());
        let json = serde_json::to_string(&config).unwrap();
        assert!(!json.contains("platformSnapshots"), "{json}");
        for (snapshots, expected) in [
            (
                PlatformSnapshots::new(4, 8192),
                serde_json::json!({"enabled": true, "maxVcpus": 4, "maxMemoryMib": 8192}),
            ),
            (
                PlatformSnapshots::new(8, 16384).with_max_image_mib(131_072),
                serde_json::json!({
                    "enabled": true, "maxVcpus": 8, "maxMemoryMib": 16384, "maxImageMib": 131072
                }),
            ),
            (
                PlatformSnapshots::disabled(),
                serde_json::json!({"enabled": false}),
            ),
        ] {
            let config =
                SetupConfig::new("/var/lib/nvx", files()).with_platform_snapshots(snapshots);
            config.validate().unwrap();
            let json = serde_json::to_value(&config).unwrap();
            assert_eq!(json["platformSnapshots"], expected);
            assert_eq!(serde_json::from_value::<SetupConfig>(json).unwrap(), config);
        }
    }

    #[test]
    fn platform_snapshots_refuse_unknown_and_missing_fields() {
        for json in [
            r#"{"enabled": true, "maxVcpus": 4, "maxMemoryMib": 8192, "maxImageSize": 1}"#,
            r#"{"enabled": true, "maxVcpus": 4, "maxMemoryMib": 8192, "minVcpus": 1}"#,
            r#"{"maxVcpus": 4, "maxMemoryMib": 8192}"#,
            r#"{"enabled": "true"}"#,
            r#"{"enabled": true, "maxVcpus": -1, "maxMemoryMib": 8192}"#,
            r#"{"enabled": true, "maxVcpus": 4, "maxMemoryMib": 8192, "maxImageMib": 4294967296}"#,
        ] {
            assert!(
                serde_json::from_str::<PlatformSnapshots>(json).is_err(),
                "{json}"
            );
        }
        let config = SetupConfig::new("/var/lib/nvx", files())
            .with_platform_snapshots(PlatformSnapshots::new(4, 8192));
        let mut json = serde_json::to_value(&config).unwrap();
        json["platformSnapshots"]["maxVcpu"] = 4.into();
        assert!(serde_json::from_value::<SetupConfig>(json).is_err());
    }

    #[test]
    fn platform_snapshots_default_to_a_64_gib_image_capacity() {
        let snapshots: PlatformSnapshots =
            serde_json::from_str(r#"{"enabled": true, "maxVcpus": 4, "maxMemoryMib": 8192}"#)
                .unwrap();
        assert_eq!(snapshots.max_image_mib, None);
        assert_eq!(snapshots.max_image_mib(), 65_536);
        assert_eq!(snapshots.with_max_image_mib(1024).max_image_mib(), 1024);
    }

    #[test]
    fn platform_snapshots_validate_each_field_present() {
        let config = |snapshots: PlatformSnapshots| {
            SetupConfig::new("/var/lib/nvx", files()).with_platform_snapshots(snapshots)
        };
        let with_memory = |memory_mib: u32, snapshots: PlatformSnapshots| {
            let mut config = config(snapshots);
            config.defaults.resources.memory_mib = Some(memory_mib);
            config
        };
        let disabled = PlatformSnapshots::disabled();
        let enabled = PlatformSnapshots::new(4, 8192);
        for vcpus in [1, 2, 4, 8] {
            config(PlatformSnapshots::new(vcpus, 8192))
                .validate()
                .unwrap();
        }
        for good in [
            // The capacity may equal the default memory, 256 MiB.
            config(PlatformSnapshots::new(1, 256).with_max_image_mib(1)),
            with_memory(1024, PlatformSnapshots::new(2, 1024)),
            with_memory(128, PlatformSnapshots::new(8, 4_294_967_168)),
            config(disabled),
            // Only snapshots that are enabled constrain the default memory.
            with_memory(
                300,
                PlatformSnapshots {
                    max_memory_mib: Some(256),
                    ..disabled
                },
            ),
        ] {
            good.validate().unwrap();
        }
        for (bad, field) in [
            (
                config(PlatformSnapshots::new(0, 8192)),
                "platformSnapshots.maxVcpus",
            ),
            (
                config(PlatformSnapshots::new(3, 8192)),
                "platformSnapshots.maxVcpus",
            ),
            (
                config(PlatformSnapshots::new(16, 8192)),
                "platformSnapshots.maxVcpus",
            ),
            (
                config(PlatformSnapshots::new(4, 0)),
                "platformSnapshots.maxMemoryMib",
            ),
            (
                config(PlatformSnapshots::new(4, 8000)),
                "platformSnapshots.maxMemoryMib",
            ),
            (
                config(enabled.with_max_image_mib(0)),
                "platformSnapshots.maxImageMib",
            ),
            // Snapshots that are disabled are checked as well.
            (
                config(PlatformSnapshots {
                    max_vcpus: Some(3),
                    ..disabled
                }),
                "platformSnapshots.maxVcpus",
            ),
            (
                config(PlatformSnapshots {
                    max_memory_mib: Some(100),
                    ..disabled
                }),
                "platformSnapshots.maxMemoryMib",
            ),
            (
                config(disabled.with_max_image_mib(0)),
                "platformSnapshots.maxImageMib",
            ),
            // Snapshots that are enabled need both capacities.
            (
                config(PlatformSnapshots {
                    max_vcpus: None,
                    ..enabled
                }),
                "platformSnapshots must set maxVcpus and maxMemoryMib",
            ),
            (
                config(PlatformSnapshots {
                    max_memory_mib: None,
                    ..enabled
                }),
                "platformSnapshots must set maxVcpus and maxMemoryMib",
            ),
            // The templates boot with the default memory.
            (
                with_memory(300, enabled),
                "defaults.resources.memoryMib must be a multiple of 128",
            ),
            (
                with_memory(1024, PlatformSnapshots::new(4, 512)),
                "defaults.resources.memoryMib, 1024, must not exceed",
            ),
        ] {
            let error = bad.validate().unwrap_err();
            assert_eq!(error.code(), ErrorCode::MalformedRequest, "{bad:?}");
            assert!(error.message().contains(field), "{field}: {error}");
        }
    }
}
