# ACI Edge Sandbox

ACI Edge Sandbox is a Rust SDK for creating and managing ACI isolated sandboxes on edge devices
and in on-premises environments, without requiring the Azure control plane, giving you guarantees
that your application is as isolated in a container as it would be in a VM.

ACI Edge Sandbox can use traditional Hyper-V-isolated ACI backends, as well as nvx, a novel
cross-platform, OpenVMM-based micro-VM sandbox specialized for agentic workloads.

The current bundle includes only nvx as its micro-VM sandbox backend. Additional backends
will be added in the future.

## Getting started

The SDK is the `aci_edge_sandboxes` Rust crate in [`aci-edge-sandbox/`](aci-edge-sandbox/). Its
[README](aci-edge-sandbox/README.md) describes the API, backends, and configuration.

Building it needs Rust 1.89 or later and Git:

```bash
cd aci-edge-sandbox
cargo build
ACI_EDGE_SANDBOXES_AGENT_BUNDLE=skip cargo test --all-features
```

The `skip` variable keeps `--all-features` builds from downloading the edge agent bundle.

## Code of Conduct

This project has adopted the [Microsoft Open Source Code of Conduct](https://opensource.microsoft.com/codeofconduct/).
For more information see the [Code of Conduct FAQ](https://opensource.microsoft.com/codeofconduct/faq/)
or contact [opencode@microsoft.com](mailto:opencode@microsoft.com) with any additional questions or comments.

## Security

**Please do not report security vulnerabilities through public GitHub issues.**
For instructions on reporting security issues, see [SECURITY.md](SECURITY.md).

## Trademarks

This project may contain trademarks or logos for projects, products, or services. Authorized use of
Microsoft trademarks or logos is subject to and must follow
[Microsoft's Trademark & Brand Guidelines](https://www.microsoft.com/en-us/legal/intellectualproperty/trademarks/usage/general).
Use of Microsoft trademarks or logos in modified versions of this project must not cause confusion or
imply Microsoft sponsorship. Any use of third-party trademarks or logos are subject to those
third-party's policies.
