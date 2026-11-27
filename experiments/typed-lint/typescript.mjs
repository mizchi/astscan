import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";
import { capture } from "./scan.mjs";

const require = createRequire(import.meta.url);

// The scanner emits UTF-8 byte ranges. TypeScript nodes use UTF-16 offsets.
// Convert just the requested offsets in one forward pass over the source.
function positions(bytes, byteOffsets, bom) {
  const decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });
  const offsets = new Map();
  let byte = bom;
  let utf16 = 0;
  for (const next of [...new Set(byteOffsets)].sort((a, b) => a - b)) {
    if (next < byte || next > bytes.length || (bytes[next] & 0xc0) === 0x80) continue;
    utf16 += decoder.decode(bytes.subarray(byte, next)).length;
    offsets.set(next, utf16);
    byte = next;
  }
  return offsets;
}

function* awaitable(TypeFlags, checker, type) {
  if (type.flags & TypeFlags.TypeParameter) {
    type = (yield* checker.getBaseConstraintOfType.gen(type)) ?? type;
  }
  if (type.flags & TypeFlags.Union) {
    let status = "ok";
    for (const part of yield* type.getTypes.gen()) {
      const result = yield* awaitable(TypeFlags, checker, part);
      if (result === "violation") return result;
      if (result === "unknown") status = result;
    }
    return status;
  }
  if (type.flags & (TypeFlags.Any | TypeFlags.Unknown | TypeFlags.TypeParameter)) return "unknown";
  const awaited = yield* checker.getAwaitedType.gen(type);
  return awaited === undefined ? "unknown" : awaited.id === type.id ? "ok" : "violation";
}

function collectCalls({ isCallExpression, isExpressionStatement }, source, bytes, candidates, calls) {
  const bom = bytes[0] === 0xef && bytes[1] === 0xbb && bytes[2] === 0xbf && source.text.charCodeAt(0) !== 0xfeff ? 3 : 0;
  if (!bytes.subarray(bom).equals(Buffer.from(source.text))) return;
  const offsets = positions(bytes, candidates.flatMap(({ range }) => [range.start, range.end]), bom);
  const byStart = Map.groupBy(candidates, ({ range }) => offsets.get(range.start));

  function visit(node) {
    if (isExpressionStatement(node) && isCallExpression(node.expression)) {
      const call = node.expression;
      const group = byStart.get(call.getStart(source));
      if (group) {
        const exact = group.filter(({ range }) => offsets.get(range.end) === call.getEnd());
        if (exact.length) calls.push({ call, candidates: exact });
      }
    }
    node.forEachChild(visit);
  }

  source.forEachChild(visit);
}

function classify({ api, ast, TypeFlags }, { program, checker, facts }, projectDir, matches) {
  const candidates = matches.map((match) => ({
    file: match.file,
    line: match.range.start.line + 1,
    range: capture(match, "CALL"),
    status: "unknown",
  }));
  const files = [...Map.groupBy(candidates, (candidate) => resolve(projectDir, candidate.file))];
  const sources = api.batch(...files.map(([file]) => program.getSourceFile.gen(file)));
  const calls = [];
  for (const [index, [file, group]] of files.entries()) {
    if (sources[index]) collectCalls(ast, sources[index], readFileSync(file), group, calls);
  }
  if (!calls.length) return candidates;

  const types = checker.getTypeAtLocation(calls.map(({ call }) => call));
  const missing = [...new Map(types.filter((type) => !facts.has(type.id)).map((type) => [type.id, type])).values()];
  if (missing.length) {
    const statuses = api.batch(...missing.map((type) => awaitable(TypeFlags, checker, type)));
    for (const [index, type] of missing.entries()) facts.set(type.id, statuses[index]);
  }
  for (const [index, { candidates: group }] of calls.entries()) {
    for (const candidate of group) candidate.status = facts.get(types[index].id);
  }
  return candidates;
}

/**
 * Opt-in type facts for one project. The host advances the revision whenever
 * source files, dependencies or compiler inputs change.
 */
export function createTypeScriptAdapter(tsconfig) {
  const configPath = resolve(tsconfig);
  const projectDir = dirname(configPath);
  let revision;
  let engine;
  let project;

  function releaseProject() {
    const old = project;
    project = undefined;
    old?.snapshot.dispose();
  }

  return {
    /**
     * @param {string} projectRevision - Identity of the snapshot scanned by astscan.
     * @param {Array<object>} matches - Slim JSON matches carrying the CALL capture.
     */
    checkDiscardedCalls(projectRevision, matches) {
      if (typeof projectRevision !== "string" || !projectRevision) throw new Error("A project revision is required for type queries");
      if (revision !== projectRevision) {
        releaseProject();
        revision = projectRevision;
      }
      if (!matches.length) return { candidates: [], programMs: 0, queryMs: 0 };

      let programMs = 0;
      if (!project) {
        const started = performance.now();
        const invalidateAll = engine !== undefined;
        if (!engine) {
          const { API, TypeFlags } = require("tsrs/sync");
          const ast = require("tsrs/ast/is");
          engine = { api: new API({ cwd: projectDir }), ast, TypeFlags };
        }
        const snapshot = engine.api.createSnapshot({
          openProjects: [configPath],
          fileNotifications: { invalidateAll },
        });
        const configured = snapshot.getConfiguredProject(configPath);
        if (!configured) {
          snapshot.dispose();
          throw new Error(`tsrs could not open ${configPath}`);
        }
        project = { snapshot, program: configured.program, checker: configured.checker, facts: new Map() };
        programMs = performance.now() - started;
      }

      const started = performance.now();
      const candidates = classify(engine, project, projectDir, matches);
      return { candidates, programMs, queryMs: performance.now() - started };
    },

    close() {
      try {
        releaseProject();
      } finally {
        const old = engine;
        engine = undefined;
        revision = undefined;
        old?.api.close();
      }
    },
  };
}
