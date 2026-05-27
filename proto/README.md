# proto/

ConnectRPC schema for zhive. This directory is the single source of truth
referenced by [research/99-decisions D-003](../research/99-decisions/README.md).

```
proto/
└── zhive/
    └── v1/
        └── zhive.proto      # session / thread / turn / approval services
```

## Generation

Rust bindings are produced inside two crates:

| Crate | What it ships |
|---|---|
| `crates/zhive-proto`   | `prost`-generated message types |
| `crates/zhive-service` | `connectrpc-build`-generated service traits and clients |

To regenerate (Phase 1 wiring lands later):

```bash
cargo xtask gen-proto
```

The generated `.rs` files are committed to the source tree (per
[M-OOBE](https://github.com/microsoft/rust-guidelines)) so that downstream
consumers do not need `protoc` to build the workspace.

## Versioning

A new major schema revision (`v2`, `v3`, ...) creates a sibling directory
under `proto/zhive/`. Old versions stay buildable until the next LTS cut so
in-flight clients can finish upgrading.
