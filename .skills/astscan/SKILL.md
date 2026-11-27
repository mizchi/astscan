---
name: astscan
description: Find code by AST shape and run structural lint checks with astscan. Use for ast-grep YAML rules, named captures, multi-rule repository scans, or integration with jev-lint or the astscan-core Rust library.
---

# astscan

Run `astscan scan --rule rules.yml --json=stream --fail-on-skipped -- PATH...`.
Use an installed `astscan` or build it from an astscan checkout with
`cargo build --locked --release -p astscan` and use `target/release/astscan`.
The build requires Rust 1.90 or newer and a C compiler.

## Write and check a rule

Choose the grammar with `language`. Start with a concrete source pattern and
replace the part to capture with a named metavariable such as `$ARG`. Use `kind`
for node types and `all`, `any`, `not`, `inside`, or `has` for structural
conditions. Put multiple rule documents in one YAML file, separated by `---`.
Run the rules together so astscan can parse each input once.

```sh
work=$(mktemp -d)
cat > "$work/rules.yml" <<'YAML'
id: log-call
language: JavaScript
severity: warning
rule:
  pattern: console.log($ARG)
YAML
printf '%s\n' 'console.log("hello");' 'console.info("hello");' > "$work/example.js"
astscan scan --rule "$work/rules.yml" --json=stream --fail-on-skipped -- "$work/example.js"
```

Expect one match with `ruleId` equal to `log-call` and
`metaVariables.single.ARG.text` equal to `"hello"` including the quotes.
Check a representative example that should match and one that should be excluded
before scanning a directory. See the [rule compatibility list](https://github.com/mizchi/astscan#compatibility)
when porting rules. Apply code changes with a separate tool. astscan extracts
matches, accepts `fix` without applying it, and rejects `transform` and `rewriters`.

For nested AST existence checks, use a `cypher:` document with `MATCH`,
`RETURN`, and `EXISTS` or `NOT EXISTS` over `CHILD` and `DESCENDANT` edges.
Use `PATTERN("$CALLEE($VALUE)")` at the start of the outer `WHERE` to emit
captures and `[:CHILD {field: "arguments"}]` for a named direct edge.
See the [Cypher example](https://github.com/mizchi/astscan#cypher-queries).
For TypeScript audits that need symbol identity or data flow, see the
[opt-in audit runner](https://github.com/mizchi/astscan/tree/main/experiments/typed-lint#audit-with-data-flow).

## Read results and compose scans

- `--json=stream` emits one ast-grep-compatible match object per line with
  `file`, `ruleId`, `range`, and captures when present. Sort by path and byte
  range, using `ruleId` to break ties, if a consumer needs deterministic ordering.
- `--json=slim` emits byte ranges and `ruleIds` for consumers that already
  have the source. Use UTF-8 byte offsets to recover source text.
- Directory scans honor `.gitignore`. An explicit file bypasses ignore filters
  and still needs a rule for its language. Use `--stats` to investigate skips.
  With `--fail-on-skipped`, exit 2 indicates an incomplete scan or a scan error.
  Exit 1 indicates an error-severity match. Warnings can be present with exit 0.
- In jev-lint, point `JEV_LINT_AST_GREP` to the astscan executable and run
  `jev-lint check`. In Rust, compile YAML once with `RuleSet::compile`, share
  the rules across workers, and reuse one `Scanner` per worker. Copy any match
  data needed after the callback returns. See the [library quick start](https://github.com/mizchi/astscan/tree/main/crates/astscan-core#quick-start).
