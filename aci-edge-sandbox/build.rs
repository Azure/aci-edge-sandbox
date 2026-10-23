// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Passes the bundle that `nvx` staged on to the build scripts of this crate's dependents, and
//! stages the edge agent bundle when the `agent-bundled` feature is enabled.
//!
//! With the `bundled` feature, the build script of `nvx` stages the OpenVMM executable, guest
//! kernel, and Alpine initramfs, and exports the staged directory as `DEP_NVX_ARTIFACTS_DIR`.
//! Cargo gives `DEP_*` variables only to direct dependents, so this script exports the directory
//! again, as `DEP_ACI_EDGE_SANDBOXES_ARTIFACTS_DIR`, for the build scripts of crates that depend on
//! this one and ship the bundle next to their executables.
//!
//! With the `agent-bundled` feature, it stages the edge agent bundle: the agent library, OpenVMM,
//! the edge guest, and the image tool. See `build/agent_bundle.rs`.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=DEP_NVX_ARTIFACTS_DIR");
    if let Some(directory) = std::env::var_os("DEP_NVX_ARTIFACTS_DIR") {
        println!(
            "cargo::metadata=artifacts_dir={}",
            std::path::Path::new(&directory).display()
        );
    }
    #[cfg(feature = "agent-bundled")]
    agent_bundle::run();
}

// The names and pins that this script shares with the crate.
#[cfg(feature = "agent-bundled")]
#[allow(dead_code)]
#[path = "src/agent/bundle_contract.rs"]
mod bundle_contract;

#[cfg(feature = "agent-bundled")]
#[path = "build/agent_bundle.rs"]
mod agent_bundle;
