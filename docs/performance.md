# Performance

## Speed

Apple M5, 10 cores, wall time, mean of 5 runs after a warm-up, output to
`/dev/null`, jev-lint's 207 rules:

| Corpus | ast-grep 0.45.3 | astscan `stream` | astscan `slim` |
| --- | ---: | ---: | ---: |
| playwright (26 MB) | 3981 ms | 401 ms (9.9x) | 233 ms (17x) |
| wasmtime (34 MB) | 21556 ms | 1543 ms (14x) | 433 ms (50x) |
| Go standard library | 6007 ms | 1319 ms (4.6x) | 1172 ms (5.1x) |
| Python standard library (37 MB) | 1605 ms | 594 ms (2.7x) | 433 ms (3.7x) |
| jev-lint | 989 ms | 373 ms (2.6x) | 367 ms (2.7x) |

Peak memory on playwright is 2.6 GB for ast-grep, 349 MB for astscan `stream`
and 180 MB for `slim`.

### Release candidate spot check

The astscan 0.1.0 release candidate (`d03d1fb`) and ast-grep 0.45.3
returned the same 48,829 matches, field for field in `stream` mode, on
Go 1.27.3's `src/net`. The scan used jev-lint's 14 Go rules.
Measurement on an Apple M5 with macOS 26.6.2 used hyperfine 2.0.0,
4 threads per command, 3 warm-ups and 15 timed runs, and stdout sent
to `/dev/null`. astscan used its default parser and did not use a result cache.

| Command | Mean wall time ± standard deviation | Speedup |
| --- | ---: | ---: |
| ast-grep `stream` | 182.5 ± 6.6 ms | 1× |
| astscan `stream` | 85.9 ± 0.7 ms | 2.1× |
| astscan `slim` | 79.8 ± 1.6 ms | 2.3× |

Set `AST_GREP`, `ASTSCAN`, and `GO_RULES` to absolute paths. The Go-only YAML
used here has SHA-256 `3e8e51a8ade6c7c55922a7067d78179ab6284013c751f95cf25ae39f8cdd7ae1`.

```sh
cd "$(go env GOROOT)/src/net"
hyperfine --warmup 3 --runs 15 --output null \
  "\"$AST_GREP\" scan --rule \"$GO_RULES\" --json=stream --threads 4 -- ." \
  "\"$ASTSCAN\" scan --rule \"$GO_RULES\" --json=stream --threads 4 -- ." \
  "\"$ASTSCAN\" scan --rule \"$GO_RULES\" --json=slim --threads 4 -- ."
```

### Implementation

- **One tree walk per file, not one per rule.** The tree-sitter tree is
  flattened once into pre-order arrays (`crates/astscan-core/src/tree.rs`). Every node is one
  array index, a subtree is a contiguous index range, and parent and sibling
  are array reads. Each node is dispatched by kind to only the rules that can
  match that kind.
- **Rules that share a matcher run once.** jev-lint emits the same matcher
  under several ids (`x@JavaScript` and `x@Jsx` are one grammar). Identical
  matchers are merged and the match is reported under every id.
- **Flattening reads the tree directly.** `crates/astscan-core/csrc/flatten.c` walks tree-sitter's
  subtrees through its own inline accessors instead of a cursor, which was
  3.2x faster. It depends on private headers, so the tree-sitter version is
  pinned exactly; `ASTSCAN_CHECK_FLATTEN=1` compares it with the cursor
  implementation on every file scanned.
- **Backtracking without cloning.** The metavariable environment is three
  append-only stacks. Trying a sub-rule is a mark, and undoing it a truncate.
- **SIMD where a byte scan is the inner loop** (`crates/astscan-core/src/simd.rs`, NEON and SSE2):
  finding the next node of a kind in the kind array for `has: {kind, stopBy:
  end}`, and finding the next byte a JSON string must escape (`src/simd.rs`). Newline
  indexing, the `ast-grep-ignore` pre-check and UTF-8 validation use `memchr`
  and `simdutf8`.
- **Columns on long lines.** A column is a character count from the start of
  the line, which is quadratic on a minified file. A count sampled every 64
  bytes makes it constant.
- **Largest files first.** Files are listed, sorted by size and pulled by
  worker threads from a shared counter, so the run does not end waiting on
  one big file that started last.
- **One allocator.** tree-sitter's allocations go to mimalloc as well.

What is left is tree-sitter: in `slim` mode 73% to 85% of CPU time is inside
`ts_parser_parse`. `--stats` prints the split per run.

`ASTSCAN_LITERAL_PREFILTER` accepts `files` (default), `1` (files and
individual rule groups), or `0` (off). File checks run when each applicable
group has a provable necessary literal. `ASTSCAN_WORKER_LOCAL_REGEX=1`
gives each worker its own regex scratch space. With `--stats`, the prefilter
reports parser calls and bytes skipped, rule groups rejected, and CPU time.

