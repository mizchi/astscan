# Experiments

Run a probe with `cargo run --release -p astscan-experiments --bin <probe> -- …`.

| Probe | Arguments | What it measures |
| --- | --- | --- |
| `fastparse_probe` | `<Language> <dir>...` | Parser speed, fallbacks, and node-by-node agreement with tree-sitter |
| `glr_probe` | `<dir>...` | How often grammars need multiple parse stacks or error recovery |
| `chunk_probe` | `<repo> [commits=2000] [Language,Language...]` | Chunk reuse and storage cost across versions of source files |
| `pack_probe` | `<repo> [commits=2000]` | Compression and reconstruction cost for full versions and deltas |

[Typed lint](typed-lint/) exercises structural evidence extraction and
opt-in tsrs type queries over slim JSON matches. It also includes
[compiler diagnostics and runtime traces](typed-lint/#compiler-diagnostics-and-runtime-traces)
for TypeScript, JavaScript, Go, and Rust.

The standalone [Rust library benchmark](library-bench/) compares the
in-memory scan APIs. See [results and reproduction steps](../docs/performance.md#rust-library-benchmark).
