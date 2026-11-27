# Structural and type-aware lint experiments

Run the structural probe from the repository root with Node 24.

```sh
cargo build --release -p astscan
node experiments/typed-lint/probe.mjs target/release/astscan
```

The advanced probe uses the
[tsrs Node API](https://github.com/oxc-project/tsrs/blob/main/docs/NODE_API.md).
Install the version pinned for this experiment and run with `--types`.

```sh
pnpm --dir experiments/typed-lint install --frozen-lockfile
node experiments/typed-lint/probe.mjs target/release/astscan --types
```

The probe exercises two uses of the same scanner.

- [Guarded error evidence](guarded-errors.yml) captures an error message and
  its enclosing `if` consequence guard. Two throws with the same message can
  carry different evidence to a jev-lint rule. An `else` requires the false
  branch of the predicate. A throw after `if (ok) return` requires control-flow
  evidence. The fixture distinguishes both shapes from a consequence guard.
- [Discarded awaitable](bare-calls.yml) finds direct call expression statements.
  [The type adapter](typescript.mjs) checks each call's inferred type and
  reports a violation if any possible return type is awaitable. The policy
  uses explicit `void` for deliberate discards. Awaited calls, assigned results,
  and direct returns fall outside the candidate shape.

The probe checks import aliases, a shadowed name, overload resolution, return
unions, a non-callable `then` property, an unknown type, and UTF-8 byte offsets
after text containing an emoji. It changes only the imported module in a
temporary project and asserts that the same structural candidates receive a
different type verdict.

## Advanced type adapter

The caller explicitly creates one adapter per project. Its input is a
project revision and a batch of slim JSON matches. The first nonempty batch
starts the tsrs process and opens a project snapshot. Later batches for that
revision reuse the snapshot and the facts already resolved for each type.
The adapter batches source-file lookups and type queries. An empty batch
returns immediately. A revision change releases the old snapshot and opens
a fresh one on demand in the existing process.

```js
const adapter = createTypeScriptAdapter(tsconfig);
try {
  const result = adapter.checkDiscardedCalls(projectRevision, matches);
  console.log(result.candidates);
} finally {
  adapter.close();
}
```

The host supplies the revision for the source snapshot scanned by astscan.
Advance it on changes to source files, dependencies, compiler version, or
project options. A revision can be an opaque generation counter encoded as a
string. The adapter compares source ranges across UTF-8 and UTF-16 and
returns `unknown` when a candidate cannot be resolved. Timings report
snapshot initialization and fact queries separately.

## Audit with data flow

The opt-in audit runner takes two inputs. A [Cypher rule file](audit/fixture/candidates.yml)
selects entry functions and captures call arguments with astscan. An
[audit policy](audit/fixture/policy.json) selects the modeled declarations by
file and name. tsrs resolves the call symbols, including import aliases, and
checks optional argument types for findings. The example audits whether
unmasked input can reach a logging API. It runs static analysis only.

```sh
pnpm --dir experiments/typed-lint install --frozen-lockfile
cargo build --locked --release -p astscan
fixture=experiments/typed-lint/audit/fixture
node experiments/typed-lint/audit/cli.mjs target/release/astscan \
  "$fixture/tsconfig.json" "$fixture/candidates.yml" "$fixture/policy.json"
```

The CLI scans from the `tsconfig.json` directory and accepts optional
project-relative inputs. It emits a diagnostic batch as JSON. Exit 0 means
complete coverage with no matches, exit 1 means a match with complete coverage,
and exit 2 means some coverage is partial. The fixture demonstrates both
findings and partial coverage.

| Model `kind` | Contract |
| --- | --- |
| `source` | Treat the result of the modeled call as external input. |
| `sink` | Report flow to the zero-based `argument`. Optional `capture` binds it to an astscan `{rule, name}` capture. Optional `type` requires `string`, `number`, or `boolean`. |
| `sanitize-return` | Clear the flow on the returned value. |
| `guard` | Clear the flow on the local binding passed as `argument` on the branch where the resolved boolean result is true. Await an async guard to test its result. |
| `sanitize-field` | Clear the named `field` of the argument object. |
| `propagate-return` | Propagate flow from `argument` to the return value. |
| `propagate-field` | Write flow from `argument` to the named `field` on the object passed at `target`. |

`entryRule` names the rule selecting functions. Optional `entryFiles` filters
those matches by path relative to `tsconfig.json`. `entryParameters` identifies
zero-based parameter positions to treat as input. Remaining parameter values
have no configured source at entry. Every model supplies a declaration `file`
and `name`.

The evaluator follows assignments, shadowing, `if`, early returns, string
concatenation, and named fields of simple objects. For `for`, `while`, and
`do...while`, it joins incoming flows with successive iterations until the
facts stabilize. Unlabelled `break` and `continue` target the innermost loop.
It applies function summaries to resolved calls, including direct `await`
calls to async functions in other files. A summary carries flows to the return
value, observed sink arguments, and direct writes to argument fields. A call
with no normal return path stops analysis of the calling path.

Dynamic calls, indirect awaits, unawaited async functions, `for...of`,
`for...in`, repeated allocation in a loop, and exceeded limits can leave
coverage partial. `candidates[].issues` records the reason and location.

`query` defaults to `EXISTS`. For `NOT EXISTS`, a candidate matches only
when the relevant scan and flow evaluation are complete and no qualifying path
exists. `exists` and `matched` are `null` when the answer is unknown. A
positive diagnostic points at the sink argument. Its `relatedInformation`
and `evidence` carry the source and one witnessed flow path in order.

Embedders can call `createTypeScriptAudit(tsconfig).check(revision, matches,
policy, { scanComplete, steps, calls, nesting, loops })` with astscan slim JSON
matches. `loops` bounds iterations per loop and defaults to 32.
Supply `scanComplete: true` after verifying the scan scope, change `revision`
when snapshot inputs change, and call `close()` when done. An empty entry
batch skips compiler initialization.

## Compiler diagnostics and runtime traces

The [diagnostic contract](diagnostics/contract.mjs) is an opt-in Node API for
consumers of astscan. It uses the LSP fields `range`, `severity`, `code`,
`codeDescription`, `source`, `message`, `tags`, `relatedInformation`, and
`data`. `uri` identifies the document and `range` uses zero-based UTF-16
positions. A diagnostic may omit these fields when a tool supplies less
information. Compiler-specific codes stay scoped to their source.
`locations` adds UTF-8 offsets for matching AST nodes. `evidence` also
preserves nested explanations, macro expansions, and suggestions. Runtime
traces keep each stack's original frame order.

| Input | Adapter |
| --- | --- |
| TypeScript documents | [tsrs LSP](diagnostics/typescript.mjs) pulls diagnostics for the requested documents |
| TypeScript project | [tsrs compiler API](diagnostics/typescript.mjs) checks the configured project and keeps message chains and related information |
| Go module | [Go](diagnostics/go.mjs) uses `go/packages`, `go/scanner`, and `go/types` for package, syntax, and type diagnostics |
| Rust project | [Rust](diagnostics/rust.mjs) reads Cargo's `compiler-message` events and resolves relative spans against the workspace root |
| JavaScript error | [JavaScript](diagnostics/javascript.mjs) prefers V8 CallSites and resolves source maps with `@jridgewell/trace-mapping` |
| Go or Rust panic | [Go recovery](diagnostics/go-native/panictrace/capture.go) and [Rust panic hook](diagnostics/rust-panic/src/lib.rs) capture frames in the failing process |

All diagnostic connectors expose `check(revision, scope)` and `close()`.
Use `await` to consume either a synchronous compiler adapter or an asynchronous
LSP connector. The revision identifies the source snapshot, dependencies,
and configuration. Compiler adapters check their configured project scope
and cache the current revision. Treat batches as read-only. Compiler errors
appear in the batch. Tool startup failures and failed builds without compiler
diagnostics throw.

Go syntax and type errors use physical file offsets even when a `//line`
directive changes the displayed location. The package loader exposes some
locations as a dedicated position string, and `go/types` marks secondary
messages with a leading tab. To request full compiler and linker diagnostics,
pass `mode: "build"` to `createGoAdapter`. In that mode `go build -json`
provides package events and the adapter reads the diagnostic text within
those events.

```js
import { diagnosticsInRanges } from "./diagnostics/contract.mjs";
import { createTypeScriptDiagnostics } from "./diagnostics/typescript.mjs";

const compiler = createTypeScriptDiagnostics("/project/tsconfig.json");
try {
  const { diagnostics } = compiler.check("revision-1");
  const ranges = slimMatches.map(({ file, range }) => (
    { file, ...range.byteOffset }
  ));
  const findings = diagnosticsInRanges(diagnostics, ranges, { cwd: "/project" });
  console.log(findings);
} finally {
  compiler.close();
}
```

When present, `location.bytes` is a half-open UTF-8 range, with equal
endpoints for a point. Text positions are 1-based and use the originating
tool's column units. Go compile diagnostics usually provide a point.
Go and Rust panic traces retain the line and any column supplied by the
runtime. `diagnosticsInRanges` selects by primary byte locations and preserves
the full evidence for each selected diagnostic. Use inputs from the same
source revision when joining with AST matches.

### LSP document diagnostics and server-specific collection

```js
import { pathToFileURL } from "node:url";
import { createTypeScriptLspConnector } from "./diagnostics/typescript.mjs";

const connector = createTypeScriptLspConnector({ cwd: "/project" });
try {
  const batch = await connector.check("revision-1", {
    documents: [{ uri: pathToFileURL("/project/main.ts").href, languageId: "typescript" }],
  });
  console.log(batch.coverage, batch.diagnostics);
} finally {
  connector.close();
}
```

Each document can supply `text` to check an in-memory snapshot. Its UTF-8
byte spans are computed from that text. Document coverage distinguishes
`complete`, `provisional`, and `unreported`. An empty array can accompany
any of those states. The default collector opens the requested documents,
uses `textDocument/diagnostic` when advertised, and collects
`textDocument/publishDiagnostics` while waiting. Both push snapshots and
unconfirmed pull reports are provisional. The tsrs profile confirms full
pull reports for the requested documents. Servers such as rust-analyzer can
send an empty full report while the project is still loading.

Use `createLspConnector` and `connectStdio` from
[`lsp.mjs`](diagnostics/lsp.mjs) to add another server. Supply
`clientCapabilities` and `initializationOptions` for its handshake,
`configure(rpc)` to register server requests or readiness notifications
before initialization, and `collect(context)` to run server-specific
requests and report document states. `configure` can also return a collector
with state local to that connection. Standard LSP positions are negotiated
as UTF-16. Collection deadlines return the results received so far and
partial coverage. For a project-wide check, pass `{ project: true }` and
provide a collector that calls `completeProject()` after the server's own
completion signal. The compiler adapters remain available for project
checks when a language server's coverage is partial.

`readJavaScriptError(error, { cwd, frames, loadMap, remap })` accepts
caller-supplied CallSites or frame objects. On Node it also reads V8 CallSites
when a live error's stack is first accessed, preserving the application's
stack formatter. Already formatted stacks use `error-stack-parser`.
Source maps come from the supplied loader, the Node source map registry,
or inline and local external maps linked by `sourceMappingURL`. A caller can
supply a map for hosted assets by returning `{ map, url }` from
`loadMap(generatedFile)`. Maps are resolved once per generated file during
each read. A mapped frame carries both its original `location` and its
`generated` location. Missing or invalid maps leave the frame at its
generated location. Set `remap: false` for stacks already mapped by the
runtime.

For a live Go panic, call `panictrace.Capture(value)` inside the deferred
function that recovered it and encode the returned report as JSON. This
captures the current goroutine through `runtime.CallersFrames`. For Rust, call
`astscan_panic_trace::capture(info)` in a panic hook and serialize the report
with Serde. The panic hook supplies the primary location and the backtrace
library supplies the frames. Symbol availability depends on the build.
Pass the decoded report to `readGoPanic(report, { cwd })` or
`readRustPanic(report, { cwd })`. Both also accept existing stderr logs.
Use the Cargo workspace root as `cwd` for workspace-relative Rust paths.

Run the integration tests with Go 1.26 or later, Rust 1.88 or later for the
panic collector, and Node 24. Use the repository's Rust toolchain to build
astscan.
`ASTSCAN` enables the test that joins actual slim JSON matches to tsrs
diagnostics.

```sh
pnpm --dir experiments/typed-lint install --frozen-lockfile
cargo build --release -p astscan
ASTSCAN=target/release/astscan node --test experiments/typed-lint/*.test.mjs
```
