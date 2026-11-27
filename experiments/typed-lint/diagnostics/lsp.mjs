import { spawn } from "node:child_process";
import { once } from "node:events";
import { readFileSync } from "node:fs";
import { basename, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { createMessageConnection } from "vscode-jsonrpc/node";
import { sourceLocations } from "./contract.mjs";

/** @typedef {import("./contract.mjs").LspDiagnostic} LspDiagnostic */
/** @typedef {import("./contract.mjs").DocumentState} DocumentState */
/** @typedef {Required<import("./contract.mjs").Document> & {version: number}} OpenDocument */

/**
 * @typedef {{
 *   request(method: string, params?: any): Promise<any>,
 *   notify(method: string, params?: any): void | Promise<void>,
 *   onNotification(method: string, handler: (params: any) => void): () => void,
 *   onRequest?(method: string, handler: (params: any) => any): void,
 *   close(): void
 * }} Transport
 */
/**
 * @typedef {{
 *   rpc: Transport, scope: "project" | "documents", documents: OpenDocument[],
 *   capabilities: any, signal: AbortSignal, confirmPull: boolean,
 *   report(uri: string, items: LspDiagnostic[], state: Exclude<DocumentState, "unreported">): void,
 *   completeProject(): void
 * }} Collection
 */

/** @returns {Promise<Transport>} */
export async function connectStdio(command, args, { cwd = process.cwd() } = {}) {
  const child = spawn(command, args, { cwd, stdio: ["pipe", "pipe", "pipe"], windowsHide: true });
  child.stderr.resume();
  await once(child, "spawn");
  const connection = createMessageConnection(child.stdout, child.stdin);
  connection.listen();
  const stop = () => connection.dispose();
  child.once("error", stop);
  child.once("close", stop);
  return {
    request: (method, params) => connection.sendRequest(method, ...(params === undefined ? [] : [params])),
    notify: (method, params) => connection.sendNotification(method, ...(params === undefined ? [] : [params])),
    onNotification: (method, handler) => {
      const subscription = connection.onNotification(method, handler);
      return () => subscription.dispose();
    },
    onRequest: (method, handler) => connection.onRequest(method, handler),
    close() {
      stop();
      child.kill();
    },
  };
}

function until(pending, signal) {
  const result = Promise.resolve(pending);
  if (signal.aborted) {
    void result.catch(() => {});
    return Promise.reject(signal.reason);
  }
  return new Promise((resolve, reject) => {
    const stop = () => reject(signal.reason);
    signal.addEventListener("abort", stop, { once: true });
    result.then(resolve, reject).finally(() => signal.removeEventListener("abort", stop));
  });
}

/**
 * Open the requested documents, prefer full pull reports, and collect pushes
 * as provisional snapshots. Only the server can signal a final push.
 * @param {Collection} context
 */
export async function collectDocumentDiagnostics({ rpc, documents, capabilities, signal, confirmPull, report }) {
  const versions = new Map(documents.map(({ uri, version }) => [uri, version]));
  const confirmed = new Set();
  const pushed = new Set();
  const dispose = rpc.onNotification("textDocument/publishDiagnostics", ({ uri, version, diagnostics }) => {
    if (versions.has(uri) && (version === undefined || version === versions.get(uri)) && Array.isArray(diagnostics)) {
      pushed.add(uri);
      report(uri, diagnostics, "provisional");
    }
  });
  try {
    for (const document of documents) {
      signal.throwIfAborted();
      await rpc.notify("textDocument/didOpen", { textDocument: document });
    }
    const provider = capabilities.diagnosticProvider;
    if (provider) {
      await Promise.allSettled(documents.map(async ({ uri }) => {
        const result = await rpc.request("textDocument/diagnostic", { textDocument: { uri }, identifier: provider.identifier });
        if (result?.kind !== "full" || !Array.isArray(result.items)) return;
        if (confirmPull) {
          report(uri, result.items, "complete");
          confirmed.add(uri);
        } else if (!pushed.has(uri)) {
          report(uri, result.items, "provisional");
        }
        for (const [relatedUri, entry] of Object.entries(result.relatedDocuments ?? {})) {
          if (entry.kind === "full" && Array.isArray(entry.items) && !versions.has(relatedUri)) {
            report(relatedUri, entry.items, confirmPull ? "complete" : "provisional");
          }
        }
      }));
    }
    if (confirmed.size !== documents.length && !signal.aborted) {
      await new Promise((resolve) => signal.addEventListener("abort", resolve, { once: true }));
    }
  } finally {
    dispose();
  }
}

/**
 * @param {{
 *   cwd?: string,
 *   connect(context: {cwd: string, signal: AbortSignal}): Transport | Promise<Transport>,
 *   collect?: (context: Collection) => Promise<void>,
 *   configure?: (rpc: Transport) => void | ((context: Collection) => Promise<void>),
 *   clientCapabilities?: any,
 *   confirmPull?: boolean,
 *   initializationOptions?: unknown,
 *   timeoutMs?: number
 * }} options
 * @returns {import("./contract.mjs").DiagnosticConnector}
 */
export function createLspConnector({
  cwd = process.cwd(), connect, collect = collectDocumentDiagnostics, configure, clientCapabilities, confirmPull = false,
  initializationOptions, timeoutMs = 5_000,
}) {
  cwd = resolve(cwd);
  const rootUri = pathToFileURL(cwd).href;
  const folders = [{ uri: rootUri, name: basename(cwd) }];
  const active = new Set();
  return {
    async check(revision, scope) {
      if (typeof revision !== "string" || !revision) throw new Error("A project revision is required");
      const project = scope && "project" in scope;
      const requested = scope && "documents" in scope ? scope.documents : undefined;
      if (!project && !requested) throw new Error("The LSP connector needs an explicit diagnostic scope");
      /** @type {Map<string, OpenDocument>} */
      const byUri = new Map((requested ?? []).map(({ uri, languageId, text }) => [
        uri, { uri, languageId, version: 1, text: text ?? readFileSync(fileURLToPath(uri), "utf8") },
      ]));
      const documents = [...byUri.values()];
      const found = new Map();
      const controller = new AbortController();
      let rpc;
      let projectComplete = false;
      active.add(controller);
      try {
        if (project || documents.length) {
          const init = AbortSignal.any([controller.signal, AbortSignal.timeout(timeoutMs)]);
          const opening = Promise.resolve(connect({ cwd, signal: init }));
          try {
            rpc = await until(opening, init);
          } catch (error) {
            void opening.then((client) => client.close(), () => {});
            throw error;
          }
          rpc.onRequest?.("workspace/configuration", ({ items }) => items.map(() => null));
          rpc.onRequest?.("workspace/workspaceFolders", () => folders);
          rpc.onRequest?.("window/workDoneProgress/create", () => null);
          const collectSession = configure?.(rpc) ?? collect;
          if (project && collectSession === collectDocumentDiagnostics) {
            throw new Error("Project diagnostics need a server-specific LSP collector");
          }
          const { capabilities } = await until(rpc.request("initialize", {
            processId: process.pid,
            rootUri,
            workspaceFolders: folders,
            initializationOptions,
            capabilities: {
              ...clientCapabilities,
              general: { ...clientCapabilities?.general, positionEncodings: ["utf-16"] },
              workspace: { configuration: true, workspaceFolders: true, ...clientCapabilities?.workspace },
              textDocument: {
                diagnostic: { relatedDocumentSupport: true },
                publishDiagnostics: { relatedInformation: true, versionSupport: true, dataSupport: true, codeDescriptionSupport: true, tagSupport: { valueSet: [1, 2] } },
                ...clientCapabilities?.textDocument,
              },
            },
          }), init);
          if (capabilities.positionEncoding && capabilities.positionEncoding !== "utf-16") {
            throw new Error(`Unexpected LSP position encoding ${capabilities.positionEncoding}`);
          }
          await until(rpc.notify("initialized", {}), init);
          const signal = AbortSignal.any([controller.signal, AbortSignal.timeout(timeoutMs)]);
          try {
            await until(collectSession({
              rpc, scope: project ? "project" : "documents", documents, capabilities, signal, confirmPull,
              report(uri, items, state) {
                if (signal.aborted || (state === "provisional" && found.get(uri)?.state === "complete")) return;
                found.set(uri, { items, state });
              },
              completeProject() {
                if (project && !signal.aborted) projectComplete = true;
              },
            }), signal);
          } catch (error) {
            if (!signal.aborted || controller.signal.aborted || error !== signal.reason) throw error;
          }
        }
        const sources = sourceLocations(cwd);
        const location = (uri, range) => sources.fromLsp(uri, range, byUri.get(uri)?.text);
        const diagnostics = [...found].flatMap(([uri, { items }]) => items.map((item) => ({
          ...item,
          uri,
          phase: "compile",
          locations: [location(uri, item.range)],
          evidence: (item.relatedInformation ?? []).map(({ message, location: site }) => ({
            kind: "related", message, locations: [location(site.uri, site.range)],
          })),
        })));
        const checked = documents.map(({ uri }) => ({ uri, state: found.get(uri)?.state ?? "unreported" }));
        return {
          revision, diagnostics,
          coverage: project ? { scope: "project", state: projectComplete ? "complete" : "partial" } : {
            scope: "documents",
            state: checked.every(({ state }) => state === "complete") ? "complete" : "partial",
            documents: checked,
          },
        };
      } finally {
        active.delete(controller);
        controller.abort();
        rpc?.close();
      }
    },
    close() {
      for (const controller of active) controller.abort();
    },
  };
}
