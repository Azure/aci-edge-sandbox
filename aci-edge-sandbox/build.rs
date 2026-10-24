// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Stages the edge agent bundle when the `agent-bundled` feature is enabled: the agent library,
//! OpenVMM, the edge guest, and the image tool. See `build/agent_bundle.rs`.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
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
