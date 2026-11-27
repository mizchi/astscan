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
