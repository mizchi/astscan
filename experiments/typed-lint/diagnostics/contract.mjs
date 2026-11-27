import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { isAbsolute, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

/** @typedef {{start: number, end: number}} ByteRange */
/** @typedef {{line: number, character: number}} Position */
/** @typedef {{start: Position, end: Position}} Range */
/** @typedef {{uri: string, range: Range}} LspLocation */
/** @typedef {{location: LspLocation, message: string}} DiagnosticRelatedInformation */
/** @typedef {{range: Range, message: string, severity?: 1 | 2 | 3 | 4, code?: string | number, codeDescription?: {href: string}, source?: string, tags?: number[], relatedInformation?: DiagnosticRelatedInformation[], data?: unknown}} LspDiagnostic */
/** @typedef {{file: string, bytes?: ByteRange, line?: number, column?: number, range?: Range}} Location */
/** @typedef {{kind: string, message?: string, code?: string | number, locations: Location[], evidence?: Evidence[], replacement?: string, applicability?: string}} Evidence */
/** @typedef {{functionName?: string, address?: string, location?: Location, generated?: Location}} Frame */
/** @typedef {{label?: string, frames: Frame[]}} Trace */
/** @typedef {{message: string, location?: Location, traces?: Trace[]}} RuntimeReport */
/** @typedef {Omit<LspDiagnostic, "range"> & {uri?: string, range?: Range, phase: "compile" | "runtime" | "static", locations: Location[], evidence: Evidence[], traces?: Trace[]}} Diagnostic */
/** @typedef {Omit<Diagnostic, "severity"> & {severity: "error" | "warning" | "info"}} NativeDiagnostic */
/** @typedef {"complete" | "provisional" | "unreported"} DocumentState */
/** @typedef {{scope: "project", state: "complete" | "partial"} | {scope: "documents", state: "complete" | "partial", documents: {uri: string, state: DocumentState}[]}} Coverage */
/** @typedef {{revision: string, diagnostics: Diagnostic[], coverage: Coverage}} DiagnosticBatch */
/** @typedef {{uri: string, languageId: string, text?: string}} Document */
/** @typedef {{documents: Document[]} | {project: true}} DiagnosticScope */
/** @typedef {{check(revision: string, scope?: DiagnosticScope): DiagnosticBatch | Promise<DiagnosticBatch>, close(): void}} DiagnosticConnector */

/**
 * @param {() => NativeDiagnostic[]} run
 */
export function byRevision(run, close = () => {}) {
  /** @type {DiagnosticBatch | undefined} */
  let batch;
  return {
    check(revision) {
      if (typeof revision !== "string" || revision.length === 0) throw new Error("A project revision is required");
      if (batch?.revision !== revision) {
        const sources = sourceLocations(process.cwd());
        batch = {
          revision,
          diagnostics: run().map((diagnostic) => nativeDiagnostic(diagnostic, sources)),
          coverage: { scope: "project", state: "complete" },
        };
      }
      return batch;
    },
    close() {
      batch = undefined;
      close();
    },
  };
}

/**
 * A shared lint predicate for astscan ranges, changed lines converted to byte
 * ranges, and compiler diagnostics. Related evidence travels with each hit.
 * @param {Diagnostic[]} diagnostics
 * @param {(ByteRange & {file: string})[]} ranges
 */
export function diagnosticsInRanges(diagnostics, ranges, { cwd = process.cwd() } = {}) {
  const byFile = Map.groupBy(ranges, ({ file }) => fileIdentity(file, cwd));
  return diagnostics.filter(({ locations }) => locations.some(({ file, bytes }) =>
    bytes && byFile.get(file)?.some(({ start, end }) =>
      bytes.start === bytes.end
        ? start <= bytes.start && bytes.start < end
        : bytes.start < end && start < bytes.end)));
}

export function fileIdentity(file, cwd) {
  if (file.startsWith("file:")) return fileURLToPath(file);
  if (isAbsolute(file) || !/^(?:[a-z][a-z\d+.-]*:|<)/i.test(file)) return resolve(cwd, file);
  return file;
}

const severity = { error: 1, warning: 2, info: 3 };

/** @param {NativeDiagnostic} diagnostic */
export function nativeDiagnostic(diagnostic, sources = sourceLocations(process.cwd())) {
  const location = (site) => {
    if (!site) return {};
    const uri = isAbsolute(site.file) ? pathToFileURL(site.file).href : site.file.includes(":") ? site.file : undefined;
    return { uri, range: site.range ?? (site.bytes && sources.range(site.file, site.bytes)) };
  };
  const relatedInformation = [];
  function related(evidence) {
    for (const item of evidence) {
      if (item.message) {
        for (const site of item.locations) {
          const entry = location(site);
          if (entry.uri && entry.range) relatedInformation.push({ location: entry, message: item.message });
        }
      }
      if (item.evidence) related(item.evidence);
    }
  }
  related(diagnostic.evidence);
  return {
    ...diagnostic,
    ...location(diagnostic.locations[0]),
    severity: severity[diagnostic.severity],
    ...(relatedInformation.length ? { relatedInformation } : {}),
  };
}

/** @param {RuntimeReport} report */
export function runtimeReport(source, report, cwd) {
  const locate = (location) => location && { ...location, file: fileIdentity(location.file, cwd) };
  const traces = (report.traces ?? []).map(({ label, frames }) => ({
    label,
    frames: frames.map((frame) => ({ ...frame, location: locate(frame.location) })),
  }));
  const primary = locate(report.location) ?? traces.find(({ frames }) => frames.some(({ location }) => location))
    ?.frames.find(({ location }) => location)?.location;
  return nativeDiagnostic({
    source, phase: "runtime", severity: "error",
    message: report.message, locations: primary ? [primary] : [], evidence: [], traces,
  });
}

function readLocal(file) {
  if (!isAbsolute(file)) return undefined;
  try {
    return readFileSync(file);
  } catch {
    return undefined;
  }
}

function utf16Lines(text, skipBom, breaks = /\r\n|[\r\n\u2028\u2029]/g) {
  const lines = [];
  let start = skipBom && text.startsWith("\ufeff") ? 1 : 0;
  let bytes = start === 1 ? 3 : 0;
  for (const match of text.matchAll(breaks)) {
    const next = match.index + match[0].length;
    lines.push({ start, end: match.index, bytes });
    bytes += Buffer.byteLength(text.slice(start, next));
    start = next;
  }
  lines.push({ start, end: text.length, bytes });
  return lines;
}

function utf16Byte(text, row, column) {
  if (!row || column < 1) return undefined;
  const offset = row.start + column - 1;
  if (offset > row.end || (text.charCodeAt(offset) & 0xfc00) === 0xdc00) return undefined;
  return row.bytes + Buffer.byteLength(text.slice(row.start, offset));
}

function positionAtByte(text, lines, offset) {
  if (!Number.isInteger(offset) || offset < 0) return;
  let low = 0;
  let high = lines.length;
  while (low + 1 < high) {
    const mid = (low + high) >>> 1;
    if (lines[mid].bytes <= offset) low = mid;
    else high = mid;
  }
  const row = lines[low];
  let character = 0;
  let byte = row.bytes;
  for (const unit of text.slice(row.start, row.end)) {
    if (byte >= offset) break;
    byte += Buffer.byteLength(unit);
    character += unit.length;
  }
  if (byte === offset) return { line: low, character };
}

export function sourceLocations(cwd) {
  const local = new Map();
  const utf16 = new Map();
  const embedded = new Map();
  const byteLines = new Map();
  const lsp = new Map();
  function source(file) {
    if (!local.has(file)) local.set(file, readLocal(file));
    return local.get(file);
  }
  function lspText(file, content) {
    if (!lsp.has(file)) {
      const text = content ?? source(file)?.toString("utf8");
      lsp.set(file, text === undefined ? undefined : { text, lines: utf16Lines(text, false, /\r\n|[\r\n]/g) });
    }
    return lsp.get(file);
  }
  return {
    source,
    range(file, { start, end }) {
      const entry = lspText(file);
      if (!entry) return;
      const first = positionAtByte(entry.text, entry.lines, start);
      const last = positionAtByte(entry.text, entry.lines, end);
      if (first && last) return { start: first, end: last };
    },
    fromLsp(uri, range, content) {
      const file = fileIdentity(uri, cwd);
      const { start: first, end: last } = range;
      const location = { file, line: first.line + 1, column: first.character + 1, range };
      const entry = lspText(file, content);
      if (entry) {
        const start = utf16Byte(entry.text, entry.lines[first.line], first.character + 1);
        const end = utf16Byte(entry.text, entry.lines[last.line], last.character + 1);
        if (start !== undefined && end !== undefined && start <= end) location.bytes = { start, end };
      }
      return location;
    },
    // Lines and columns are 1-based. TS and source maps count UTF-16 units.
    utf16(file, line, column, { endLine = line, endColumn = column, content, skipBom = false } = {}) {
      file = fileIdentity(file, cwd);
      const cache = content == null ? utf16 : embedded;
      const key = content ?? file;
      if (!cache.has(key)) {
        const text = content ?? source(file)?.toString("utf8");
        cache.set(key, text === undefined ? undefined : { text, lines: utf16Lines(text, skipBom) });
      }
      const location = { file, line, column };
      if (content != null) {
        location.range = {
          start: { line: line - 1, character: column - 1 },
          end: { line: endLine - 1, character: endColumn - 1 },
        };
      }
      const entry = cache.get(key);
      if (entry) {
        const start = utf16Byte(entry.text, entry.lines[line - 1], column);
        const end = utf16Byte(entry.text, entry.lines[endLine - 1], endColumn);
        if (start !== undefined && end !== undefined && start <= end) location.bytes = { start, end };
      }
      return location;
    },
    byteColumn(file, line, column) {
      file = fileIdentity(file, cwd);
      const location = { file, line };
      if (column === undefined) return location;
      location.column = column;
      const bytes = source(file);
      if (!bytes) return location;
      if (!byteLines.has(file)) {
        const starts = [0];
        for (let i = bytes.indexOf(10); i !== -1; i = bytes.indexOf(10, i + 1)) starts.push(i + 1);
        byteLines.set(file, starts);
      }
      const starts = byteLines.get(file);
      const rowStart = starts[line - 1];
      const point = rowStart + column - 1;
      const rowEnd = line < starts.length ? starts[line] - 1 : bytes.length;
      if (Number.isInteger(point) && column > 0 && point <= rowEnd && (bytes[point] & 0xc0) !== 0x80) {
        location.bytes = { start: point, end: point };
      }
      return location;
    },
  };
}

export function compilerOutput(command, args, cwd) {
  const result = spawnSync(command, args, { cwd, encoding: "utf8", maxBuffer: 32 * 1024 * 1024 });
  if (result.error) throw result.error;
  if (result.signal) throw new Error(`${command} exited on ${result.signal}`);
  return result;
}

export function jsonLines(text) {
  return text.split("\n").flatMap((line) => {
    if (!line.startsWith("{")) return [];
    try {
      return [JSON.parse(line)];
    } catch {
      return [];
    }
  });
}
