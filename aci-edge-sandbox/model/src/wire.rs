// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! JSON shapes of the sandbox ABI between a client and a native host library.
//!
//! Both sides compile these definitions, so they agree on every field. Requests and results that
//! the lifecycle already defines, such as [`ExecRequest`] and [`StartResult`](crate::StartResult),
//! cross the boundary unchanged; this module adds the envelopes around them.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorBody, Result};
use crate::image::{ImageDigest, ImageId};
use crate::model::{ExecRequest, Metadata, ProvisionRequest};
use crate::outcome::ExecOutcome;
use crate::setup::RuntimeDigests;
use crate::snapshot::PlatformSnapshotsInfo;
use crate::spec::{SandboxSpec, validate_path, validate_reference};

/// Version of the sandbox ABI. A client requires a library that reports exactly this version.
///
/// Version 3 added [`SetupConfig::platform_snapshots`](crate::SetupConfig::platform_snapshots) and
/// [`HostInfo::platform_snapshots`].
pub const SANDBOX_ABI_VERSION: u32 = 3;

/// Input of the provision call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProvisionInput {
    /// The provision request.
    pub request: ProvisionRequest,
    /// Creation settings outside the request.
    #[serde(default, skip_serializing_if = "SandboxSpec::is_empty")]
    pub spec: SandboxSpec,
}

/// Result of the provision call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProvisionReply {
    /// The sandbox's token: 32 lowercase hexadecimal digits. The client turns it into its own
    /// sandbox ID format.
    pub token: String,
    /// Backend-defined metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

/// Returns whether `value` is a sandbox token: 32 lowercase hexadecimal digits.
pub fn is_token(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Input of the exec call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecInput {
    /// The exec request.
    pub request: ExecRequest,
    /// Whether the caller will write the workload's standard input.
    #[serde(default)]
    pub stdin: bool,
    /// Runs the workload on a terminal of this size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pty: Option<PtySize>,
}

/// Size of a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PtySize {
    /// Rows.
    pub rows: u16,
    /// Columns.
    pub cols: u16,
}

/// How an execution ended: its outcome, or the error that prevented one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum ExecEnd {
    /// The workload's outcome.
    Outcome(ExecOutcome),
    /// The execution failed without an outcome.
    Error(ErrorBody),
}

/// Input of the image registration call: the image, a local disk image or a registry image, and
/// how the host establishes its digest.
///
/// It serializes as `{"path": "…"}` for a local image or `{"reference": "…"}` for a registry
/// image, which the host pulls first, with a `"digest"` beside either unless it is
/// [`ImageDigest::Compute`]. A registration names exactly one image, as
/// [`validate`](Self::validate) checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageRegistration {
    /// Absolute path of a local GPT disk image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// A container image reference, such as `registry/repository:tag`, which a host that supports
    /// references pulls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// How the host establishes the image's digest.
    #[serde(default, skip_serializing_if = "ImageDigest::is_compute")]
    pub digest: ImageDigest,
}

impl ImageRegistration {
    /// Returns the registration of the local image at `path`.
    pub fn path(path: impl Into<PathBuf>, digest: ImageDigest) -> Self {
        Self {
            path: Some(path.into()),
            reference: None,
            digest,
        }
    }

    /// Returns the registration of the registry image `reference`.
    pub fn reference(reference: impl Into<String>, digest: ImageDigest) -> Self {
        Self {
            path: None,
            reference: Some(reference.into()),
            digest,
        }
    }

    /// Checks the registration's structure, reporting
    /// [`ErrorCode::MalformedRequest`](crate::ErrorCode::MalformedRequest): it names exactly one
    /// image, a `path` that is absolute and valid UTF-8 without NUL characters, or a nonempty
    /// `reference` without whitespace or control characters.
    pub fn validate(&self) -> Result<()> {
        match (&self.path, &self.reference) {
            (Some(path), None) => validate_path(path, "path"),
            (None, Some(reference)) => validate_reference(reference, "reference"),
            _ => Err(Error::malformed_request(
                "an image registration names exactly one of path and reference",
            )),
        }
    }
}

/// Facts about an open host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct HostInfo {
    /// The registered default image, when the setup names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_image: Option<ImageId>,
    /// Digests of the runtime files that the host verified.
    pub runtime_digests: RuntimeDigests,
    /// The host's templates, when its setup enables
    /// [platform snapshots](crate::SetupConfig::platform_snapshots).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_snapshots: Option<PlatformSnapshotsInfo>,
}

