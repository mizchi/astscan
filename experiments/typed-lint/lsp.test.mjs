import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { diagnosticsInRanges } from "./diagnostics/contract.mjs";
import { createLspConnector } from "./diagnostics/lsp.mjs";
import { createTypeScriptLspConnector } from "./diagnostics/typescript.mjs";

const span = (line, start, end) => ({
  start: { line, character: start },
  end: { line, character: end },
});

test("pull reports preserve LSP fields and join against the opened source bytes", async () => {
  const uri = "file:///project/a.ts";
  const raw = {
    range: span(0, 2, 3),
    severity: 2,
    code: 100,
    codeDescription: { href: "https://example.test/100" },
    source: "fixture",
    message: "warning",
    tags: [1],
    data: { key: "action" },
    relatedInformation: [{ location: { uri, range: span(0, 3, 4) }, message: "related" }],
  };
  const seen = [];
  const connector = createLspConnector({
    cwd: "/project",
    confirmPull: true,
    connect: () => ({
      async request(method, params) {
        seen.push(method);
        if (method === "initialize") return { capabilities: { diagnosticProvider: {} } };
        assert.equal(params.textDocument.uri, uri);
        return { kind: "full", items: [raw] };
      },
      notify(method) { seen.push(method); },
      onNotification() { return () => {}; },
      close() { seen.push("close"); },
    }),
  });
  const result = await connector.check("fixture-v1", { documents: [{ uri, text: "🙂ab", languageId: "typescript" }] });
  const [diagnostic] = result.diagnostics;
  for (const key of Object.keys(raw)) assert.deepEqual(diagnostic[key], raw[key]);
  assert.deepEqual(diagnostic.locations[0].bytes, { start: 4, end: 5 });
  assert.deepEqual(diagnostic.evidence[0].locations[0].bytes, { start: 5, end: 6 });
  assert.deepEqual(diagnosticsInRanges(result.diagnostics, [{ file: "/project/a.ts", start: 4, end: 5 }]), [diagnostic]);
  assert.deepEqual(result.coverage, { scope: "documents", state: "complete", documents: [{ uri, state: "complete" }] });
  assert.deepEqual(seen, ["initialize", "initialized", "textDocument/didOpen", "textDocument/diagnostic", "close"]);
});

test("a full but unconfirmed LSP reply does not imply a clean document", async () => {
  const uri = "file:///project/a.rs";
  const connector = createLspConnector({
    timeoutMs: 30,
    connect: () => ({
      async request(method) {
        return method === "initialize" ? { capabilities: { diagnosticProvider: {} } } : { kind: "full", items: [] };
      },
      notify() {},
      onNotification() { return () => {}; },
      close() {},
    }),
  });
  const result = await connector.check("fixture-v1", { documents: [{ uri, languageId: "rust", text: "" }] });
  assert.deepEqual(result.coverage, { scope: "documents", state: "partial", documents: [{ uri, state: "provisional" }] });
});

test("push snapshots remain provisional and an absent report stays unreported", async () => {
  const a = "file:///project/a.ts";
  const b = "file:///project/b.ts";
  let publish;
  const connector = createLspConnector({
    cwd: "/project",
    timeoutMs: 40,
    connect: () => ({
      async request() { return { capabilities: {} }; },
      notify(method, params) {
        if (method !== "textDocument/didOpen") return;
        const { uri, version } = params.textDocument;
        if (uri === a) publish({ uri, version, diagnostics: [] });
        if (uri === b) publish({ uri, version: version - 1, diagnostics: [] });
      },
      onNotification(method, handler) {
        assert.equal(method, "textDocument/publishDiagnostics");
        publish = handler;
        return () => {};
      },
      close() {},
    }),
  });
  const documents = [a, b].map((uri) => ({ uri, text: "", languageId: "typescript" }));
  const result = await connector.check("fixture-v1", { documents });
  assert.deepEqual(result.diagnostics, []);
  assert.deepEqual(result.coverage, {
    scope: "documents", state: "partial",
    documents: [{ uri: a, state: "provisional" }, { uri: b, state: "unreported" }],
  });
});

