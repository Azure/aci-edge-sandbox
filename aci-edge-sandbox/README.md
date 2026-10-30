# aci_edge_sandboxes

`aci_edge_sandboxes` is the Rust interface to the ACI Edge Sandboxes lifecycle.
It exposes provision, start, exec, stop, and deprovision through a pluggable
backend. The default backend, `agent`, uses a separately supplied native library,
`aci_edge_agent`, which runs commands against a caller-supplied, read-only GPT
image with a RAM overlay.

The Cargo package and library are named `aci_edge_sandboxes`; the crate lives in the
repository's `aci-edge-sandbox` directory.

## Quick start

Create a client over the agent library with `AciEdgeSandbox::agent`, then provision, start,
exec, stop, and deprovision; see [Agent backend](#agent-backend). A sandbox boots only an image
that the library has registered: the setup's default image, which the backend registers when it
opens, the local paths of the setup's `images.prefetch`, which it registers in the background,
or an image that `AgentBackend::register_image` or `AgentBackend::register_reference`
registered first. Provision and start never pull, copy, or register an image.
[`examples/agent_lifecycle.rs`](examples/agent_lifecycle.rs) is a runnable version with
command-line arguments.

## Lifecycle

| Operation | Transition | Result |
| --- | --- | --- |
| `AciEdgeSandbox::provision` | (none) → provisioned | `SandboxId` (`aci-edge-sandboxes:<32 hex>`) and optional metadata |
| `AciEdgeSandbox::start` | provisioned → running | optional metadata |
| `AciEdgeSandbox::exec` | running → running | live stdout and stderr, then an `ExecOutcome` |
| `AciEdgeSandbox::stop` | running → provisioned | optional metadata |
| `AciEdgeSandbox::deprovision` | provisioned → (none) | optional metadata; the ID becomes stale |

Images are registered outside this lifecycle. With a backend that registers images, such as the
agent backend, `provision` takes the image that its spec names, or the setup's default image,
only if that image is registered, and fails with `BackendUnavailable` otherwise. `start` boots
the image that the sandbox was provisioned with.

`ExecOutcome` distinguishes `Exited(code)`, `Signaled(signal)`, `TimedOut`,
`Cancelled`, and `Failed(reason)`. `Failed(WorkingDirectory)` means that the
workload never ran because it could not enter its working directory. An `Err`
from `Execution::wait` means the outcome could not be determined, for example
because the VM stopped.

Request types serialize with the contract's JSON field names (`readonlyPaths`,
`memoryMib`, `commandLine`, and so on) and reject unknown fields. Every
provision field is optional, so `{}` is a valid provision request. Like the contract's
schema, they accept a `process.timeout` from 0 through 4,294,967,295
milliseconds, about 49.7 days, and refuse a negative or larger one. The wire
envelope (`version`, `phase`, and `containment`) belongs to the caller.

