import assert from "node:assert/strict";
import { cpSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { createTypeScriptAudit } from "./audit/index.mjs";
import { scan } from "./scan.mjs";

const fixture = fileURLToPath(new URL("./audit/fixture/", import.meta.url));
const astscan = resolve(process.env.ASTSCAN ?? "target/release/astscan");
const policy = JSON.parse(readFileSync(join(fixture, "policy.json"), "utf8"));
const complete = { scanComplete: true };

function readAt({ file, bytes }) {
  return readFileSync(file).subarray(bytes.start, bytes.end).toString("utf8");
}

function checks(result) {
  return Object.fromEntries(result.candidates.map((candidate) => [readAt(candidate.location).match(/\bfunction (\w+)/)[1], candidate]));
}

test("audit policies resolve symbols, track local and cross-file values, and explain results", () => {
  const workspace = mkdtempSync(join(tmpdir(), "astscan-audit-"));
  const audit = createTypeScriptAudit(join(workspace, "tsconfig.json"));
  try {
    cpSync(fixture, workspace, { recursive: true });
    const app = join(workspace, "app.ts");
    writeFileSync(app, "\ufeff" + readFileSync(app, "utf8"));
    const matches = () => scan(astscan, workspace, "audit/fixture/candidates.yml", ["app.ts", "helpers.ts"]);
    const matchesV1 = matches();
    const result = audit.check("v1", matchesV1, policy, complete);
    const byName = checks(result);
    const trueCases = [
      "direct", "oneBranch", "throughReturn", "throughEffect", "modeledCopy", "modeledField",
      "countedLoop", "loopCarry", "loopContinue", "throughAwait", "guardPromise",
    ];
    const falseCases = [
      "masked", "bothBranches", "unrelated", "shadowedApi", "checked", "fieldMasked", "throughParameter",
      "loopMasked", "loopUnreachable", "unreachableArgument", "unreachableAfterCall", "nestedLoopJumps", "awaitedMasked", "awaitedGuard",
    ];
    for (const name of trueCases) assert.equal(byName[name].exists, true, name);
    for (const name of falseCases) assert.equal(byName[name].exists, false, name);
    assert.equal(result.diagnostics.length, trueCases.length);
    assert.equal(result.coverage.state, "partial");
    assert(byName.guardPromise.issues.some(({ code }) => code === "unresolved-guard"));
    for (const [name, issue] of [["unknownCall", "unresolved-call"], ["unknownType", "unresolved-type"], ["unknownIterator", "unsupported-statement"], ["unawaited", "unawaited-call"]]) {
      assert.equal(byName[name].exists, null, name);
      assert(byName[name].issues.some(({ code }) => code === issue), name);
    }

    const unicode = result.diagnostics.find(({ locations }) => readAt(locations[0]) === "α + value");
    assert.equal(readAt(unicode.evidence[0].locations[0]), "read()");
    assert.equal(unicode.range.start.character, 6);
    assert(unicode.relatedInformation.length >= 2);
    const effect = result.diagnostics.find(({ locations }) => locations[0].file.endsWith("/helpers.ts"));
    assert.equal(readAt(effect.locations[0]), "target.value");
    assert(effect.evidence.some(({ kind }) => kind === "effect"));
    assert(effect.relatedInformation.some(({ location }) => location.uri.endsWith("/app.ts")));
    const awaited = result.diagnostics.find(({ locations }) => readAt(locations[0]) === "await fetchAsync()");
    assert.equal(readAt(awaited.evidence[0].locations[0]), "input()");
    assert(awaited.evidence.some(({ kind }) => kind === "await"));
    assert(awaited.relatedInformation.some(({ location }) => location.uri.endsWith("/helpers.ts")));
    assert.deepEqual(audit.check("v1", matchesV1, policy, complete), result);

    const negated = checks(audit.check("v1", matchesV1, { ...policy, query: "NOT EXISTS" }, complete));
    assert.equal(negated.masked.matched, true);
    assert.equal(negated.direct.matched, false);
    assert.equal(negated.unknownCall.matched, null);
    const partial = checks(audit.check("v1", matchesV1, { ...policy, query: "NOT EXISTS" }));
    assert.equal(partial.masked.matched, null);
    assert.equal(partial.direct.matched, false);

    const noCapture = matchesV1.filter((match) => match.ruleIds.includes("entry"));
    const unbound = checks(audit.check("v1", noCapture, policy, complete));
    assert.equal(unbound.direct.exists, null);
    assert(unbound.direct.issues.some(({ code }) => code === "unresolved-capture"));
    const param = checks(audit.check("v1", matchesV1, { ...policy, entryParameters: [0] }, complete));
    assert.equal(param.throughParameter.exists, true);
    const capped = checks(audit.check("v1", matchesV1, policy, { ...complete, calls: 0 }));
    assert.equal(capped.direct.exists, true);
    assert.equal(capped.throughReturn.exists, null);
    assert(capped.throughReturn.issues.some(({ code }) => code === "call-limit"));
    const loopBudget = checks(audit.check("v1", matchesV1, { ...policy, query: "NOT EXISTS" }, { ...complete, loops: 1 }));
    assert.equal(loopBudget.loopCarry.matched, null);
    assert(loopBudget.loopCarry.issues.some(({ code }) => code === "loop-limit"));

    const helpers = join(workspace, "helpers.ts");
    writeFileSync(helpers, readFileSync(helpers, "utf8").replace("return copy;", 'return "constant";'));
    const changed = checks(audit.check("v2", matches(), policy, complete));
    assert.equal(changed.throughReturn.exists, false);
    assert.equal(changed.throughEffect.exists, true);
  } finally {
    audit.close();
    rmSync(workspace, { recursive: true, force: true });
  }
});

test("an empty candidate batch is lazy and lists missing entry coverage", () => {
  const audit = createTypeScriptAudit(join(fixture, "absent-tsconfig.json"));
  try {
    const result = audit.check("empty", [], policy, complete);
    assert.deepEqual(result.candidates, []);
    assert.deepEqual(result.diagnostics, []);
    assert.equal(result.coverage.state, "partial");
    assert.equal(result.coverage.documents[0].state, "unreported");
  } finally {
    audit.close();
  }
});
