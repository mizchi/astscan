import { devNull } from "node:os";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { byRevision, compilerOutput, fileIdentity, jsonLines, nativeDiagnostic, runtimeReport, sourceLocations } from "./contract.mjs";

const checker = fileURLToPath(new URL("./go-native/", import.meta.url));

function buildDiagnostics(output, cwd) {
  const events = Map.groupBy(jsonLines(output), ({ ImportPath }) => ImportPath);
  const locate = sourceLocations(cwd);
  const diagnostics = [];
  for (const build of events.values()) {
    if (!build.some(({ Action }) => Action === "build-fail")) continue;
    const lines = build.filter(({ Action }) => Action === "build-output").map(({ Output }) => Output).join("").split(/\r?\n/);
    let current;
    for (const line of lines) {
      if (!line.trim() || line.startsWith("# ")) continue;
      const text = line.trimStart();
      const span = /^(.+?):(\d+):(?:(\d+):)?\s+(.*)$/.exec(text);
      const locations = span ? [locate.byteColumn(span[1], Number(span[2]), span[3] ? Number(span[3]) : undefined)] : [];
      const message = span?.[4] ?? text;
      if (current && /^\s/.test(line)) {
        current.evidence.push({ kind: span ? "related" : "note", message, locations });
      } else {
        current = { source: "go", phase: "compile", severity: "error", message, locations, evidence: [] };
        diagnostics.push(current);
      }
    }
  }
  return diagnostics;
}

export function createGoAdapter({ cwd = process.cwd(), packages = ["./..."], mode = "types" } = {}) {
  cwd = resolve(cwd);
  if (mode !== "types" && mode !== "build") throw new Error(`Unknown Go check mode ${mode}`);
  return byRevision(() => {
    if (mode === "build") {
      const { status, stdout, stderr } = compilerOutput("go", ["build", "-json", "-o", devNull, ...packages], cwd);
      const diagnostics = buildDiagnostics(stdout, cwd);
      if (status !== 0 && diagnostics.length === 0) throw new Error(stderr.trim() || `go build exited with status ${status}`);
      return diagnostics;
    }
    const { status, stdout, stderr } = compilerOutput("go", [
      "run", "-mod=readonly", "./cmd/check", cwd, ...packages,
    ], checker);
    if (status !== 0) throw new Error(stderr.trim() || `Go diagnostic checker exited with status ${status}`);
    const locations = (list) => list.map((location) => ({ ...location, file: fileIdentity(location.file, cwd) }));
    return JSON.parse(stdout).map((diagnostic) => ({
      ...diagnostic,
      locations: locations(diagnostic.locations),
      evidence: diagnostic.evidence.map((entry) => ({ ...entry, locations: locations(entry.locations) })),
    }));
  });
}

/**
 * Read a structured panic report or an existing Go panic log.
 * @param {string | import("./contract.mjs").RuntimeReport} input
 * @returns {import("./contract.mjs").Diagnostic | undefined}
 */
export function readGoPanic(input, { cwd = process.cwd() } = {}) {
  if (typeof input !== "string") return runtimeReport("go", input, cwd);
  const lines = input.split(/\r?\n/);
  const start = lines.findIndex((line) => /^(?:panic|fatal error): /.test(line));
  if (start < 0) return undefined;
  const body = lines.slice(start + 1);
  const headerPattern = /^goroutine (\d+ \[.*\]):$/;
  const end = body.findIndex((line) => headerPattern.test(line) || /^exit status \d+$/.test(line));
  const continuation = (end < 0 ? body : body.slice(0, end)).map((line) => line.replace(/^\t/, ""));
  const message = [lines[start].replace(/^[^:]+: /, ""), ...continuation].join("\n").replace(/\n+$/, "");
  const traces = [];
  const evidence = [];
  let trace;
  let frame;
  for (const line of end < 0 ? [] : body.slice(end)) {
    const header = headerPattern.exec(line);
    if (header) {
      trace = { label: `goroutine ${header[1]}`, frames: [] };
      traces.push(trace);
      frame = undefined;
    } else if (trace) {
      const site = /^\t(.+):(\d+)(?:\s+.*)?$/.exec(line);
      if (site && frame) {
        const location = { file: fileIdentity(site[1], cwd), line: Number(site[2]) };
        if (frame.functionName.startsWith("created by ")) {
          evidence.push({ kind: "related", message: frame.functionName, locations: [location] });
        } else {
          frame.location = location;
          trace.frames.push(frame);
        }
        frame = undefined;
      } else if (/^created by /.test(line)) {
        frame = { functionName: line };
      } else {
        const name = /^(\S+)\(.*\)$/.exec(line);
        frame = name ? { functionName: name[1] } : undefined;
      }
    }
  }
  const primary = traces.find(({ frames }) => frames.length)?.frames[0]?.location;
  return nativeDiagnostic({
    source: "go", phase: "runtime", severity: "error",
    message, locations: primary ? [primary] : [], evidence, traces,
  });
}