test("a refused pull leaves any push diagnostics available", async () => {
  const uri = "file:///project/a.rs";
  let publish;
  const connector = createLspConnector({
    timeoutMs: 30,
    confirmPull: true,
    connect: () => ({
      async request(method) {
        if (method === "initialize") return { capabilities: { diagnosticProvider: {} } };
        throw new Error("Method not found");
      },
      notify(method) {
        if (method === "textDocument/didOpen") publish({ uri, diagnostics: [{ message: "from push", range: span(0, 0, 1) }] });
      },
      onNotification(_method, handler) {
        publish = handler;
        return () => {};
      },
      close() {},
    }),
  });
  const result = await connector.check("v1", { documents: [{ uri, text: "x", languageId: "rust" }] });
  assert.equal(result.diagnostics[0].message, "from push");
  assert.equal(result.coverage.documents[0].state, "provisional");
});

test("a server profile can drive custom readiness and report final results", async () => {
  const uri = "file:///project/fixture.rs";
  const connector = createLspConnector({
    cwd: "/project",
    connect: () => ({
      async request(method) {
        if (method === "initialize") return { capabilities: {} };
        assert.equal(method, "fixture/check");
        return [{ range: span(0, 0, 1), message: "custom diagnostics" }];
      },
      notify() {},
      close() {},
    }),
    collect: async ({ rpc, documents, report }) => {
      const [document] = documents;
      const items = await rpc.request("fixture/check", { uri: document.uri });
      report(document.uri, items, "complete");
    },
  });
  const result = await connector.check("fixture-v1", { documents: [{ uri, text: "x", languageId: "rust" }] });
  assert.equal(result.coverage.state, "complete");
  assert.equal(result.diagnostics[0].message, "custom diagnostics");
});

test("project coverage is only complete when a custom collector confirms it", async () => {
  let ready = false;
  const uri = "file:///project/a.rs";
  const connector = createLspConnector({
    connect: () => ({
      async request() { return { capabilities: {} }; },
      notify() {},
      close() {},
    }),
    collect: async ({ scope, report, completeProject }) => {
      assert.equal(scope, "project");
      report(uri, [{ range: span(0, 0, 1), message: "project diagnostic" }], "complete");
      if (ready) completeProject();
    },
  });
  const before = await connector.check("fixture-v1", { project: true });
  assert.equal(before.diagnostics.length, 1);
  assert.deepEqual(before.coverage, { scope: "project", state: "partial" });
  ready = true;
  assert.deepEqual((await connector.check("fixture-v2", { project: true })).coverage, { scope: "project", state: "complete" });
});

test("tsrs over stdio LSP returns a TypeScript error from the configured project", async (t) => {
  const cwd = mkdtempSync(join(tmpdir(), "astscan-lsp-"));
  t.after(() => rmSync(cwd, { recursive: true, force: true }));
  const uri = pathToFileURL(join(cwd, "main.ts")).href;
  const source = 'const emoji = "🙂"; const bad: number = "text";\n';
  writeFileSync(fileURLToPath(uri), source);
  writeFileSync(join(cwd, "tsconfig.json"), '{"compilerOptions":{"strict":true},"files":["main.ts"]}');
  const connector = createTypeScriptLspConnector({ cwd });
  t.after(() => connector.close());
  const result = await connector.check("fixture-v1", { documents: [{ uri, languageId: "typescript" }] });
  const diagnostic = result.diagnostics.find(({ code }) => Number(code) === 2322);
  assert.equal(result.coverage.state, "complete");
  assert.ok(diagnostic, JSON.stringify(result.diagnostics));
  assert.equal(diagnostic.locations[0].bytes.start, Buffer.byteLength(source.slice(0, source.indexOf("bad"))));
});
