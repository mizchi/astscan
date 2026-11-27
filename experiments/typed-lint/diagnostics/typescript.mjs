import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";
import { byRevision, fileIdentity, sourceLocations } from "./contract.mjs";
import { connectStdio, createLspConnector } from "./lsp.mjs";

const require = createRequire(import.meta.url);
const severity = ["warning", "error", "info", "info"];

export function createTypeScriptLspConnector(options = {}) {
  const bin = resolve(dirname(require.resolve("tsrs/package.json")), "bin/tsrs");
  return createLspConnector({
    confirmPull: true,
    ...options,
    connect: ({ cwd }) => connectStdio(process.execPath, [bin, "--lsp", "-stdio"], { cwd }),
  });
}

export function createTypeScriptDiagnostics(tsconfig) {
  const config = resolve(tsconfig);
  const cwd = dirname(config);
  let api;
  return byRevision(() => {
    const invalidateAll = api !== undefined;
    if (!api) {
      const { API } = require("tsrs/sync");
      api = new API({ cwd });
    }
    const snapshot = api.createSnapshot({ openProjects: [config], fileNotifications: { invalidateAll } });
    try {
      const configured = snapshot.getConfiguredProject(config);
      if (!configured) throw new Error(`tsrs could not open ${config}`);
      const { program } = configured;
      const diagnostics = api.batch(
        program.getConfigFileParsingDiagnostics.gen(),
        program.getProgramDiagnostics.gen(),
        program.getGlobalDiagnostics.gen(),
        program.getSyntacticDiagnostics.gen(),
        program.getSemanticDiagnostics.gen(),
      );
      const locate = sourceLocations(cwd);
      function locations(d) {
        if (!d.fileName) return [];
        if (!d.startPosition || !d.endPosition) return [{ file: fileIdentity(d.fileName, cwd) }];
        return [locate.utf16(d.fileName, d.startPosition.line + 1, d.startPosition.character + 1, {
          endLine: d.endPosition.line + 1,
          endColumn: d.endPosition.character + 1,
          skipBom: true,
        })];
      }
      function evidence(d) {
        const entries = (kind, items = []) => items.map((item) => ({
          kind,
          code: String(item.code),
          message: item.text,
          locations: locations(item),
          evidence: evidence(item),
        }));
        return [...entries("explanation", d.messageChain), ...entries("related", d.relatedInformation)];
      }
      return diagnostics.flat().map((d) => ({
        source: d.source || "tsrs",
        phase: "compile",
        severity: severity[d.category] ?? "info",
        code: String(d.code),
        message: d.text,
        locations: locations(d),
        evidence: evidence(d),
      }));
    } finally {
      snapshot.dispose();
    }
  }, () => {
    const old = api;
    api = undefined;
    old?.close();
  });
}
