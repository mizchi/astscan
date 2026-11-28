# astscan-core

Fast structural code search over in-memory source with ast-grep rule YAML.
Compile rules once, share them across workers, and reuse one scanner per
worker. The scanner flattens each syntax tree once and dispatches candidate
rules by node kind. Identical matchers run once even when several rule IDs
share them.

## Installation

Use Rust 1.90 or later and a C compiler.

```sh
cargo add astscan-core
```

## Quick Start

```rust
use astscan_core::{Registry, RuleSet, Scanner};

let yaml = r#"
id: log-call
language: JavaScript
rule:
  pattern: console.log($ARG)
"#;
let rules = RuleSet::compile(yaml, &mut Registry::default()).unwrap();
let mut scanner = Scanner::new(&rules);
let source = "console.log('hello');";
let mut matches = 0;

scanner.scan("js", source, |found| {
    matches += 1;
    assert_eq!(found.rules().next().unwrap().id, "log-call");
    for (name, bytes) in found.single_captures() {
        assert_eq!(name, "ARG");
        assert_eq!(&source[bytes.start as usize..bytes.end as usize], "'hello'");
    }
}).unwrap();
assert_eq!(matches, 1);
```

Results borrow the scanner's buffers for the duration of the callback.
Matches, captures, and secondary labels use UTF-8 byte ranges into the
original source, including for scripts and styles embedded in HTML.
`Match::rules` yields the rule metadata after applying `ast-grep-ignore`.
Compilation warnings are available via `RuleSet::warnings`.

`Scanner::with_optimizations` accepts `ScanOptimizations` to select source
literal checks and worker-local regex scratch space independently.
The default `LiteralPrefilter::Files` skips parsing when all rule groups can
be ruled out. `LiteralPrefilter::FilesAndGroups` also bypasses individual rule
groups in files that still need parsing.

The default parser uses tree-sitter. `Scanner::with_parser` can opt into
`ParserKind::Fast`, which drives the same grammars and falls back to
tree-sitter when it needs error recovery. Building the library requires a C
compiler for the grammars and the tree-flattening routine.

See the [supported rule syntax](https://github.com/mizchi/astscan#compatibility)
and [Rust crate benchmarks](https://github.com/mizchi/astscan/blob/main/docs/performance.md#rust-library-benchmark).
Licensed under MIT.
