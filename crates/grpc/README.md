# frontend-grpc

Konveyor gRPC provider interface for the frontend-analyzer-provider. Implements the `ProviderService` and `ProviderCodeLocationService` protocols defined by the Konveyor analyzer-lsp/kantra ecosystem.

## Overview

This crate is the network boundary layer. It exposes the frontend scanners as a gRPC server that kantra can connect to for rule evaluation. The server receives analysis requests over gRPC, dispatches them to the JS and CSS scanners, and returns structured incident results.

## Services

### ProviderService

| RPC | Description |
|---|---|
| `Capabilities` | Returns supported capabilities: `referenced`, `cssclass`, `cssvar`, `dependency` |
| `Init` | Initializes the provider with a project root path |
| `Evaluate` | Evaluates a rule condition against the project, returns matching incidents |
| `Stop` | Graceful shutdown |
| `GetDependencies` | Returns project dependencies from package.json |

### ProviderCodeLocationService

| RPC | Description |
|---|---|
| `GetCodeSnip` | Returns a source code snippet around a given file position with configurable context lines |

## Usage

```rust
use frontend_grpc::server::serve_tcp;
use frontend_grpc::service::FrontendProvider;
use std::sync::Arc;

let provider = Arc::new(FrontendProvider::new(2)); // 2 context lines
serve_tcp(provider, 9001).await?;
```

The server also supports Unix domain sockets on Linux/macOS:

```rust
use frontend_grpc::server::serve_unix;

serve_unix(provider, "/tmp/frontend-provider.sock").await?;
```

## Architecture

```
kantra / analyzer-lsp
       |
       | gRPC (protobuf)
       v
  FrontendProvider
       |
       +-- Init: stores project root
       |
       +-- Evaluate:
       |     |
       |     +-- evaluate_condition()
       |           |
       |           +-- frontend-js-scanner (referenced, cssclass in JS, cssvar in JS)
       |           +-- frontend-css-scanner (cssclass in CSS, cssvar in CSS)
       |           +-- dependency check (package.json)
       |
       +-- GetCodeSnip: reads file, extracts lines around position
```

## Proto generation

The protobuf definitions live in `proto/provider.proto`. Generated code is checked in at `src/generated/provider.rs`. To regenerate:

```bash
cargo build --features generate-proto
```

This requires `protoc` (downloaded automatically via `dlprotoc`).

## License

Apache-2.0
