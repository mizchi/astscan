import assert from "node:assert/strict";
import { cpSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { capture, scan } from "./scan.mjs";

const [astscan, mode, ...rest] = process.argv.slice(2);
if (!astscan || (mode && mode !== "--types") || rest.length) {
  throw new Error("Usage: node probe.mjs <astscan binary> [--types]");
}
const fixture = fileURLToPath(new URL("./fixture/", import.meta.url));
const src = readFileSync(join(fixture, "cases.ts"));
const slice = ({ start, end }) => src.subarray(start, end).toString("utf8");

const guarded = scan(astscan, fixture, "guarded-errors.yml", ["cases.ts"]).map((match) => ({
  guard: slice(capture(match, "GUARD")),
  message: slice(capture(match, "MESSAGE")),
}));
assert.deepEqual(guarded, [
  { guard: "(!email)", message: '"missing value"' },
  { guard: '(role !== "admin")', message: '"missing value"' },
]);

if (mode === "--types") {
  const { createTypeScriptAdapter } = await import("./typescript.mjs");
  const adapter = createTypeScriptAdapter(join(fixture, "tsconfig.json"));
  try {
    const empty = adapter.checkDiscardedCalls("fixture-v1", []);
    assert.deepEqual(empty, { candidates: [], programMs: 0, queryMs: 0 });

    const matches = scan(astscan, fixture, "bare-calls.yml", ["cases.ts"]);
    const result = adapter.checkDiscardedCalls("fixture-v1", matches);
    const warm = adapter.checkDiscardedCalls("fixture-v1", matches);
    assert.deepEqual(warm.candidates, result.candidates);
    assert.equal(warm.programMs, 0);
    const classified = result.candidates.map(({ range, status }) => [slice(range), status]);
    assert.deepEqual(classified, [
      ["read()", "violation"],
      ['lookup("async")', "violation"],
      ['lookup("sync")', "ok"],
      ["ambiguous()", "violation"],
      ["dataWithThenProperty()", "ok"],
      ["opaque()", "unknown"],
      ["read()", "ok"],
    ]);

    const project = mkdtempSync(join(tmpdir(), "astscan-typed-lint-"));
    const projectAdapter = createTypeScriptAdapter(join(project, "tsconfig.json"));
    try {
      cpSync(fixture, project, { recursive: true });
      assert.deepEqual(projectAdapter.checkDiscardedCalls("copy-v1", matches).candidates, result.candidates);

      const dependency = join(project, "api.ts");
      const original = readFileSync(dependency, "utf8");
      const changed = original.replace("async function fetchRecord(): Promise<number>", "function fetchRecord(): number");
      assert.notEqual(original, changed);
      writeFileSync(dependency, changed);
      const sameMatches = scan(astscan, project, "bare-calls.yml", ["cases.ts"]);
      assert.deepEqual(sameMatches, matches);
      assert.deepEqual(projectAdapter.checkDiscardedCalls("copy-v2", []), empty);

      const after = projectAdapter.checkDiscardedCalls("copy-v2", sameMatches);
      assert.deepEqual(after.candidates.map(({ status }) => status), ["ok", ...result.candidates.slice(1).map(({ status }) => status)]);
    } finally {
      projectAdapter.close();
      rmSync(project, { recursive: true, force: true });
    }
    console.log(JSON.stringify({
      guarded,
      classified,
      cold: { programMs: result.programMs, queryMs: result.queryMs },
      warm: { programMs: warm.programMs, queryMs: warm.queryMs },
    }, null, 2));
  } finally {
    adapter.close();
  }
} else {
  console.log(JSON.stringify({ guarded }, null, 2));
}