## Architecture
- The NVX mechanics live in the [`nvx` crate](https://github.com/microsoft/nvx/blob/6b485c82c9fdf4f592e1221efc7d6d41ab9bc66c/nvx-rs/README.md),
  which this crate re-exports as `aci_edge_sandboxes::nvx`. This crate keeps the ACI Edge
  Sandboxes contract: the facade, `Backend`, the data model and its validation, the error
  codes, `SandboxId`, the execution and stream plumbing, and the asynchronous
  and mock APIs; see [Error mapping](#error-mapping).
- `AciEdgeSandbox` is the entry point. It validates every request in the order the
  contract requires, then calls a backend:
  1. structural checks, reported as `malformed_request` or `malformed_id`;
  2. capability checks against the backend's `Capabilities`, reported as
     `policy_validation`;
  3. deterministic backend policy checks, such as guest argument and egress
     rule limits, through side-effect-free validation hooks;
  4. host-dependent checks, such as artifact or hypervisor availability, only
     when performing the operation.

  A rejected request never runs anything.
- `Backend` is an object-safe trait with one method per operation plus
  `capabilities`, `probe`, and provision/exec validation hooks. Implement those
  hooks with the same deterministic policy checks used by the operations;
  validation must not inspect host files, create state, or launch processes.
  Backends push workload output into
  `OutputSink`s and return an `ExecControl` for waiting and cancellation. The
  facade decides how output reaches the caller: operating-system pipes in the
  synchronous API, and in-process streams in the asynchronous API. The facade's
  sinks return an `OutputCloser` from `OutputSink::close_handle`; close it on
  cancellation or timeout when a write could wait for a caller that stopped
  reading.
  `ExecIo::stdin` supplies an interruptible `InputSource`. Clone its
  `InputCloser` to wake pending reads on cancellation or timeout, and finish
  input workers before reporting completion. Synchronous callers still receive
  the original OS-pipe writer; writes fail once the backend has closed input.
- Backends in this crate:
  - `agent::AgentBackend` (feature `agent`, default) forwards the lifecycle to a separately
    supplied native library, which drives OpenVMM and an image-backed guest.
  - `testing::MockBackend` (feature `testing`) implements the state machine in
    memory for consumers' unit tests.
- `AsyncAciEdgeSandbox` (feature `async`) wraps `AciEdgeSandbox` for Tokio. Lifecycle calls run on
  the blocking pool, and output arrives as `AsyncRead` streams. A call keeps
  running when its future is dropped. A dropped `exec` requests the
  cancellation of its workload as soon as the workload starts, but only
  backends whose `capabilities().exec.cancel` is true, such as the
  agent backend, honor the request; with other backends, the workload
  runs until it ends. The other calls complete, so the sandbox's state shows
  their effects.

To add a backend, implement `Backend`. Declare only the capabilities it can
enforce, and report state-machine violations with the codes listed in the
[error mapping](#error-mapping).

## Agent backend

The `agent` feature (on by default) provides `AciEdgeSandbox::agent(AgentConfig)`. This backend is a thin client of a
separately supplied native library, `aci_edge_agent.dll` (`libaci_edge_agent.so` on Linux),
which owns the whole sandbox lifecycle: the state root and its records, the OpenVMM processes
and their boot consoles, the guest sessions, and the guest images. The crate does **not** build or publish the library,
and it downloads one only with the `agent-bundled` feature; see
[Bundled agent](#bundled-agent-agent-bundled).

`AgentConfig` names the library, the SHA-256 of the library that the caller's own policy
approved, and a `SetupConfig`, which the library applies once per process:

```jsonc
{
  "stateRoot": "C:\\nvx-edge-state",
  // A release bundle trusted through its manifest's SHA-256, or explicit files:
  // "runtime": { "files": { "openvmm": "…", "kernel": "…", "initrd": "…" } }
  "runtime": { "bundle": { "path": "C:\\nvx\\edge", "manifestSha256": "<64 hex digits>" } },
  "hypervisor": "whp",          // optional; KVM on Linux and WHP on Windows otherwise
  "cpuProfile": "auto",         // or "host"
  "timeouts": { "startMs": 60000, "controlMs": 60000, "stopMs": 30000, "execGraceMs": 30000 },
  "defaults": {                 // what a sandbox inherits unless its spec sets it
    "image": { "path": "C:\\images\\layers.gpt" },
    "resources": { "vcpus": 1, "memoryMib": 256 },
    "guestNetwork": "10.0.0.2/24"
  },
  "images": {
    // Local images that the library registers in the background when it opens.
    "prefetch": [{ "path": "C:\\images\\tools.gpt" }],
    // How long a library that pulls registry references may take to materialize one.
    "references": { "pullTimeoutMs": 1800000 },
    // Optional: a limit on the image cache, in bytes; see "Image cache limit" below.
    "maxCacheCapacityBytes": 10737418240
  },
  "diagnostics": { "guestDebug": false },
  // Optional: templates that sandboxes start as clones of; see "Platform snapshots".
  "platformSnapshots": { "enabled": true, "maxVcpus": 4, "maxMemoryMib": 8192 }
}
```

Every field except `stateRoot` and `runtime` has a default. A bundle directory holds a format-2
`SOURCE-MANIFEST.json` of the `edge` profile, `bin/openvmm[.exe]`, `bin/direct-images[.exe]`,
the library itself (`bin/aci_edge_agent.dll` or `bin/libaci_edge_agent.so`), `guest/vmlinux`,
and `guest/initramfs-edge.cpio.gz`. The manifest pins every file's SHA-256 and the guest's
runtime ABI. The library checks every file but itself: the caller passes the library's SHA-256
(`agent.library_sha256` in the manifest) to `AgentConfig`. Explicit files can be pinned with
`RuntimeFiles::approved_sha256`.

A `SandboxSpec`, passed to `AciEdgeSandbox::provision_with` beside the provision request, sets
one sandbox's image (`ImageSource::Path` of a registered local GPT disk, `ImageSource::Digest` of
a registered image, or `ImageSource::Reference` of a registered registry image where the library
supports it), its resources, its guest network, and its forwarded ports. The library accepts 1,
2, 4, or 8 virtual processors, the counts that OpenVMM's microVM accepts, in `resources.vcpus`
and in the setup's `defaults.resources.vcpus`, and refuses other counts; `resources.memoryMib`
sets the guest's memory.
`Capabilities::spec` lists the fields that the library honors; hostnames are not among them yet.
A request may still set `microvm.provision.memoryMib`, but not together
with `spec.resources.memoryMib`.

A library that honors `spec.image.reference` (`Capabilities::spec.image_reference`) registers a
reference with `register_reference`, or as the setup's default image when a backend opens: it
converts the reference into a local disk image with an external tool, once, and keeps the
result by the reference's text. A sandbox boots only a registered reference.
The tool comes from the runtime bundle (`bin/direct-images[.exe]`), so a library given explicit
runtime files does not materialize references. It does not check a tag again, so a moving tag
such as `latest` keeps naming the content that was first pulled: use a version tag or a digest
(`repository@sha256:…`) to control what a sandbox boots. `images.references.pullTimeoutMs`
limits each pull (30 minutes by default).

For private registries, set `images.references.authFile` to a host-side
Docker-compatible JSON credentials file, or use
`ReferenceSettings::with_auth_file`. The setup carries only its path:

```json
{
  "images": {
    "references": {
      "authFile": "C:\\edge-secrets\\registry-auth.json"
    }
  }
}
```

The file's `auths` entries are keyed by the registry host, including its port
when present. Entries support `auth` (base64 of `username:password`),
`username` with `password`, or `registrytoken` for an already issued registry
bearer token. Base64 is **not encryption**; protect the file with host
permissions or ACLs. Credential helpers (`credHelpers` and `credsStore`) and
identity/refresh tokens are not supported and fail explicitly. The host does
not search the user's Docker configuration or other credential files.

The host selects credentials only for the requested registry, and rereads
the file on each actual pull so it can be replaced to rotate credentials.
An unrelated registry is pulled anonymously; a malformed or unreadable
selected file fails the pull. Local images and registered references do not
read it. The existing state root remains the trust boundary of registered
images; credentials do not restrict access to an image already registered
there.

The native host passes credentials through the image tool's existing
password/token command-line options. They are visible to users who can
inspect that process, even though the client receives only the file path.
Do not map the credential file into a guest; the host rejects mappings that
would expose it.

- **Loading.** `AgentBackend::new` checks the library's SHA-256 and sandbox ABI version and
  loads it once per process; it stays loaded until the process exits. It loads the bytes that it
  checked. On Linux it loads them from a sealed in-memory copy, and fails if the kernel refuses
  to create or execute one. On Windows, its open handle keeps writers out of the file and keeps
  the file and its directories from being renamed or deleted until the library is loaded. Other
  hosts cannot load the library. A missing, wrong-version, or wrong-digest library fails rather
  than falling back.
- **Opening.** The first backend that a process opens for a setup verifies the runtime files, or
  checks the seals that an earlier process recorded under the state root, and registers the
  default image. It also starts registering the local paths of `images.prefetch` in the
  background; a provision that names one fails until its registration finishes, or if it failed.
  Later backends for the same setup reuse that host without touching a file, so a caller may
  create a backend for every operation; the operations check the files that they use. One that
  finds a runtime file changed makes the next backend verify the files again, and one that finds
  the default image changed or unregistered makes it register the image again.
  Hosts, guest sessions, and boot-console capture stay in the library for the life of the
  process; dropping a backend leaves its sandboxes running.
- **Images.** The library registers images under the state root by their content ID,
  `sha256:<hex>` (`ImageId`), the SHA-256 of the registered file as the caller gave it.
  Registration is the one operation that reads an image: it hashes the image once, unless
  `ImageDigest::Trusted` records a digest that the caller's own policy verified, and
  `ImageDigest::Expect` additionally requires a digest. It then stages the image as a read-only
  copy of the library's own, in raw form (a fixed VHD loses its footer but keeps its ID), which
  `RegisteredImage::path` names and sandboxes attach, so later changes to the registered file do
  not reach them. A later registration, in any process, reuses one that the file still matches
  without reading the file. Provision takes only registered images: by ID, by the path that was
  registered, or by the reference. One that is not registered fails the provision with
  `backend_unavailable`: register it first. A path whose file changed since its registration
  fails until it is registered again, and provision never pulls, copies, or hashes an image; it
  records the image ID and the runtime digests. Start compares the seal of each runtime file and
  of the staged copy (volume, file ID, length, and last-write time, plus the change time on
  Linux) with the one taken when it was hashed or staged, instead of hashing again, hands OpenVMM
  the copy that it checked, and fails with `backend_unavailable` if anything changed, so a start
  costs about the guest's boot time. A seal detects replacement or modification, not a writer that
  deliberately restores timestamps. On Windows, start also keeps writers out of the files until
  OpenVMM has opened them. Register an image again with `register_image` to stage it anew:
  unchanged content keeps its ID, so sandboxes that use it start again. `images` lists the
  registrations and whether each staged copy still matches, `verify_image` hashes one again, as a
  diagnostic, and `unregister_image` refuses while a provisioned sandbox uses the image, and
  otherwise removes its staged copy.
- **Image cache limit.** `images.maxCacheCapacityBytes` limits the combined length, in bytes, of
  the staged copies that the library's image cache counts. Omitted, the cache has no limit; zero
  is refused.
  - The cache counts registry references, and the setup's default image and prefetched paths.
    Other images registered by path do not count, and the library never evicts them.
  - A registration that makes an image count admits it first. If the counted images and the new
    one do not fit, the library evicts unused references, the largest first, until it fits. It
    refuses an image longer than the limit with `policy_validation`, and one that the unused
    references cannot make room for with `backend_unavailable`, and then evicts nothing.
  - The library never evicts an image that a provisioned sandbox uses, from its provision until
    its deprovision, whichever process provisioned it. The setup's default image and prefetched
    paths are protected: counted, but never evicted, until they are unregistered. An image
    registered both by path and by reference counts, and is never evicted.
  - Eviction unregisters the image. A provision that names an evicted reference fails with
    `backend_unavailable`, and `register_reference` registers it again, which pulls it.
  - Each backend enforces its own setup's limit when it registers an image, so processes whose
    setups differ can share a state root. A backend without a limit counts, protects, and evicts
    nothing, so another with a limit may evict its default image when that is an unused
    reference. A library that does not support the limit refuses a setup that sets it.
- **The guest.** The image is attached read-only; the edge guest validates its GPT and p2+ ext4
  layers and keeps its writable layer in RAM, so it needs no scratch disk. Start sends OpenVMM a
  fresh 32-byte capability through standard input, checks the process that serves the control
  endpoint, and waits until the guest agent is ready.
- **Guest sessions.** OpenVMM accepts one host session per VM at a time. The library keeps one
  per running sandbox for the process that uses it, and that process's operations share it, so
  they never wait for each other's sessions, and its executions run side by side over it.
  Another process waits until the holder releases the session, on stop, deprovision, or exit.
  `guestSession.hold: false` releases a session once it has been idle for `lingerMs`, for
  callers that drive one sandbox from several long-lived processes.
- **Interrupted starts.** Start claims the sandbox's OpenVMM log before launching, and OpenVMM
  inherits the claim. If the caller dies before recording OpenVMM's identity, the next operation
  waits up to the start timeout for that OpenVMM to open its endpoint and then terminates it;
  once nothing holds the log, the sandbox is usable again.
- **Boot console.** The library copies the guest boot console to the console log that
  `AgentBackend::diagnostics` names, from the process that started or last used the sandbox.
  OpenVMM serves one console client at a time and holds guest output while none is connected, so
  a guest that writes enough console output stalls until a listener reconnects: keep that
  process running, or use the sandbox from its successor, whose listener takes the capture over.
- **Metadata.** Start reports `bootMode`, `bootMilliseconds`, `guestBuildId`, and `phases`
  (`spawnMs`, `attachMs`, and `readyMs`), and, with [platform snapshots](#platform-snapshots),
  `coldReason` or `template`. Stop reports `forced`, `phases` (`shutdownMs` and `exitMs`), and,
  after a failed graceful shutdown, `gracefulError`. A capture failure never fails stop or
  deprovision: both report it as `consoleError`. `AgentBackend::guest_logs` reads a bounded
  snapshot of the guest's log while it runs.

Executions run shell commands or argv and stream while they run. Output arrives as the command
writes it, with no size limit, `ExecIo::stdin` feeds the command's standard input, and
`ExecControl` cancels it. Each execution's flow control bounds the output that the library
buffers for it, so a caller that stops reading pauses that command, not the others. At most 16
executions run at once per sandbox; one more fails with `backend_error`. A timeout is rounded up
to whole milliseconds, can be up to an hour, and ends the command as `TimedOut`. When it elapses
before the outcome arrives, `wait` closes the output streams, as cancellation does, so that a
stream that the caller holds without reading cannot delay the outcome; an execution that loses
output that way ends as `TimedOut`.
Guest-side image pulls are not part of this profile.

[`examples/agent_lifecycle.rs`](examples/agent_lifecycle.rs) runs one lifecycle. It needs
`--host-library`, `--host-sha256`, `--state-root`, either `--image` or `--image-ref`, and either `--openvmm`, `--kernel`,
and `--initrd` or `--bundle` and `--bundle-sha256`, and the command after `--`, unless it is built
with `agent-bundled`, which gives each of them a default; see
[Bundled agent](#bundled-agent-agent-bundled). `--image-sha256`
requires a local image's digest when it is registered. `--image-ref` requires a bundle;
`--registry-auth-file PATH` selects its credentials file and `--pull-timeout-ms` sets its pull
deadline. `--hypervisor`, `--cpu-profile auto|host`, and `--guest-debug` adjust the setup, and
`--platform-snapshots MAX_VCPUS,MAX_MEMORY_MIB[,MAX_IMAGE_MIB]` enables
[platform snapshots](#platform-snapshots) in it. `--vcpus N` and `--memory-mib N` size the
sandbox, and the example prints its start's `bootMode` and any `coldReason` on standard error.
The repeatable `--readonly`,
`--readwrite`, and `--denied` options map host paths, and `--egress allow|deny` with the
repeatable `--egress-allow` and `--egress-deny` options, each taking `CIDR` or
`CIDR:tcp|udp:PORT`, attach a network. `--network-proxy URL` routes the guest's traffic through
a host proxy, and `--host-loopback allow|deny` with the repeatable
`--host-loopback-forward tcp|udp:HOST:GUEST` publishes guest ports on host loopback. The
repeatable `--env KEY=VALUE`, layered over the guest's environment, and `--cwd PATH` apply to the
command. The ignored `tests/agent_guest.rs` exercises an actual guest when the corresponding
`EDGE_AGENT_TEST_*` paths and approved library digest are set: under WHP on Windows, or under the
hypervisor that `EDGE_AGENT_TEST_HYPERVISOR` names, such as `mshv` on Linux.
`EDGE_AGENT_TEST_CPU_PROFILE=host` boots its guests on a host CPU profile.

For example, from `aci_edge_sandboxes` on a Windows WHP host, set the following paths to
compatible, separately built artifacts and a caller-prepared GPT disk with p2+ ext4 layers (not
a container image reference). Use the **edge** guest initramfs, not the default Alpine or
standard container guest initramfs:

```powershell
$openvmm = 'C:\path\to\openvmm.exe'
$kernel = 'C:\path\to\vmlinux'
$initrd = 'C:\path\to\nvx-edge-initramfs.cpio.gz'
$image = 'C:\path\to\layers.gpt'
$hostLibrary = 'C:\path\to\aci_edge_agent.dll'
$approvedHostSha256 = '<64 hexadecimal digits from an independent trust policy>'
$stateRoot = 'C:\nvx-edge-state'

cargo run --release --locked --features agent --example agent_lifecycle -- `
    --openvmm $openvmm --kernel $kernel --initrd $initrd --image $image `
    --host-library $hostLibrary --host-sha256 $approvedHostSha256 `
    --state-root $stateRoot -- 'printf READY'
```

To pull a private image through the library instead, use the verified bundle
and replace `--image` with `--image-ref`:

```powershell
cargo run --release --locked --no-default-features --features agent --example agent_lifecycle -- `
    --bundle 'C:\path\to\edge-bundle' --bundle-sha256 '<approved manifest SHA-256>' `
    --image-ref 'registry.example:5443/team/image:1' `
    --registry-auth-file 'C:\edge-secrets\registry-auth.json' --pull-timeout-ms 120000 `
    --host-library $hostLibrary --host-sha256 $approvedHostSha256 `
    --state-root $stateRoot -- 'printf READY'
```

To map `C:\work\src` read-only and `C:\work\out` read-write, and let the guest reach only
`192.0.2.10` on TCP port 443, add
`--readonly C:\work\src --readwrite C:\work\out --egress deny --egress-allow 192.0.2.10:tcp:443`
before `--`; `nvx::resolve_guest_path` gives the guest paths, here `/mnt/c/work/src` and
`/mnt/c/work/out`. To let the guest out only through a proxy that listens on host port 3128, add
`--egress deny --network-proxy http://127.0.0.1:3128` instead; to reach a guest service on port
8080 at host `127.0.0.1:18080`, add
`--egress allow --host-loopback allow --host-loopback-forward tcp:18080:8080`.

Build the native library and the static edge initramfs separately from their matching sources;
this example neither fetches nor builds them unless it is built with `agent-bundled`. Use the
pinned OpenVMM, which includes the
scratchless RAM-overlay topology, and the NVX kernel that its time ABI requires, such as a
release's `guest/vmlinux`. OpenVMM selects a [CPU profile](https://github.com/microsoft/nvx/blob/6b485c82c9fdf4f592e1221efc7d6d41ab9bc66c/doc/usage.md#cpu-profiles) at
every cold boot, so start fails with `backend_error` on a host that none of its built-in
profiles serves, or whose hypervisor does not support its profile; the OpenVMM log that the
error names gives the reason. On such a development host, `cpuProfile: "host"` (the example's
`--cpu-profile host`) boots the guests on a host profile, which OpenVMM derives from the host's
hypervisor at every cold boot and does not pin. On Linux, supply a matching
`libaci_edge_agent.so` and OpenVMM build and select `--hypervisor mshv`.

### Platform snapshots

A setup's optional `platformSnapshots` object (`SetupConfig::with_platform_snapshots`) lets
sandboxes start as clones of templates instead of cold booting, which is faster:

```jsonc
"platformSnapshots": {
  "enabled": true,        // required
  "maxVcpus": 4,          // 1, 2, 4, or 8; required when enabled
  "maxMemoryMib": 8192,   // a positive multiple of 128; required when enabled
  "maxImageMib": 65536    // optional; 64 GiB unless set
}
```

- **Templates.** The library keeps two templates for the setup, microVM snapshots that hold no
  tenant data: one without a NIC, and one with a NIC on `defaults.guestNetwork` that denies all
  traffic. They boot with the default memory, `defaults.resources.memoryMib`, which must then be
  whole 128 MiB blocks, no more than `maxMemoryMib`. `maxVcpus` and `maxMemoryMib` are the
  templates' capacities, and `maxImageMib` is the longest image that a clone can use.
  `SetupConfig::validate` checks each field that is present, even when `enabled` is false.
- **Capture.** The first open of a setup that enables them captures the templates, which takes
  seconds; later opens, in any process, reuse them. A failed capture fails the open, and the next
  open tries again. The library captures a template again in the background before it expires,
  and after a clone fails because of it; meanwhile, the starts that need it cold boot.
- **Which starts clone.** A start clones when the setup does not set `diagnostics.guestDebug`
  and a ready template fits the sandbox: at most `maxVcpus` virtual processors; memory of whole
  128 MiB blocks, from `defaults.resources.memoryMib` through `maxMemoryMib`; no NIC, or a NIC on
  `defaults.guestNetwork`; and an image no longer than `maxImageMib`. Every other start cold
  boots, as without platform snapshots, and its `coldReason` says why: `guest-debug`,
  `vcpus-above-capacity`, `memory-not-whole-blocks`, `memory-below-template`,
  `memory-above-capacity`, `guest-network`, `image-too-long`, `no-eligible-template` (none was
  captured yet, or the template of its class expired or failed a clone and is being captured
  again), `plan-refused` (the library refused the clone for another reason), or `template-fault`
  (a clone failed because of its template, and the start cold booted instead).
- **Start metadata.** Every start reports `bootMode`, `clone` or `cold`, and a clone reports its
  `template`: `{"class": "noNic" | "nic", "key": "<64 hex digits>", "generation": 3}`.
  `StartResult::boot_mode`, `cold_reason`, and `template` read them, returning `None` for an
  absent key and `backend_error` for a malformed one.
- **Host info.** `AgentBackend::info().platform_snapshots` describes the templates as of when the
  backend opened the host. `key`, `generation`, and `capturedAt` (RFC 3339 in UTC) describe the
  newest generation of a template, when one exists, and `lastError` its last failed capture or
  refresh:

  ```jsonc
  "platformSnapshots": {
    "disabledReason": "guest-debug",   // only when no template is kept despite the setup
    "templates": [
      { "class": "noNic", "state": "ready", "key": "<64 hex digits>", "generation": 3,
        "capturedAt": "2026-10-29T07:00:00Z" },
      { "class": "nic", "state": "unavailable", "lastError": "…" }
    ]
  }
  ```
- **Limits.** A clone runs with the kernel's default CPU vulnerability mitigations, where a cold
  boot turns them off, and its console log holds no guest boot output. Both last until the
  library's platform lifts those limits.

### Bundled agent (`agent-bundled`)

The `agent-bundled` feature enables `agent` and makes the build script download, verify, and
stage a complete edge agent bundle: the agent library, OpenVMM, the edge guest kernel and
initramfs, the image tool, their manifests, and `SHA256SUMS`. A program built with it needs no
paths or digests:

```rust
use aci_edge_sandboxes::AciEdgeSandbox;
use aci_edge_sandboxes::agent::AgentConfig;

let client = AciEdgeSandbox::agent(AgentConfig::bundled("C:\\nvx-edge-state")?)?;
```

`BundledAgent::discover` returns the bundle's directory, runtime, library, and digests, for a
setup of one's own: `bundle.config(bundle.setup(state_root).with_cpu_profile(...))`.

By default the build script downloads the release asset that
[`agent-bundle.json`](agent-bundle.json) pins: the release tag (`release`), the asset's file name
(`asset`), and its SHA-256 (`sha256`). It downloads
`https://github.com/Azure/aci-edge-sandbox/releases/download/<release>/<asset>` and refuses the
ZIP unless its SHA-256 is the pinned one. To move to another bundle, publish it as a release asset
and update all three fields of `agent-bundle.json` together.

> **Note:** The default build and unit tests pass. The currently pinned bundle provides sandbox
> ABI 1, while this crate requires ABI 3. Consequently, `agent-bundled` builds and runtime use
> with that bundle fail the compatibility check. A compatible bundle and pin update are pending.

Releases publish no bundle for Linux, so a Linux build needs one of the variables below. Other
targets, where the agent library cannot run, stage nothing. The build script
runs again when `agent-bundle.json` or a variable below changes. It
also runs again when the copy beside the executables (see below) is replaced, changed, or
removed, for example by a build of another feature configuration that uses a different bundle,
and restores the bundle that the configuration being built pins.

| Build variable | Effect |
| --- | --- |
| `ACI_EDGE_SANDBOXES_AGENT_BUNDLE_URL` | Downloads this bundle ZIP instead of the pinned one, from any URL that `curl` reads, such as a mirror; a `file://` URL names a ZIP on the build host |
| `ACI_EDGE_SANDBOXES_AGENT_BUNDLE_DIR` | Stages this extracted bundle instead, for offline builds or locally built bundles |
| `ACI_EDGE_SANDBOXES_AGENT_BUNDLE_SHA256` | Requires the ZIP that `ACI_EDGE_SANDBOXES_AGENT_BUNDLE_URL` names to have this SHA-256; without that variable, it must equal the pinned SHA-256 |
| `ACI_EDGE_SANDBOXES_AGENT_BUNDLE=skip` | Stages nothing, as when the feature is disabled; `BundledAgent::discover` then fails with `backend_unavailable`. CI sets it for `--all-features` builds |

The build fails unless every file that `SHA256SUMS` lists matches it, the bundle holds no links,
`SOURCE-MANIFEST.json` is a format-2 manifest of the `edge` profile whose pins match
`bin/openvmm[.exe]`, `guest/vmlinux`, `guest/initramfs-edge.cpio.gz`,
`bin/direct-images[.exe]`, and the library, `bundle-manifest.json` describes a complete bundle
whose consumer section names that manifest and an agent library for the target platform with
the digests found, and the library speaks this crate's sandbox ABI. These checks prove that the
bundle is intact, not that it is authorized: the trust comes from the pinned SHA-256, or, for a
bundle named by `ACI_EDGE_SANDBOXES_AGENT_BUNDLE_URL`, from its source and
`ACI_EDGE_SANDBOXES_AGENT_BUNDLE_SHA256`.
Extracting the ZIP needs bsdtar (Windows' own `tar`) or `unzip` on the build host.

The build script stages the bundle in its output directory, which the build scripts of
dependents read as `DEP_ACI_EDGE_SANDBOXES_AGENT_ARTIFACTS_DIR`, and copies it to an
`aci-edge-sandboxes` directory in Cargo's profile directory, such as `target/release`, beside
the executables that Cargo builds. The crate compiles in only the SHA-256 digests of the
bundle's `SOURCE-MANIFEST.json` and library. `BundledAgent::discover` uses, in this order:

1. The directory that `ACI_EDGE_SANDBOXES_AGENT_ARTIFACTS_DIR` names, when it is set and not
   empty. The directory must then hold a copy of the bundle; discovery does not fall back.
2. An `aci-edge-sandboxes/` directory next to the current executable, if it holds the bundle.
3. One next to the executable's directory, which serves Cargo's `examples/` and `deps/`.

An executable therefore keeps working when it is copied elsewhere together with the
`aci-edge-sandboxes` directory, as long as their relative positions stay the same, and the
host refuses a bundle whose files differ from the digests that were compiled in. When Cargo's
`build-dir` is not the target directory, the copy goes there and serves only test executables;
for the others, copy the bundle from `DEP_ACI_EDGE_SANDBOXES_AGENT_ARTIFACTS_DIR` next to the
executable or set `ACI_EDGE_SANDBOXES_AGENT_ARTIFACTS_DIR`.

Built with `agent-bundled`, [`examples/agent_lifecycle.rs`](examples/agent_lifecycle.rs) runs
without any option:

```powershell
cargo run --release --locked --features agent-bundled --example agent_lifecycle
```

It uses the staged bundle and library, keeps its state in `aci-edge-sandboxes-state/` beside
the executable, boots `mcr.microsoft.com/azurelinux/base/core:3.0`, and runs
`printf READY; echo; head -n 2 /etc/os-release`. Every option still overrides its default, and
`--bundle`, `--openvmm`, or `--host-library` options replace the staged runtime or library.

### Native host paths and network

The native backend maps host paths to guest paths and reads network rules as described in
[Host paths](#host-paths) and [Network rules](#network-rules). It
exports one directory for all mapped paths, under the rules below. It also
refuses mappings that would show workloads its state root, described below. Its workloads run
as the guest's root. The library plans the policies at provision: it resolves the mapped and
denied paths, chooses the export, pins the objects inside read-write mappings, and expands the
network rules, and the sandbox's record keeps the resulting plan. Every start checks the plan
again against every planning rule that needs no host access, and checks the pinned objects: if
one was replaced, start fails with `backend_error`, and the sandbox must be deprovisioned and
provisioned again to accept the change.

- The state root holds the record, and so the plan, of every sandbox, which decides what the
  next start exports, which egress it allows, and which host loopback ports the guest reaches
  or publishes. Start's checks catch a damaged plan, but not one changed into another plan that
  planning could have produced, so only the caller may write the state root. Provision
  therefore refuses, with `policy_validation`, a mapped path inside the state root, and one that
  contains it unless a denied path inside the mapping hides it.
- OpenVMM exports the deepest directory that contains every mapped path through one
  virtio-fs device, read-only unless a path is read-write, and hides the denied paths. The
  guest mounts the export where workloads cannot reach it and bind-mounts each mapped path
  into the RAM overlay at its guest path, read-only where requested. The guest's kernel command
  line announces only how many paths are mapped: the guest receives the bind mounts in one
  request of at most 256 KiB once it is ready, and the start completes once it has bound them.
  OpenVMM's command line names the exported, hidden, and writable paths, and every host keeps
  it, with the network rules, within the 32,767 characters that Windows allows a process, less
  room for the rest of the launch. That leaves room for over a hundred long paths; provision
  rejects a policy that does not fit with `policy_validation`.
- Denied paths stay hidden through their aliases: `..`, host symbolic links and junctions,
  links that workloads create, and other names of a denied object, such as a host hard link to
  a denied file. OpenVMM never follows a link on the host, so the guest resolves every link
  itself, and each lookup of a denied path fails with `EACCES`; an absolute Windows symbolic
  link or a junction cannot be followed at all (`EPERM`). OpenVMM recognizes only the denied
  objects themselves: a host hard link that already joins a file inside a denied directory to a
  name outside it stays readable through that name. A directory that holds a host hard link to
  a denied file may list the link's name, and a listing that also looks its entries up, as the
  guest's first read of a directory does, fails with `EACCES`.
- Workloads run as the guest's root with only the default container capabilities (`CHOWN`,
  `DAC_OVERRIDE`, `FOWNER`, `FSETID`, `KILL`, `SETGID`, `SETUID`, `NET_BIND_SERVICE`, and
  `AUDIT_WRITE`), with `no_new_privs`, and without user namespaces, so they cannot remount or
  unmount the mapped paths or mount the export again. Host-side changes through read-write
  mappings, including modes and ownership, happen with the credentials of the account that
  runs OpenVMM, so run OpenVMM unprivileged. A host hard link that already joins a file in a
  read-write mapping to one in a read-only mapping stays writable through the read-write
  path.
- A policy that denies egress without allow rules or a proxy attaches no network device.
  Otherwise the guest gets its spec's `guestNetwork`, or the setup's default (`10.0.0.2/24`
  unless set), behind OpenVMM's NAT gateway, the network's first address. OpenVMM also gives
  the guest the IPv6 network that embeds this one in `fd00::/96`, such as `fd00::a00:2/120`,
  behind the gateway's counterpart, such as `fd00::a00:1`. The guest names the gateway as its
  DNS server in `/etc/resolv.conf` when the policy allows TCP or UDP port 53 to it, and
  otherwise the gateway's IPv6 address when the policy allows DNS to that one. Choose a guest
  network that contains no address the guest must reach. Ingress must be `deny`; host-loopback
  access may be allowed independently, as described next.

### Native proxy and host loopback

The proxy keeps general host-loopback access denied. Generic listener publication and explicit
forwards need host-loopback access allowed.

- `runtimeConfig.networkProxy` (`ProvisionRequest::with_network_proxy`) names an HTTP or HTTPS
  proxy that listens on host IPv4 loopback, such as `http://127.0.0.1:3128` or
  `http://localhost:3128`: an explicit port, and at most a trailing `/` after it. As in the contract's
  schema, the proxy is the guest's only way out, so the network policy must deny egress without
  allow or deny rules; provision rejects other policies, and IPv6 or remote proxies, with
  `policy_validation`. The guest's only reachable destination is TCP to its gateway at the
  proxy's port, which OpenVMM connects to the proxy; it cannot reach a DNS server, so the proxy
  resolves names. Every workload gets `HTTP_PROXY`, `HTTPS_PROXY`, `http_proxy`, and
  `https_proxy` naming the proxy at the gateway, such as `http://10.0.0.1:3128`, and `NO_PROXY`
  and `no_proxy` set to `localhost,127.0.0.1`; an HTTPS proxy's certificate must therefore be
  valid for the gateway's address.
- `network.ingress.hostLoopback: allow` lets the guest reach host-loopback services through its
  gateway, and publishes each TCP or UDP listener that the guest binds to a non-loopback address
  on the same port of host IPv4 or IPv6 loopback, without a port list. A listener bound only to
  guest `127.0.0.1` or `::1` stays private to the sandbox, OpenVMM never publishes a listener on
  a LAN or public interface or displaces a host listener that holds the port, and replies to the
  host pass whatever the egress policy. It needs a guest network of `/29` or larger: OpenVMM
  presents host peers to the guest at an address of its network, which a `/30` cannot spare.
- `hostLoopbackForwards` of the sandbox spec (`SandboxSpec::with_host_loopback_forward`, passed
  to `AciEdgeSandbox::provision_with`) publishes up to 64 further guest ports on host loopback:
  OpenVMM listens on `127.0.0.1:hostPort` and relays each TCP connection or UDP datagram to the
  guest's `guestPort`. Ports are nonzero, and a host port and a guest port each appear at most
  once per protocol, because OpenVMM keeps one forward listener per guest port.
  Forwarded ports need `network.ingress.hostLoopback: allow`. The guest answers forwarded
  traffic at addresses that OpenVMM assigns inside the guest network, so the egress default
  must be `allow`, which leaves the guest's other egress open, and no deny rule may cover the
  guest network.

Only the guest sets the proxy variables: an exec whose `process.env` names one of them, in any
case, fails with `policy_validation`, with or without a proxy.

### Native working directories and environments

`process.cwd` must be an absolute guest path without `..` of at most 4095 bytes;
`nvx::resolve_guest_path` gives the guest path of a mapped host path. A
directory that the workload cannot enter ends the command with exit code 126 and an
`NVX-EDGE-STAGE-ERROR` line on standard error. The guest's default environment holds only
`PATH` (`/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`) and the proxy
variables. `process.env` can only be layered over it, with `inheritDefaultEnv: true`, as at
most 256 entries of at most 32 KiB in total; replacing it fails with `policy_validation`. A
later entry for a name replaces an earlier one, and an entry replaces the default of the same
name.

The caller must independently approve and protect the native asset. Checking a caller-supplied
digest does not make a writable path or a self-declared digest trustworthy; use this profile
only with an immutable, externally authorized library installation.

## Policies and errors

### Host paths

Mapped host paths appear in the guest the way WSL maps Windows drives: a
Windows path `C:\work\src` appears at `/mnt/c/work/src`, and an absolute Linux
path appears at the same path. Host paths may contain commas, as the contract's
`readonlyPaths` and `readwritePaths` allow, so `C:\reports\2026, final` appears
at `/mnt/c/reports/2026, final`. Guest paths derive from the resolved host path,
so links, letter case, and Windows short names do not create second guest
paths for one object. `nvx::resolve_guest_path` performs the same
translation, for example to turn a host working directory into a
`process.cwd`; `nvx::guest_path` translates a path as written.

### Network rules

A network policy is an egress default plus IPv4 and
IPv6 allow and deny rules; deny rules take precedence, and a rule matches only
destinations of its networks' family, so a denied default blocks the other
family. Each rule lists destination networks (`to`, each a CIDR with optional
`except` sub-networks) and destination `ports` (protocol, `port`, optional
`endPort`). An empty `to` matches every destination of both families, such as
`0.0.0.0/0` and `::/0` together, and an empty `ports` matches every protocol
and port. A `tcp` or `udp` entry without a `port` matches every port of that
protocol, one with a `port` and an `endPort` matches every port from `port`
through `endPort`, an `icmp` entry matches ICMP alone, or ICMPv6 for IPv6
networks, and `any` matches every protocol or, with a port or a port range,
TCP and UDP on those ports.

### Error mapping

| Condition | `ErrorCode` | Wire code |
| --- | --- | --- |
| Invalid request shape, relative or non-UTF-8 path, zero `memoryMib`, `process.timeout` above 4,294,967,295 ms | `MalformedRequest` | `malformed_request` |
| ID without the `aci-edge-sandboxes:` prefix or a 32-digit lowercase hexadecimal token | `MalformedId` | `malformed_id` |
| The ID no longer refers to a provisioned sandbox (never provisioned, or deprovisioned) | `StaleId` | `stale_id` |
| `exec` on a stopped sandbox, or a VM that died | `NotStarted` | `not_started` |
| `start` or `deprovision` on a running sandbox | `AlreadyStarted` | `already_started` |
| `stop` on a stopped sandbox | `AlreadyStopped` | `already_stopped` |
| Unsupported policy or exec feature, oversized command, relative or oversized `process.cwd`, an image longer than the image cache's limit | `PolicyValidation` | `policy_validation` |
| A runtime dependency of the backend is missing or unusable, such as an image that is not registered or changed since its registration, or one that the image cache cannot make room for | `BackendUnavailable` | `backend_unavailable` |
| The backend does not implement the requested operation | `Unsupported` | `backend_error` (the wire model has no separate code) |
| Any other backend failure | `BackendError` | `backend_error` |

Any `nvx::Error` converts into an `Error` with the same message: `Unavailable`
becomes `backend_unavailable`, `Unsupported` becomes `policy_validation`, and
every other kind becomes `backend_error`. `Error::nvx_code` keeps the error's
[`nvx::ErrorCode`](https://github.com/microsoft/nvx/blob/6b485c82c9fdf4f592e1221efc7d6d41ab9bc66c/nvx-rs/README.md#error-codes), which tells apart the
failures that share an `ErrorCode`. Callers match these codes rather than messages, whose
wording may change. The wire body carries the `ErrorCode` alone.

## Testing

```bash
ACI_EDGE_SANDBOXES_AGENT_BUNDLE=skip cargo test --workspace --all-features   # unit, mock, and model tests; no hypervisor needed
```

`--workspace` also runs the data model's own unit tests, in `model/`. Set
`ACI_EDGE_SANDBOXES_AGENT_BUNDLE=skip` for `--all-features` builds that should not
download the edge agent bundle.

- `testing::MockBackend` scripts executions for consumers' unit tests.
- The ignored `agent_guest` tests run the agent backend against a real hypervisor; they need an
  approved agent library, the edge guest, a GPT image, and a hypervisor host.
- The agent-bundle staging tests are driven by `scripts/nvx.py test-agent-bundling` in the
  [`microsoft/nvx`](https://github.com/microsoft/nvx) repository. It stages a synthetic agent
  bundle through `ACI_EDGE_SANDBOXES_AGENT_BUNDLE_DIR`, which the bundled-agent tests must find,
  and checks that the build rejects broken copies of it; it needs neither a hypervisor nor an
  agent library that runs.

## Dependency on `nvx`

The crate is not published to a registry. It and its data model depend on the
[`nvx` crate](https://github.com/microsoft/nvx/blob/6b485c82c9fdf4f592e1221efc7d6d41ab9bc66c/nvx-rs/README.md)
0.1.0-alpha.6 from crates.io, which was published from commit `6b485c82` of the public
`microsoft/nvx` repository.

## Breaking changes

The crate now speaks sandbox ABI 3: the setup gained `platformSnapshots`, and the host info a
`platformSnapshots` field. `AgentBackend::new` refuses an agent library of ABI 2 with
`backend_unavailable`, so use a library of ABI 3. The Rust API only gains items.

The direct OpenVMM backend of earlier versions has been removed; use the agent backend. These
changes affect Rust callers and builds:

- The `openvmm` module (`OpenVmmConfig`, `OpenVmmBackend`) and `AciEdgeSandbox::openvmm` are
  gone.
- The `openvmm` and `bundled` features are gone, and the default features are now `agent`. The
  `agent` feature enables the host side of `nvx`, so `nvx::resolve_guest_path` stays available.
- The `testing` feature no longer builds the `aci-edge-sandboxes-fake-openvmm` binary.
- The build script no longer passes on `DEP_ACI_EDGE_SANDBOXES_ARTIFACTS_DIR`;
  `agent-bundled` still exports `DEP_ACI_EDGE_SANDBOXES_AGENT_ARTIFACTS_DIR`.
- Requests, results, wire formats, error codes, and `SandboxId` are unchanged.

## Limitations

Guest state does not survive a stop.