impl HostInfo {
    /// Describes a host.
    pub fn new(default_image: Option<ImageId>, runtime_digests: RuntimeDigests) -> Self {
        Self {
            default_image,
            runtime_digests,
            platform_snapshots: None,
        }
    }

    /// Sets the host's templates, for a setup that enables platform snapshots.
    #[must_use]
    pub fn with_platform_snapshots(mut self, platform_snapshots: PlatformSnapshotsInfo) -> Self {
        self.platform_snapshots = Some(platform_snapshots);
        self
    }
}

/// Where a sandbox's host-side diagnostics are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct SandboxDiagnostics {
    /// The VMM's own log.
    pub vmm_log: PathBuf,
    /// The guest's boot console.
    pub console_log: PathBuf,
    /// The VMM's report of how its last run ended.
    pub outcome_report: PathBuf,
}

impl SandboxDiagnostics {
    /// Describes a sandbox's diagnostics.
    pub fn new(vmm_log: PathBuf, console_log: PathBuf, outcome_report: PathBuf) -> Self {
        Self {
            vmm_log,
            console_log,
            outcome_report,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;
    use crate::spec::ImageSource;

    #[test]
    fn provision_inputs_omit_an_empty_spec() {
        let input = ProvisionInput::default();
        assert_eq!(serde_json::to_string(&input).unwrap(), r#"{"request":{}}"#);
        let input = ProvisionInput {
            request: ProvisionRequest::new(),
            spec: SandboxSpec::new().with_image(ImageSource::Path("/i.vhd".into())),
        };
        let json = serde_json::to_string(&input).unwrap();
        assert_eq!(
            serde_json::from_str::<ProvisionInput>(&json).unwrap(),
            input
        );
    }

    #[test]
    fn image_registrations_name_a_local_image_or_a_registry_image() {
        // A local image keeps the form that path registrations always had.
        let path = std::env::temp_dir().join("image.vhd");
        let local = ImageRegistration::path(&path, ImageDigest::Compute);
        assert_eq!(
            serde_json::to_value(&local).unwrap(),
            serde_json::json!({"path": path.to_str().unwrap()})
        );
        let pulled =
            ImageRegistration::reference("registry.example/a:1", ImageDigest::Expect([1; 32]));
        assert_eq!(
            serde_json::to_value(&pulled).unwrap(),
            serde_json::json!({"reference": "registry.example/a:1", "digest": {"expect": "01".repeat(32)}})
        );
        for registration in [&local, &pulled] {
            registration.validate().unwrap();
            let json = serde_json::to_string(registration).unwrap();
            assert_eq!(
                &serde_json::from_str::<ImageRegistration>(&json).unwrap(),
                registration
            );
        }
        // Exactly one image, of a valid form.
        let malformed = |registration: ImageRegistration, message: &str| {
            let error = registration.validate().unwrap_err();
            assert_eq!(error.code(), ErrorCode::MalformedRequest, "{message}");
            assert_eq!(error.message(), message);
        };
        let one = "an image registration names exactly one of path and reference";
        malformed(
            ImageRegistration {
                reference: Some("registry.example/a:1".to_owned()),
                ..local.clone()
            },
            one,
        );
        malformed(
            serde_json::from_str::<ImageRegistration>(r#"{"digest":"compute"}"#).unwrap(),
            one,
        );
        malformed(
            ImageRegistration::path("image.vhd", ImageDigest::Compute),
            "path must be an absolute valid UTF-8 path without NUL characters",
        );
        malformed(
            ImageRegistration::reference("registry.example/a :1", ImageDigest::Compute),
            "reference must be a nonempty image reference without whitespace or control \
             characters",
        );
        assert!(serde_json::from_str::<ImageRegistration>(r#"{"image":"a"}"#).is_err());
    }

    #[test]
    fn exec_ends_carry_an_outcome_or_an_error() {
        let ended = ExecEnd::Outcome(ExecOutcome::Exited(3));
        assert_eq!(
            serde_json::to_string(&ended).unwrap(),
            r#"{"outcome":{"exited":3}}"#
        );
        let failed = ExecEnd::Error(crate::Error::stale_id("gone").body());
        let json = serde_json::to_string(&failed).unwrap();
        assert_eq!(json, r#"{"error":{"code":"stale_id","message":"gone"}}"#);
        assert_eq!(serde_json::from_str::<ExecEnd>(&json).unwrap(), failed);
        assert_eq!(ErrorCode::StaleId.as_str(), "stale_id");
    }

    #[test]
    fn tokens_are_32_lowercase_hexadecimal_digits() {
        assert!(is_token("0123456789abcdef0123456789abcdef"));
        assert!(!is_token("0123456789ABCDEF0123456789abcdef"));
        assert!(!is_token("0123456789abcdef"));
        assert!(!is_token("../0123456789abcdef0123456789abc"));
    }

    fn host_with_templates() -> HostInfo {
        use crate::snapshot::{TemplateClass, TemplateInfo, TemplateKey, TemplateState};
        use std::time::{Duration, UNIX_EPOCH};

        HostInfo::new(None, DIGESTS).with_platform_snapshots(PlatformSnapshotsInfo::new(vec![
            TemplateInfo::new(TemplateClass::NoNic, TemplateState::Ready).with_generation(
                TemplateKey::from_bytes([0xcd; 32]),
                3,
                UNIX_EPOCH + Duration::from_secs(1_793_257_200),
            ),
            TemplateInfo::new(TemplateClass::Nic, TemplateState::Unavailable)
                .with_last_error("the capture timed out"),
        ]))
    }

    const DIGESTS: RuntimeDigests = RuntimeDigests {
        openvmm: [1; 32],
        kernel: [2; 32],
        initrd: [3; 32],
    };

    #[test]
    fn host_info_lists_templates_only_when_the_setup_enables_platform_snapshots() {
        use crate::snapshot::DisabledReason;

        let host = HostInfo::new(None, DIGESTS);
        let json = serde_json::to_value(&host).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"runtimeDigests": {
                "openvmm": "01".repeat(32), "kernel": "02".repeat(32), "initrd": "03".repeat(32)
            }})
        );
        assert_eq!(serde_json::from_value::<HostInfo>(json).unwrap(), host);

        let host = host_with_templates();
        let json = serde_json::to_value(&host).unwrap();
        assert_eq!(
            json["platformSnapshots"],
            serde_json::json!({"templates": [
                {
                    "class": "noNic",
                    "state": "ready",
                    "key": "cd".repeat(32),
                    "generation": 3,
                    "capturedAt": "2026-10-29T07:00:00Z"
                },
                {"class": "nic", "state": "unavailable", "lastError": "the capture timed out"}
            ]})
        );
        assert_eq!(serde_json::from_value::<HostInfo>(json).unwrap(), host);

        let host = HostInfo::new(None, DIGESTS)
            .with_platform_snapshots(PlatformSnapshotsInfo::disabled(DisabledReason::GuestDebug));
        let json = serde_json::to_value(&host).unwrap();
        assert_eq!(
            json["platformSnapshots"],
            serde_json::json!({"disabledReason": "guest-debug", "templates": []})
        );
        assert_eq!(serde_json::from_value::<HostInfo>(json).unwrap(), host);
    }

    #[test]
    fn host_info_refuses_unknown_fields_and_malformed_templates() {
        let json = serde_json::to_value(host_with_templates()).unwrap();
        serde_json::from_value::<HostInfo>(json.clone()).unwrap();
        let refused = |pointer: &str, field: &str, value: serde_json::Value| {
            let mut json = json.clone();
            json.pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert(field.to_owned(), value.clone());
            assert!(
                serde_json::from_value::<HostInfo>(json).is_err(),
                "{pointer}/{field}: {value}"
            );
        };
        refused("", "templates", serde_json::json!([]));
        refused("/platformSnapshots", "enabled", true.into());
        refused(
            "/platformSnapshots",
            "disabledReason",
            "image-too-long".into(),
        );
        refused(
            "/platformSnapshots",
            "disabledReason",
            serde_json::json!({"guest-debug": null}),
        );
        let template = "/platformSnapshots/templates/0";
        refused(template, "eligible", true.into());
        refused(template, "class", "no-nic".into());
        refused(template, "class", serde_json::json!({"noNic": null}));
        refused(template, "state", "capturing".into());
        refused(template, "state", serde_json::json!({"ready": null}));
        refused(template, "key", "CD".repeat(32).into());
        refused(template, "generation", (-1).into());
        refused(template, "capturedAt", "2026-10-29T07:00:00+00:00".into());
        refused(template, "capturedAt", "2026-10-29 07:00:00Z".into());
        refused(template, "capturedAt", 1_793_257_200.into());
        refused(template, "lastError", false.into());
    }
}
