import { dirname, resolve } from "node:path";
import { byRevision, compilerOutput, fileIdentity, jsonLines, nativeDiagnostic, runtimeReport } from "./contract.mjs";

function rustDiagnostic(root, cwd) {
  const locate = (span) => ({
    file: fileIdentity(span.file_name, cwd),
    line: span.line_start,
    column: span.column_start,
    bytes: { start: span.byte_start, end: span.byte_end },
  });
  function expansion(span) {
    if (!span.expansion) return [];
    const { span: call, def_site_span: definition, macro_decl_name: message } = span.expansion;
    return [{
      kind: "expansion", message,
      locations: [locate(call), ...(definition ? [locate(definition)] : [])],
      evidence: expansion(call),
    }];
  }
  function evidence(diagnostic) {
    const related = diagnostic.spans.filter((span) => !span.is_primary).map((span) => ({
      kind: "related", message: span.label ?? undefined, locations: [locate(span)],
    }));
    const edits = diagnostic.spans.filter((span) => span.suggested_replacement !== null && span.suggested_replacement !== undefined)
      .map((span) => ({
        kind: "suggestion", message: span.label ?? undefined, locations: [locate(span)],
        replacement: span.suggested_replacement, applicability: span.suggestion_applicability ?? undefined,
      }));
    const children = diagnostic.children.map((child) => ({
      kind: child.level,
      code: child.code?.code,
      message: child.message,
      locations: child.spans.map(locate),
      evidence: evidence(child),
    }));
    return [...related, ...diagnostic.spans.flatMap(expansion), ...edits, ...children];
  }
  return {
    source: "rustc", phase: "compile",
    severity: root.level === "error" || root.level === "error: internal compiler error" ? "error"
      : root.level === "warning" ? "warning" : "info",
    code: root.code?.code,
    message: root.message,
    locations: root.spans.filter((span) => span.is_primary).map(locate),
    evidence: evidence(root),
  };
}

export function createRustAdapter({ cwd = process.cwd(), manifestPath, cargoArgs = [] } = {}) {
  cwd = resolve(cwd);
  const manifest = manifestPath ? ["--manifest-path", resolve(cwd, manifestPath)] : [];
  return byRevision(() => {
    // Cargo's relative rustc paths use the workspace root even for a member.
    const root = compilerOutput("cargo", ["locate-project", "--workspace", "--message-format=plain", ...manifest], cwd);
    if (root.status !== 0) throw new Error(root.stderr.trim() || "cargo could not locate the workspace");
    const workspace = dirname(root.stdout.trim());
    const { status, stdout, stderr } = compilerOutput("cargo", ["check", ...cargoArgs, "--message-format=json", ...manifest], cwd);
    const diagnostics = jsonLines(stdout)
      .filter((entry) => entry.reason === "compiler-message")
      .map(({ message }) => rustDiagnostic(message, workspace));
    if (status !== 0 && !diagnostics.some(({ severity }) => severity === "error")) {
      throw new Error(stderr.trim() || `cargo check exited with status ${status}`);
    }
    return diagnostics;
  });
}

/**
 * Read a structured panic report or an existing Rust panic log.
 * @param {string | import("./contract.mjs").RuntimeReport} input
 * @returns {import("./contract.mjs").Diagnostic | undefined}
 */
export function readRustPanic(input, { cwd = process.cwd() } = {}) {
  if (typeof input !== "string") return runtimeReport("rust", input, cwd);
  const lines = input.split(/\r?\n/);
  const header = /^thread '(.+)'(?: \(\d+\))? panicked at (.+):(\d+):(\d+):$/;
  const start = lines.findIndex((line) => header.test(line));
  if (start < 0) return undefined;
  const [, thread, file, line, column] = header.exec(lines[start]);
  const location = {
    file: fileIdentity(file, cwd), line: Number(line), column: Number(column),
  };
  const body = lines.slice(start + 1);
  const end = body.findIndex((line) => /^(?:stack backtrace:|note: run with `RUST_BACKTRACE=|thread ')/.test(line));
  const frames = [];
  if (body[end] === "stack backtrace:") {
    for (const row of body.slice(end + 1)) {
      if (row.startsWith("thread '")) break;
      const frame = /^\s*\d+:\s+(?:0x[\da-f]+\s+-\s+)?(.+)$/.exec(row);
      const site = /^\s+at (.+):(\d+):(\d+)$/.exec(row);
      if (frame) frames.push({ functionName: frame[1] });
      else if (site && frames.length && !frames.at(-1).location) {
        frames.at(-1).location = {
          file: fileIdentity(site[1], cwd), line: Number(site[2]), column: Number(site[3]),
        };
      }
    }
  }
  return nativeDiagnostic({
    source: "rust", phase: "runtime", severity: "error",
    message: (end === -1 ? body : body.slice(0, end)).join("\n").trim(),
    locations: [location], evidence: [],
    traces: frames.length ? [{ label: thread, frames }] : [],
  });
}
