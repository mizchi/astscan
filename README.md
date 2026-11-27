# astscan

Fast multi-rule structural code search using [ast-grep](https://github.com/ast-grep/ast-grep)
rules. It implements the extraction half of `ast-grep scan`.

- [`astscan-core`](https://github.com/mizchi/astscan/tree/main/crates/astscan-core) is the Rust library. It compiles
  rule YAML once and returns structured matches from in-memory source.
- `astscan` is the CLI. It finds source files, scans them in parallel,
  and handles JSON output and the optional persistent cache.
- [`experiments/`](https://github.com/mizchi/astscan/tree/main/experiments) contains parser, storage, and diagnostic prototypes.

## Installation

You need Rust 1.90 or later and a C compiler. Install the released CLI
from crates.io.

```sh
cargo install astscan --locked
```

To install from source, run this from the root of a repository checkout.

```sh
cargo install --path . --locked
```

For Rust applications, use `cargo add astscan-core` and follow the
[library quick start](https://github.com/mizchi/astscan/tree/main/crates/astscan-core#quick-start).

## Quick Start

```sh
mkdir -p src
cat > rules.yml <<'YAML'
id: log-call
language: JavaScript
severity: warning
rule:
  pattern: console.log($ARG)
YAML
printf 'console.log("hello");\n' > src/main.js
astscan scan --rule rules.yml --json=stream -- src
```

The command prints one JSON line for `src/main.js`. Its `ruleId` is
`log-call`, and the `ARG` capture contains `"hello"`.

Directory scans honor `.gitignore`, including in source trees without Git
metadata. Naming a file explicitly bypasses ignore filters. Its language
still needs an applicable rule.

Unreadable inputs, invalid UTF-8, and parser failures are reported on
stderr. The CLI also skips files that exceed both 3,000,000 bytes and
200,000 lines. With `--fail-on-skipped`, an incomplete scan exits 2.
Otherwise, exit 1 means an error-severity rule matched and exit 0 means no
error-severity match was found. Use `--stats` to see how many inputs were
skipped. Ignore rules and language selection define the scan scope.

See [compatibility](#compatibility), [output formats](#output), and the
[library API](https://docs.rs/astscan-core) for embedders. The
[performance guide](https://github.com/mizchi/astscan/blob/main/docs/performance.md)
covers `--cache`, the experimental parser, and benchmark conditions.

## Compatibility

The command line is the part of `ast-grep scan` that jev-lint uses, so it can
stand in for the binary:

```sh
JEV_LINT_AST_GREP=/path/to/astscan jev-lint check
```

`--json=stream` output is ast-grep 0.45.3's, field for field. That is checked
by `tools/diff.mjs`, which runs both tools with one rule file and compares
every match. With jev-lint's shipped rules (207 rule documents, 26 languages)
the two agree on all of these:

| Corpus | Matches |
| --- | ---: |
| microsoft/playwright | 281,918 |
| Go standard library (`$GOROOT/src`) | 925,030 |
| Python 3.14 standard library | 516,997 |
| bytecodealliance/wasmtime | 283,288 |
| google/flatbuffers | 68,578 |
| moonbitlang/core (grammar loaded from a dylib) | 60,797 |

jev-lint's own test suite (704 tests) passes with astscan substituted.

Supported: `pattern` (string, or `context` / `selector` / `strictness`),
`kind`, `regex`, `nthChild`, `inside` / `has` / `precedes` / `follows` with
`stopBy` and `field`, `all` / `any` / `not` / `matches`, `constraints`, local
`utils`, `ast-grep-ignore` comments, scripts and styles embedded in HTML, and
`customLanguages` from an `sgconfig.yml` passed with `-c`. The built-in
grammars are ast-grep's 28, pinned to the versions ast-grep 0.45.3 was built
with.

Not supported, and rejected with an error rather than ignored: `transform`,
`rewriters`, `files` / `ignores`, `range`, selector syntax in `kind`,
parameterized `matches`, and global utility rules. `fix` and `labels` are
accepted and ignored. Metavariables in `message` are not expanded.

## Output

`--json=stream` is one ast-grep match object per line.

`--json=slim` is for a reader that already has the files. It leaves out
everything that is a copy of the source (`text`, `lines`, `labels`) and
reports a match once with every rule id that matched it:

```json
{"file":"src/a.ts","language":"TypeScript","range":{"byteOffset":{"start":16,"end":26},"start":{"line":1,"column":5},"end":{"line":1,"column":15}},"metaVariables":{"single":{"X":{"range":{...}}},"multi":{},"transformed":{}},"ruleIds":["call@TypeScript"]}
```

The difference is large when rules match big nodes. On playwright, stream
output is 3.4 GB and slim output is 130 MB for the same matches.

## License

MIT. See [LICENSE](LICENSE) for the ast-grep attribution.

## Contributing

See the [contribution guide](https://github.com/mizchi/astscan/blob/main/CONTRIBUTING.md)
for development checks and release steps.

## Reference projects

- [ast-grep](https://github.com/ast-grep/ast-grep) defines the rule syntax and matching semantics.
- [Tree-sitter](https://github.com/tree-sitter/tree-sitter) provides the parsers.
- [ripgrep](https://github.com/BurntSushi/ripgrep) provides the `ignore` crate used for directory traversal.
- [jev-lint](https://github.com/mizchi/jev-lint) supplies a multi-rule lint workload for the [benchmarks](https://github.com/mizchi/astscan/blob/main/docs/performance.md).
