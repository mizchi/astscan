import { dirname, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { fileIdentity, nativeDiagnostic } from "../diagnostics/contract.mjs";
import { createAuditFacts } from "./facts.mjs";
import { auditFunction } from "./flow.mjs";

const modelKinds = new Set([
  "source", "sink", "sanitize-return", "sanitize-field", "guard", "propagate-return", "propagate-field",
]);
const indexedKinds = new Set(["sink", "sanitize-field", "guard", "propagate-return", "propagate-field"]);
const severity = new Set(["error", "warning", "info"]);
const types = new Set(["string", "number", "boolean"]);
const argument = (value) => Number.isSafeInteger(value) && value >= 0;
const text = (value) => typeof value === "string" && value.length > 0;

function compilePolicy(policy, cwd) {
  if (!text(policy?.id) || !text(policy.message) || !text(policy.entryRule) || !Array.isArray(policy.models)) {
    throw new Error("An audit policy needs id, message, entryRule, and models");
  }
  const { query = "EXISTS", level = "warning", entryFiles, entryParameters = [] } = policy;
  if (query !== "EXISTS" && query !== "NOT EXISTS") throw new Error("Expected EXISTS or NOT EXISTS");
  if (!severity.has(level)) throw new Error("Invalid audit level");
  if (!Array.isArray(entryParameters) || !entryParameters.every(argument)) throw new Error("Invalid entry parameter indices");
  if (entryFiles !== undefined && (!Array.isArray(entryFiles) || !entryFiles.every(text))) throw new Error("Invalid entryFiles");
  const seen = new Set();
  const models = policy.models.map((model) => {
    if (!modelKinds.has(model?.kind) || !text(model.file) || !text(model.name)) throw new Error("Invalid audit model declaration");
    if (indexedKinds.has(model.kind) && !argument(model.argument)) throw new Error(`Invalid argument index for ${model.name}`);
    if ((model.kind === "propagate-field" || model.kind === "sanitize-field") && !text(model.field)) throw new Error(`Expected a field for ${model.name}`);
    if (model.kind === "propagate-field" && !argument(model.target)) throw new Error(`Invalid target index for ${model.name}`);
    if (model.type !== undefined && (model.kind !== "sink" || !types.has(model.type))) throw new Error(`Invalid argument type for ${model.name}`);
    if (model.capture !== undefined && (model.kind !== "sink" || !text(model.capture?.rule) || !text(model.capture.name))) {
      throw new Error(`Invalid capture selector for ${model.name}`);
    }
    const file = fileIdentity(model.file, cwd);
    const key = JSON.stringify([file, model.name]);
    if (seen.has(key)) throw new Error(`Duplicate audit model for ${model.name} in ${model.file}`);
    seen.add(key);
    return { ...model, file };
  });
  const files = entryFiles && new Set(entryFiles.map((file) => fileIdentity(file, cwd)));
  return { ...policy, query, level, entryParameters, files, models };
}

function scanLocation(file, bytes) {
  if (!argument(bytes?.start) || !argument(bytes?.end) || bytes.end < bytes.start) throw new Error(`Invalid astscan byte range in ${file}`);
  return { file, bytes };
}

function selectMatches(matches, rule, cwd) {
  const selectors = new Map();
  for (const { capture } of rule.models) {
    if (!capture) continue;
    if (!selectors.has(capture.rule)) selectors.set(capture.rule, new Set());
    selectors.get(capture.rule).add(capture.name);
  }
  const selected = [];
  const seenEntries = new Set();
  for (const match of matches) {
    const file = fileIdentity(match.file, cwd);
    const range = match.range?.byteOffset;
    const ids = match.ruleIds ?? [match.ruleId];
    if (ids.includes(rule.entryRule) && (!rule.files || rule.files.has(file))) {
      const entry = scanLocation(file, range);
      const key = JSON.stringify([file, range.start, range.end]);
      if (!seenEntries.has(key)) selected.push({ ...entry, kind: "entry" });
      seenEntries.add(key);
    }
    for (const id of ids) {
      for (const name of selectors.get(id) ?? []) {
        const focus = match.metaVariables?.single?.[name]?.range.byteOffset;
        if (focus) selected.push({ ...scanLocation(file, range), kind: "focus", selector: id, name, focus });
      }
    }
  }
  return selected;
}

function indexedCaptures(candidates) {
  const byId = Map.groupBy(candidates.filter(({ kind, node }) => kind === "focus" && node), ({ node }) => node.id);
  return (call, arg, { rule, name }) => byId.get(call.id)?.some(({ selector, name: capture, focusOffsets }) =>
    selector === rule && capture === name && focusOffsets?.[0] === arg.getStart() && focusOffsets[1] === arg.getEnd());
}

/** Run opt-in audit policies over astscan JSON matches from the same revision. */
export function createTypeScriptAudit(tsconfig) {
  const configPath = resolve(tsconfig);
  const cwd = dirname(configPath);
  const session = createAuditFacts(configPath, cwd);
  return {
    check(revision, matches, policy, { scanComplete = false, steps = 20000, calls = 8, nesting = 128, loops = 32 } = {}) {
      if (!text(revision)) throw new Error("A project revision is required for audit queries");
      if (!argument(steps) || steps === 0 || !argument(calls) || calls > 64 || !argument(nesting) || nesting === 0 || nesting > 256
        || !argument(loops) || loops === 0) {
        throw new Error("Invalid audit limits");
      }
      const rule = compilePolicy(policy, cwd);
      const selected = selectMatches(matches, rule, cwd);
      const diagnostics = [];
      const candidates = [];
      if (selected.some(({ kind }) => kind === "entry")) {
        const facts = session.get(revision);
        const mapped = facts.candidates(selected);
        const captures = indexedCaptures(mapped);
        const reported = new Set();
        for (const { file, bytes, kind, node } of mapped) {
          if (kind !== "entry") continue;
          const entry = { file, bytes };
          const result = node && facts.ast.isFunctionDeclaration(node)
            ? auditFunction(facts, node, rule, captures, { steps, calls, nesting, loops })
            : { findings: [], issues: [{ code: "unresolved-entry", message: "The AST match did not resolve to a function in this snapshot.", location: entry }] };
          if (!scanComplete) result.issues.push({ code: "partial-scan", message: "The structural scan did not report complete coverage.", location: entry });
          const exists = result.findings.length > 0 ? true : result.issues.length === 0 ? false : null;
          const matched = exists === null ? null : rule.query === "EXISTS" ? exists : !exists;
          const location = node ? facts.location(node) : entry;
          candidates.push({ location, exists, matched, issues: result.issues });
          if (matched === true) {
            const reports = rule.query === "EXISTS" ? result.findings : [{ location, path: [] }];
            for (const report of reports) {
              const key = JSON.stringify([report.location.file, report.location.range ?? report.location.bytes]);
              if (reported.has(key)) continue;
              reported.add(key);
              diagnostics.push(nativeDiagnostic({
                phase: "static", source: "astscan-audit", severity: rule.level, code: rule.id, message: rule.message,
                locations: [report.location], evidence: report.path, data: { query: rule.query, entry: location },
              }, facts.sources));
            }
          }
        }
      }
      const byFile = Map.groupBy(candidates, ({ location }) => location.file);
      for (const file of rule.files ?? []) if (!byFile.has(file)) byFile.set(file, []);
      const documents = [...byFile].map(([file, checks]) => ({
        uri: pathToFileURL(file).href,
        state: !checks.length ? "unreported" : checks.some(({ issues }) => issues.length > 0) ? "provisional" : "complete",
      }));
      const state = scanComplete && documents.every(({ state }) => state === "complete") ? "complete" : "partial";
      return { revision, diagnostics, candidates, coverage: { scope: "documents", state, documents } };
    },
    close: () => session.close(),
  };
}