### `--parser=fast` (experimental)

A compiled tree-sitter grammar is parse tables, a lex function and sometimes
an external scanner. tree-sitter's runtime drives them with reference-counted
subtrees, because it is built for re-parsing a file as it is edited. astscan
parses a file once and throws the tree away, so `crates/astscan-core/src/fastparse.rs` drives the
same tables itself: arenas instead of reference counts, no bookkeeping for
reuse, and the flat arrays as the direct output. The grammar's lex function
and scanner are still called as they are.

GLR is not optional. Measured with `experiments/src/bin/glr_probe.rs`, 83% of TypeScript
and 99% of Go and JavaScript (by bytes) is in files where the stack forks at
least once. So the fork, merge and tree-selection rules are a port of
tree-sitter's. Error recovery is not ported: a file that needs it is handed
back to tree-sitter, which costs nothing in a tree that compiles.

The tree has to be the same tree. With `ASTSCAN_CHECK_FLATTEN=1` every file is
parsed both ways and compared node for node; on the corpora above plus
moonbitlang/core (about 19,700 files, 20 languages) there is no difference,
and 1% to 9% of files per corpus are handed back.

| Corpus | `slim`, 10 threads | `slim`, 1 thread |
| --- | ---: | ---: |
| playwright | 209 -> 128 ms (1.63x) | 1184 -> 631 ms (1.88x) |
| wasmtime | 454 -> 286 ms (1.59x) | 1851 -> 931 ms (1.99x) |
| Go standard library | 1053 -> 595 ms (1.77x) | 5539 -> 2586 ms (2.14x) |
| Python standard library | 399 -> 219 ms (1.82x) | 2126 -> 1050 ms (2.02x) |
| jev-lint | 375 -> 219 ms (1.71x) | 1932 -> 1104 ms (1.75x) |

Two things mattered beyond dropping the reference counts. Most states of a
grammar are stored compressed, as groups of symbols sharing a value, and
searching them was a quarter of the parse; the driver expands the table once
per grammar (0.1 to 16 MB) and shares it between threads. And where the stack
forks, the paths of a pop share the subtrees above the fork as a linked list
instead of each copying them.

It reads `struct TSLanguage` directly, so it follows tree-sitter's grammar ABI
(14 and 15), not a public API. The default stays `tree-sitter`.

### `--cache <DIR>`

In CI most files are the same as in the last run. With `--cache`, the matches
of a file are stored under a hash of its bytes, and a file seen before is
answered from them: no parse, no matching. The path is not part of the key,
so a file that moved, or a second copy of one, is a file seen before.

| Corpus | no cache | first run with `--cache` | later runs | cache size |
| --- | ---: | ---: | ---: | ---: |
| playwright | 176 ms | 193 ms | 51 ms | 2.3 MB |
| wasmtime | 267 ms | 289 ms | 48 ms | 3.0 MB |
| Go standard library | 591 ms | 656 ms | 207 ms | 10 MB |
| Python standard library | 299 ms | 321 ms | 68 ms | 5.8 MB |
| jev-lint | 266 ms | 289 ms | 46 ms | 0.7 MB |

(`--parser=fast`, `slim`, 10 threads, the same 207 rules, every file a hit in
the later runs. Best of ten or more runs on a machine busy with other work,
so compare within the table: the same `no cache` runs took 128 to 595 ms when
it was idle. The Go run prints 543 MB, which is what its time is.)

- **What is stored is the match, not the line.** A line of output is the
  file's own text and positions wrapped in JSON, and all of it follows from
  the source plus a few numbers: which rule group matched, where, and where
  each metavariable and label sits (`src/record.rs`). That is about a dozen
  bytes per match as varints, zstd-compressed per file. The 124 MB of `slim`
  output for playwright is a 2.3 MB cache; stored as zstd-compressed JSON it
  was 7.4 MB.
- **A hit is written by the code that writes a fresh match.** Both produce
  the same record and hand it to one writer, so a later run cannot print
  something a first run would not. It also means one cache serves both
  output styles.
- **One file per rule set.** The key includes the astscan version, executable
  size, cache namespace, rule file, `sgconfig.yml`, and external grammar
  libraries. Changing one of those inputs starts a new cache.
- **Nothing in the file is trusted.** It is an append-only log, mapped
  read-only. A record cut short is dropped. A record whose checksum fails,
  that does not decompress, or that names a rule, a variable or an offset
  these rules and this file do not have, is scanned again. A run that cannot
  write the cache reports the same matches without it.
- A run that adds records also drops the ones it did not use, once those
  outweigh the rest; and starting a new cache file deletes the ones next to
  it that have not been written to for a week.
- Rules compile while the tree is walked, each grammar on a thread of its
  own, so neither a cached nor a fresh run waits long for them.
