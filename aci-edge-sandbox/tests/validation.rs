// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Deterministic backend policies agree with dry-run validation without runtime dependencies.
#![cfg(all(feature = "openvmm", any(target_os = "linux", windows)))]

use std::ffi::OsString;
use std::fs;
use std::io;
use std::time::Duration;

use aci_edge_sandboxes::nvx::Hypervisor;
use aci_edge_sandboxes::openvmm::OpenVmmConfig;
use aci_edge_sandboxes::{
    Access, AciEdgeSandbox, ErrorCode, ExecIo, ExecRequest, FilesystemPolicy, NetworkPolicy,
    NetworkPort, NetworkRule, OutputSink, ProcessSpec, Protocol, ProvisionRequest, SandboxId,
    StdinMode,
};

fn config(directory: &tempfile::TempDir) -> OpenVmmConfig {
    OpenVmmConfig::new(
        directory.path().join("missing-openvmm"),
        directory.path().join("missing-kernel"),
        directory.path().join("missing-initrd"),
        Hypervisor::platform_default().unwrap(),
        directory.path().join("state"),
    )
}

fn client() -> (tempfile::TempDir, AciEdgeSandbox) {
    let directory = tempfile::tempdir().unwrap();
    let client = AciEdgeSandbox::openvmm(config(&directory)).unwrap();
    (directory, client)
}

