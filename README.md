# astscan

**AST search for AI agents.** Run inline source patterns or structural Cypher
against a repository, inspect the grammar when writing a query, and get
matches as JSON Lines with captures and UTF-8 byte ranges.

[Quick Start](#quick-start) · [Examples](#examples) · [Agent skill](https://github.com/mizchi/astscan/tree/main/.skills/astscan) · [Benchmarks](#benchmarks)

## Installation

Use Rust 1.90 or later and a C compiler.

```sh
cargo install astscan --version 0.1.1 --locked
```

To install from a checkout, run `cargo install --path . --locked` from the
repository root.

## Quick Start

```sh
demo="$(mktemp -d)"
cat > "$demo/main.js" <<'JS'
function run() {
  for (let i = 0; i < 1; i++) console.log("hello");
}
JS
astscan scan -l JavaScript -p 'console.log($ARG)' -- "$demo"
```

The command returns one match with `ruleId` set to `pattern-1` and `ARG`
capturing `"hello"`. The examples reuse this demo directory.

## Examples

### Run a query batch from the shell

Repeat `--pattern` and `--cypher` to run several inline queries in one scan.
This example returns `pattern-1` and `cypher-2`.

```sh
astscan scan -l JavaScript \
  -p 'console.log($ARG)' \
  --cypher 'MATCH (f:function_declaration) RETURN f' -- "$demo"
```

Pass a JSON array to `--rule -` to run a generated batch with explicit IDs,
per-rule languages, and options such as constraints.

```sh
astscan scan --rule - --json=slim --fail-on-skipped -- "$demo" <<'JSON'
[
  {
    "id": "string-log",
    "language": "JavaScript",
    "rule": { "pattern": "console.log($ARG)" },
    "constraints": { "ARG": { "kind": "string" } }
  },
  {
    "id": "functions",
    "language": "JavaScript",
    "cypher": "MATCH (f:function_declaration) RETURN f"
  }
]
JSON
```

One scan returns byte ranges and `ruleIds` as JSON Lines. Use
`--inline-rules "$rules"` to pass a JSON array or YAML as an argument, or
`--rule rules.yml` to load a saved rule file.

### Inspect an AST

Use `astscan debug` to check node kinds and field names before writing a
pattern or Cypher query. It infers the grammar from a file extension.
Pass `--lang` to override it or to inspect source piped on stdin.

```sh
astscan debug "$demo/main.js"
printf '%s\n' 'x.y(1)' | astscan debug -l JavaScript -
printf '%s\n' 'package demo' 'func f() { x.y(1) }' | astscan debug -l Go --json -
```

The JavaScript call's `function` field contains a `member_expression`.
The Go call's `function` field contains a `selector_expression`. The default
output is an S-expression. With `--json`, the output includes the language,
syntax-error status, and all nodes in preorder. Each node has its parent
ID, field, kind, `named` / `extra` / `missing` flags, and UTF-8 byte offsets
with an exclusive end.
Use `--config` to load the same custom grammars as `scan`.

### Cypher queries

Use an astscan `cypher` rule to check for nested AST relationships. This
JavaScript rule reports each function with a `for` loop containing no `await`.

```sh
astscan scan -l JavaScript --cypher '
  MATCH (f:function_declaration)
  WHERE EXISTS {
    MATCH (f)-[:DESCENDANT]->(loop:for_statement)
    WHERE NOT EXISTS { MATCH (loop)-[:DESCENDANT]->(:await_expression) }
  }
  RETURN f
' -- "$demo"
```

Set the `cypher` field when adding this query to a JSON or YAML batch.
`RuleSet::compile` accepts either batch format in Rust.

The subset supports `MATCH (name:kind)`, `RETURN name`, and nested `EXISTS`
and `NOT EXISTS` predicates joined by `AND`. The `CHILD` and `DESCENDANT`
edges follow the syntax tree from the enclosing variable. `DESCENDANT`
includes nested scopes and excludes the enclosing node itself. A match
reports the outer node once, regardless of the number of inner matches.
Each document uses either `cypher` or `rule`. Cross-file joins, symbol and
type predicates, and Cypher aggregation are outside the core's structural subset.

Start the outer `WHERE` with `PATTERN("$CALLEE($VALUE)")` to reuse
ast-grep pattern syntax and emit captures. Use the document's `constraints`
to filter those captures. `[:CHILD {field: "arguments"}]` restricts a direct
edge to a named Tree-sitter field.

The [TypeScript audit experiment](https://github.com/mizchi/astscan/tree/main/experiments/typed-lint#audit-with-data-flow)
uses those captures for symbol-aware, cross-file data flow and diagnostics.

## Output

`--json=stream` is one ast-grep match object per line.

`--json=slim` is for a reader that already has the files. It leaves out
source excerpts (`text`, `lines`, `labels`) and reports a match once with
every rule ID that matched it. The function match from the batch above
looks like this, with the temporary path shortened.

```json
{
  "file": "main.js",
  "language": "JavaScript",
  "range": {
    "byteOffset": { "start": 0, "end": 70 },
    "start": { "line": 0, "column": 0 },
    "end": { "line": 2, "column": 1 }
  },
  "ruleIds": ["functions"]
}
```

The difference is large when rules match big nodes. On playwright, stream
output is 3.4 GB and slim output is 130 MB for the same matches.

Directory scans honor `.gitignore`, including in source trees without Git
metadata. Naming a file explicitly bypasses ignore filters. Its language
still needs an applicable rule.

Unreadable inputs, invalid UTF-8, and parser failures are reported on
stderr. The CLI also skips files that exceed both 3,000,000 bytes and
200,000 lines. With `--fail-on-skipped`, an incomplete scan exits 2.
Otherwise, exit 1 means an error-severity rule matched and exit 0 means no
error-severity match was found. Use `--stats` to see how many inputs were
skipped. Ignore rules and language selection define the scan scope.

## Benchmarks

On an Apple M5, the CLI was **2.6–14× faster than ast-grep 0.45.3** in
`--json=stream` across five corpora with jev-lint's 207 rules. On Wasmtime,
wall time dropped from **21.6 s to 1.54 s**. The compact `--json=slim`
output took **0.43 s**, roughly **50×** faster than the ast-grep baseline.
An astscan 0.1.0 spot check on Go's `net` package measured **2.1×** with
identical stream matches. [CLI benchmark details](https://github.com/mizchi/astscan/blob/main/docs/performance.md).

Rust APIs were measured on 436 files from Go 1.27.3's `src/net` on one
thread. Values are median process CPU times. All engines returned the same
rule IDs and byte ranges.

| Rust API | 1 rule | 16 rules |
| --- | ---: | ---: |
| `astscan-core` 0.1.0 | 297 ms | 386 ms |
| `astscan-core` 0.1.0, prefilter off | 398 ms | 378 ms |
| ast-grep 0.51.3 `CombinedScan` | 589 ms | 683 ms |
| Tree-sitter 0.27.0 `QueryCursor` | 499 ms | 516 ms |

[Rust benchmark conditions, variability, and reproduction command](https://github.com/mizchi/astscan/blob/main/docs/performance.md#rust-library-benchmark).

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
`customLanguages` from an `sgconfig.yml` passed with `-c`.

Not supported, and rejected with an error rather than ignored: `transform`,
`rewriters`, `files` / `ignores`, `range`, selector syntax in `kind`,
parameterized `matches`, and global utility rules. `fix` and `labels` are
accepted and ignored. Metavariables in `message` are not expanded.

## Languages

astscan bundles 33 grammars. The 28 shared with ast-grep 0.45.3 use the
grammar versions from that release. The five additions are available
directly in rule YAML and are selected by extension during directory scans.

| Rule `language` | Extensions |
| --- | --- |
| `MoonBit` | `.mbt` |
| `Zig` | `.zig` |
| `Toml` | `.toml` |
| `Sql` | `.sql` |
| `Xml` | `.xml`, `.svg`, `.xsd`, `.xsl` |

Bundled language names are case-insensitive. An exact declared
`customLanguages` name takes precedence over a bundled name, so an existing
MoonBit grammar declaration continues to work. SQL uses the
[tree-sitter SQL grammar](https://github.com/DerekStride/tree-sitter-sql).
For a statement pattern such as `SELECT id FROM $TABLE`, omit the terminal
semicolon so the pattern selects one AST node.

## Library and experiments

- `astscan` is the CLI for parallel directory scans, JSON output, and an
  optional persistent cache.
- `astscan-core` compiles rules once and returns structured matches over
  in-memory source. Install it with `cargo add astscan-core`.
  [Rust quick start](https://github.com/mizchi/astscan/tree/main/crates/astscan-core#quick-start) · [Rust API](https://docs.rs/astscan-core).
- [Experiments](https://github.com/mizchi/astscan/tree/main/experiments)
  covers AST transport, Wasm plugins, lint and CI applications, and probes.

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
