---
name: astscan
description: Search source code by syntax tree with astscan. Use for structural code search or lint checks, inline pattern and Cypher queries, and inspecting Tree-sitter node kinds and fields.
---

# astscan

Inline queries and AST inspection require astscan 0.1.1 or later. Install
with `cargo install astscan --locked` (Rust 1.90+ and a C compiler).

## Search inline

Start with a concrete source pattern and replace the part to capture with a
metavariable such as `$ARG`. Single-quote patterns in the shell to preserve
the metavariables. Repeat `-p` and `--cypher` to run queries for one language
in a single scan.

```sh
astscan scan -l JavaScript -p 'console.log($ARG)' \
  --cypher 'MATCH (f:function_declaration) RETURN f' --fail-on-skipped -- src
```

Inline IDs use the argument order across both query kinds (`pattern-1`,
`cypher-2`). Try the query on one input that should match and one that
should be excluded before widening the scan to a directory.

## Inspect the grammar

Run `astscan debug FILE` for an S-expression with node kinds and field names.
The grammar is inferred from the file extension. Set `-l` when piping a
snippet, or use it to override the inferred grammar.

```sh
printf '%s\n' 'x.y(1)' | astscan debug -l JavaScript -
printf '%s\n' 'package demo' 'func f() { x.y(1) }' | astscan debug -l Go --json -
```

The call's `function` field contains `member_expression` in JavaScript and
`selector_expression` in Go. With `--json`, check `hasErrors` and use each
node's `kind`, `field`, and `parent` to build or diagnose a query. Pass the
same `--config` to `debug` and `scan` when using custom grammars.

## Batch structured rules

Use `--rule -` with a quoted heredoc for explicit IDs, mixed languages, or
capture constraints. Each JSON array element needs an `id`, a `language`,
and either a `rule` object or a `cypher` string.

```sh
astscan scan --rule - --json=slim --fail-on-skipped -- src <<'JSON'
[
  {"id":"string-log","language":"JavaScript","rule":{"pattern":"console.log($ARG)"},
   "constraints":{"ARG":{"kind":"string"}}},
  {"id":"go-selectors","language":"Go","cypher":"MATCH (s:selector_expression) RETURN s"}
]
JSON
```

Use `--inline-rules "$rules"` when the JSON or YAML is already a shell
variable. Use `--rule FILE` to reuse saved rules.
[Rule compatibility](https://github.com/mizchi/astscan#compatibility).

For nested AST checks, use `EXISTS` and `NOT EXISTS` over `CHILD` and
`DESCENDANT` edges. Start the outer `WHERE` with
`PATTERN("$CALLEE($VALUE)")` to emit captures. Restrict a direct edge to a
named field with `[:CHILD {field: "arguments"}]`.
[Cypher examples](https://github.com/mizchi/astscan#cypher-queries).

## Read results

- The default `--json=stream` emits one ast-grep-compatible match object per line with
  `file`, `ruleId`, `range`, and captures when present. Sort by path and byte
  range, using `ruleId` to break ties, if a consumer needs deterministic ordering.
- Prefer `--json=slim` for large scans when you can read the source. Each
  match has `ruleIds`. Recover snippets using the half-open UTF-8 byte
  ranges in `range.byteOffset`, including those on captures.
- Directory scans honor `.gitignore`. An explicit file bypasses ignore filters
  and still needs a rule for its language. Use `--stats` to investigate skips.
  With `--fail-on-skipped`, exit 2 indicates an incomplete scan or a scan error.
  Exit 1 indicates an error-severity match. Exit 0 can include matches.
- Apply edits with a separate tool. astscan accepts `fix` for compatibility
  and emits matches.

## Integrations

- Set `JEV_LINT_AST_GREP` to the astscan executable for `jev-lint check`.
- In Rust, share a compiled `RuleSet` and reuse one `Scanner` per worker.
  Copy any match data needed after the callback.
  [Library quick start](https://github.com/mizchi/astscan/tree/main/crates/astscan-core#quick-start).
- For TypeScript queries that need symbol identity or cross-file data flow,
  use the [opt-in audit runner](https://github.com/mizchi/astscan/tree/main/experiments/typed-lint#audit-with-data-flow).
