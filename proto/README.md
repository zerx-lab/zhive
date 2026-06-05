# proto/

JSON-RPC 2.0 wire schema for zhive. This directory is the single source of
truth referenced by [research/99-decisions D-003](../research/99-decisions/README.md).

```
proto/
├── schema/                 # JSON Schema (one file per public wire type)
└── zhive/
    └── v1/
        └── zhive.proto      # legacy ConnectRPC artefact (superseded by D-003)
```

The authoritative schema is defined in Rust inside `crates/zhive-proto` as
`serde` types that derive `schemars::JsonSchema`; the `proto/schema/*.json`
files are emitted from those types. The `.proto` file predates the
JSON-RPC switch and is no longer the source of truth.

## Generation

Wire types live in one crate:

| Crate | What it ships |
|---|---|
| `crates/zhive-proto` | `serde`/`schemars` message types + LSP-style `Content-Length` framing |

To regenerate the JSON Schema files under `proto/schema/`:

```bash
cargo xtask schema
```

The generated `.json` files are committed to the source tree (per
[M-OOBE](https://github.com/microsoft/rust-guidelines)) so that downstream
consumers can read the schema without building the workspace.

## Versioning

A new major schema revision (`v2`, `v3`, ...) creates a sibling directory
under `proto/zhive/`. Old versions stay buildable until the next LTS cut so
in-flight clients can finish upgrading.