fn state_entries(directory: &tempfile::TempDir) -> Vec<OsString> {
    let mut entries: Vec<_> = fs::read_dir(directory.path().join("state"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    entries.sort();
    entries
}

fn stale_id() -> SandboxId {
    "aci-edge-sandboxes:00000000000000000000000000000000"
        .parse()
        .unwrap()
}

struct UnusedSink;

impl OutputSink for UnusedSink {
    fn write(&mut self, _chunk: &[u8]) -> io::Result<()> {
        panic!("validation must not produce output")
    }
}

fn unused_io() -> ExecIo {
    ExecIo {
        stdout: Box::new(UnusedSink),
        stderr: Box::new(UnusedSink),
        stdin: None,
    }
}

fn assert_exec_rejected(client: &AciEdgeSandbox, request: &ExecRequest) {
    assert_eq!(
        client.validate_exec(request).unwrap_err().code(),
        ErrorCode::PolicyValidation
    );
    assert_eq!(
        client.exec(&stale_id(), request).unwrap_err().code(),
        ErrorCode::PolicyValidation
    );
    let error = match client.backend().exec(&stale_id(), request, unused_io()) {
        Err(error) => error,
        Ok(_) => panic!("the backend accepted an invalid exec request"),
    };
    assert_eq!(error.code(), ErrorCode::PolicyValidation);
}

/// `count` entries with unique names.
fn entries(count: usize) -> Vec<String> {
    (0..count).map(|index| format!("V{index}=x")).collect()
}

/// `count` entries with unique names of about 4 KiB each.
fn large_entries(count: usize) -> Vec<String> {
    (0..count)
        .map(|index| format!("V{index:02}={}", "x".repeat(4000)))
        .collect()
}

#[test]
fn exec_validation_and_execution_share_guest_policy_checks() {
    let (directory, client) = client();
    let before = state_entries(&directory);
    let full_argument = format!("/{}", "x".repeat(4095));
    for request in [
        ExecRequest::argv(["relative-program"]),
        ExecRequest::argv(["relative-program"]).with_cwd("/tmp"),
        ExecRequest::argv(["relative-program"]).with_env("KEY=value"),
        ExecRequest::command_line("pwd").with_cwd("relative"),
        ExecRequest::command_line("x".repeat(4097)),
        ExecRequest::command_line("x".repeat(4097)).with_cwd("/tmp"),
        ExecRequest::argv(["/bin/echo".to_owned(), "x".repeat(4097)]),
        ExecRequest::argv(vec!["/bin/true"; 65]),
        ExecRequest::argv(vec!["/bin/true"; 65]).with_cwd("/tmp"),
        ExecRequest::argv(["/bin/echo".to_owned(), "x".repeat(4097)]).with_cwd("/tmp"),
        // Linux paths, including their terminating NUL, fit in 4096 bytes.
        ExecRequest::command_line("true").with_cwd(format!("/{}", "x".repeat(4095))),
        // The working directory shares the control protocol's 64 KiB request bound.
        ExecRequest::argv(vec![full_argument.as_str(); 15])
            .with_cwd(format!("/{}", "x".repeat(4094))),
        ExecRequest::command_line("true").with_envs(entries(257)),
        ExecRequest::command_line("true")
            .with_envs(entries(257))
            .with_inherit_default_env(true),
        ExecRequest::command_line("true").with_envs(["KEY=one", "KEY=two"]),
        ExecRequest::command_line("true").with_env(format!("A={}", "x".repeat(4095))),
        // The whole request shares one 64 KiB control record.
        ExecRequest::command_line("true").with_envs(large_entries(17)),
    ] {
        assert_exec_rejected(&client, &request);
    }
    assert_eq!(state_entries(&directory), before);
}

#[test]
fn exec_validation_accepts_the_exact_guest_limits() {
    let (directory, client) = client();
    let before = state_entries(&directory);
    // The working directory and the environment travel in fields of their own, so neither takes
    // anything from the limits of the arguments.
    let full_argument = format!("/{}", "x".repeat(4095));
    for request in [
        ExecRequest::command_line("x".repeat(4096)),
        ExecRequest::argv(["/bin/echo".to_owned(), "x".repeat(4096)]),
        ExecRequest::argv(vec!["/bin/true"; 64]),
        ExecRequest::argv(vec!["/bin/true"; 64]).with_cwd("/tmp"),
        ExecRequest::command_line("x".repeat(4096)).with_cwd("/tmp"),
        // Linux paths, including their terminating NUL, fit in 4096 bytes.
        ExecRequest::command_line("true").with_cwd(format!("/{}", "x".repeat(4094))),
        ExecRequest::argv(vec![full_argument.as_str(); 15]),
        // Timeouts cover the contract's whole range, far beyond an hour.
        ExecRequest::command_line("true").with_timeout(Duration::from_millis(3_600_001)),
        ExecRequest::command_line("true").with_timeout(Duration::from_millis(86_400_000)),
        ExecRequest::command_line("true")
            .with_timeout(Duration::from_millis(ProcessSpec::MAX_TIMEOUT_MS)),
        ExecRequest::command_line("true").with_envs(entries(256)),
        ExecRequest::command_line("true")
            .with_envs(entries(256))
            .with_inherit_default_env(true),
        ExecRequest::argv(vec!["/bin/true"; 64])
            .with_cwd(format!("/{}", "x".repeat(4094)))
            .with_envs(entries(256)),
        ExecRequest::command_line("x".repeat(4096))
            .with_cwd("/tmp")
            .with_env("A=b"),
        ExecRequest::command_line("true").with_env(format!("A={}", "x".repeat(4094))),
        ExecRequest::command_line("true").with_envs(large_entries(15)),
        ExecRequest::command_line("true").with_envs(Vec::<String>::new()),
        ExecRequest::command_line("true")
            .with_envs(Vec::<String>::new())
            .with_inherit_default_env(true),
    ] {
        client.validate_exec(&request).unwrap();
        client.backend().validate_exec(&request).unwrap();
    }
    assert_eq!(state_entries(&directory), before);
}

#[test]
fn exec_validation_rejects_timeouts_beyond_the_mxc_range() {
    let (directory, client) = client();
    let before = state_entries(&directory);
    let maximum = Duration::from_millis(ProcessSpec::MAX_TIMEOUT_MS);
    // Even a fraction of a millisecond above the maximum is refused.
    for timeout in [
        maximum + Duration::from_millis(1),
        maximum + Duration::from_nanos(1),
    ] {
        let request = ExecRequest::command_line("true").with_timeout(timeout);
        // The contract bounds `process.timeout`, so the client refuses a larger one as malformed
        // before any backend sees it.
        for error in [
            client.validate_exec(&request).unwrap_err(),
            client.exec(&stale_id(), &request).unwrap_err(),
        ] {
            assert_eq!(error.code(), ErrorCode::MalformedRequest, "{error}");
            assert!(error.message().contains("4294967295"), "{error}");
        }
        // The backend's own checks refuse what the guest agent's 32-bit timeout cannot carry.
        assert_eq!(
            client.backend().validate_exec(&request).unwrap_err().code(),
            ErrorCode::PolicyValidation,
            "{timeout:?}"
        );
        let error = match client.backend().exec(&stale_id(), &request, unused_io()) {
            Err(error) => error,
            Ok(_) => panic!("the backend accepted a timeout beyond its limit"),
        };
        assert_eq!(error.code(), ErrorCode::PolicyValidation, "{error}");
    }
    assert_eq!(state_entries(&directory), before);
}

fn network_request(rule: NetworkRule) -> ProvisionRequest {
    let mut network = NetworkPolicy::egress(Access::Deny);
    network.egress.allow.push(rule);
    ProvisionRequest::new().with_network(network)
}

fn port_range(protocol: Protocol, end: u16) -> NetworkRule {
    let mut rule = NetworkRule::to("192.0.2.0/24");
    rule.ports.push(NetworkPort {
        protocol,
        port: Some(1),
        end_port: Some(end),
    });
    rule
}

/// A rule with `count` ranges that a port separates, so each needs its own OpenVMM rule.
fn separate_port_ranges(protocol: Protocol, count: u16) -> NetworkRule {
    let mut rule = NetworkRule::to("192.0.2.0/24");
    rule.ports = (0..count)
        .map(|index| NetworkPort {
            protocol,
            port: Some(3 * index + 1),
            end_port: Some(3 * index + 2),
        })
        .collect();
    rule
}

#[test]
fn provision_validation_rejects_backend_policies_before_probing() {
    let (directory, client) = client();
    let before = state_entries(&directory);
    for request in [
        network_request(separate_port_ranges(Protocol::Tcp, 257)),
        network_request(separate_port_ranges(Protocol::Any, 129)),
    ] {
        assert_eq!(
            client.validate_provision(&request).unwrap_err().code(),
            ErrorCode::PolicyValidation
        );
        assert_eq!(
            client.provision(&request).unwrap_err().code(),
            ErrorCode::PolicyValidation
        );
        assert_eq!(
            client.backend().provision(&request).unwrap_err().code(),
            ErrorCode::PolicyValidation
        );
    }
    assert_eq!(state_entries(&directory), before);
}

#[test]
fn kernel_command_lines_without_room_are_rejected_before_probing() {
    // The artifacts are missing, so a check that ran after probing them would report them instead.
    let directory = tempfile::tempdir().unwrap();
    let mut oversized = config(&directory);
    oversized.kernel_command_line = "x".repeat(1024);
    let error = AciEdgeSandbox::openvmm(oversized).unwrap_err();
    assert_eq!(error.code(), ErrorCode::BackendUnavailable);
    assert!(
        error.message().contains("1024-byte kernel command line"),
        "{error}"
    );
    assert!(!directory.path().join("state").exists());
}

#[test]
fn validation_is_side_effect_free_and_host_checks_remain_in_operations() {
    let (directory, client) = client();
    let before = state_entries(&directory);
    for request in [
        ProvisionRequest::new(),
        network_request(port_range(Protocol::Tcp, 65535)),
        network_request(port_range(Protocol::Any, 65535)),
        network_request(separate_port_ranges(Protocol::Tcp, 256)),
        network_request(separate_port_ranges(Protocol::Any, 128)),
        network_request(NetworkRule::to("192.0.2.0/24").on_protocol(Protocol::Tcp)),
        network_request(NetworkRule::to("192.0.2.0/24").on_protocol(Protocol::Udp)),
        network_request(NetworkRule::to("192.0.2.0/24").on_protocol(Protocol::Icmp)),
        network_request(NetworkRule::to("::/0")),
        network_request(NetworkRule::to("2001:db8:1::/64").on_port(Protocol::Tcp, 443)),
        network_request(NetworkRule::default().on_port(Protocol::Any, 53)),
        ProvisionRequest::new().with_filesystem(FilesystemPolicy {
            readonly_paths: vec![directory.path().join("missing-mapped-file")],
            ..FilesystemPolicy::default()
        }),
    ] {
        client.validate_provision(&request).unwrap();
        client.backend().validate_provision(&request).unwrap();
        assert_eq!(
            client.provision(&request).unwrap_err().code(),
            ErrorCode::BackendUnavailable
        );
    }
    assert_eq!(state_entries(&directory), before);
}

#[test]
fn structural_and_capability_errors_precede_backend_policy_checks() {
    let (_directory, client) = client();
    assert_eq!(
        client
            .validate_exec(&ExecRequest::argv([""]))
            .unwrap_err()
            .code(),
        ErrorCode::MalformedRequest
    );
    let request = ExecRequest::argv(["relative-program"]).with_env("KEY=value");
    let error = client.validate_exec(&request).unwrap_err();
    assert_eq!(error.code(), ErrorCode::PolicyValidation);
    assert!(error.message().contains("absolute guest path"));
    // An unsupported feature is a capability error, which precedes the program check.
    let request = ExecRequest::argv(["relative-program"]).with_stdin(StdinMode::Piped);
    let error = client.validate_exec(&request).unwrap_err();
    assert_eq!(error.code(), ErrorCode::PolicyValidation);
    assert!(error.message().contains("standard input"), "{error}");
    // A malformed entry is a structural error, which precedes the program check too.
    let request = ExecRequest::argv(["relative-program"]).with_env("KEY");
    assert_eq!(
        client.validate_exec(&request).unwrap_err().code(),
        ErrorCode::MalformedRequest
    );
    assert_eq!(
        client
            .validate_provision(&ProvisionRequest::new().with_memory_mib(0))
            .unwrap_err()
            .code(),
        ErrorCode::MalformedRequest
    );
}

#[cfg(feature = "async")]
#[test]
fn async_execution_uses_the_same_backend_validation_hooks() {
    let (directory, client) = client();
    let before = state_entries(&directory);
    let client = aci_edge_sandboxes::AsyncAciEdgeSandbox::from(client);
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let error = client
                .exec(stale_id(), ExecRequest::argv(["relative-program"]))
                .await
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::PolicyValidation);
            let error = client
                .provision(network_request(separate_port_ranges(Protocol::Tcp, 257)))
                .await
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::PolicyValidation);
        });
    assert_eq!(state_entries(&directory), before);
}
