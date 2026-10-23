// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Fake OpenVMM for the tests of this crate; see `nvx::testing::fake_openvmm`.
//!
//! The binary stays in this package because `CARGO_BIN_EXE_*` names only binaries of the package
//! whose tests run.

fn main() -> std::process::ExitCode {
    aci_edge_sandboxes::nvx::testing::fake_openvmm::main()
}
