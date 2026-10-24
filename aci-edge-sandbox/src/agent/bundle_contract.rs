// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Names and pins that the build script and [`BundledAgent`](super::BundledAgent) share for the
//! edge agent bundle that the `agent-bundled` feature stages.
//!
//! The build script includes this file, so it must not depend on the rest of the crate. Each of
//! the two uses part of it.

/// Build-time variable that, set to `skip`, stages no bundle.
pub(crate) const BUNDLE_ENV: &str = "ACI_EDGE_SANDBOXES_AGENT_BUNDLE";
/// Build-time variable naming a bundle ZIP to download instead of the release's.
pub(crate) const BUNDLE_URL_ENV: &str = "ACI_EDGE_SANDBOXES_AGENT_BUNDLE_URL";
/// Build-time variable naming an extracted bundle to stage instead of downloading one.
pub(crate) const BUNDLE_DIR_ENV: &str = "ACI_EDGE_SANDBOXES_AGENT_BUNDLE_DIR";
/// Build-time variable holding the approved SHA-256 of the bundle ZIP.
pub(crate) const BUNDLE_SHA256_ENV: &str = "ACI_EDGE_SANDBOXES_AGENT_BUNDLE_SHA256";

/// Run-time variable naming a directory that holds a copy of the staged bundle.
pub(crate) const ARTIFACTS_DIR_ENV: &str = "ACI_EDGE_SANDBOXES_AGENT_ARTIFACTS_DIR";
/// Name of the bundle directory that the build script creates beside the executables that Cargo
/// builds, and that discovery looks for beside an executable.
pub(crate) const BESIDE_EXE_DIR: &str = "aci-edge-sandboxes";
/// The bundle's runtime manifest, which a bundle runtime trusts through its SHA-256.
pub(crate) const SOURCE_MANIFEST: &str = "SOURCE-MANIFEST.json";

/// What the build script verified about the staged bundle, which it compiles into the crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pins {
    /// SHA-256 of the bundle's `SOURCE-MANIFEST.json`.
    pub(crate) manifest_sha256: [u8; 32],
    /// Path of the agent library in the bundle, with `/` separators.
    pub(crate) library: &'static str,
    /// SHA-256 of the agent library.
    pub(crate) library_sha256: [u8; 32],
    /// Version of the sandbox ABI that the bundle declares for its library.
    pub(crate) sandbox_abi: u32,
}

/// Expands to the name of the file in `OUT_DIR` that the build script writes and the crate
/// includes: Rust source that defines `PINS`, an `Option<Pins>` that is `None` when the build
/// staged no bundle.
macro_rules! pins_file {
    () => {
        "agent_bundle_pins.rs"
    };
}
pub(crate) use pins_file;
