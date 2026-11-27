import { AnyMap, originalPositionFor, sourceContentFor } from "@jridgewell/trace-mapping";
import ErrorStackParser from "error-stack-parser";
import { readFileSync } from "node:fs";
import { findSourceMap } from "node:module";
import { isAbsolute } from "node:path";
import { pathToFileURL } from "node:url";
import { fileIdentity, nativeDiagnostic, sourceLocations } from "./contract.mjs";

function localMap(file, source) {
  const script = source(file)?.toString("utf8");
  if (!script) return;
  const directive = /(?:\/\/[#@]\s*sourceMappingURL=([^\s]+)|\/\*[#@]\s*sourceMappingURL=([^\s]+)\s*\*\/)\s*$/.exec(script);
  if (!directive) return;
  const scriptUrl = pathToFileURL(file);
  const url = new URL(directive[1] ?? directive[2], scriptUrl);
  if (url.protocol === "file:") return { map: readFileSync(url, "utf8"), url: url.href };
  if (url.protocol !== "data:") return;
  const [, media, payload] = /^data:(application\/json[^,]*),(.+)$/s.exec(url.href) ?? [];
  if (!media) return;
  const map = media.endsWith(";base64")
    ? Buffer.from(decodeURIComponent(payload), "base64").toString("utf8")
    : decodeURIComponent(payload);
  return { map, url: scriptUrl.href };
}

function resolveMap(file, loadMap, source) {
  try {
    const entry = loadMap?.(file);
    const url = isAbsolute(file) ? pathToFileURL(file).href : file;
    if (entry) return new AnyMap(entry.map, entry.url ?? url);
    const native = findSourceMap(file);
    if (native) return new AnyMap(native.payload, url);
    const local = localMap(file, source);
    if (local) return new AnyMap(local.map, local.url);
  } catch {
    // A missing or malformed map leaves the generated frame available.
  }
}

function stackData(error) {
  const descriptor = Object.getOwnPropertyDescriptor(Error, "prepareStackTrace");
  const format = descriptor?.value;
  if (!(error instanceof Error) || !descriptor?.writable || typeof format !== "function") return error.stack;

  let captured;
  // V8 formats lazily. Keep the caller's formatter and formatted stack intact.
  Error.prepareStackTrace = function (subject, sites) {
    if (subject === error) captured = sites;
    return Reflect.apply(format, this, [subject, sites]);
  };
  try {
    const formatted = error.stack;
    return captured ?? formatted;
  } finally {
    Error.prepareStackTrace = format;
  }
}

function structuredFrame(frame) {
  if (typeof frame.getFileName !== "function") return frame;
  return {
    functionName: frame.getFunctionName() ?? undefined,
    fileName: frame.getFileName() ?? frame.getScriptNameOrSourceURL?.(),
    lineNumber: frame.getLineNumber(),
    columnNumber: frame.getColumnNumber(),
  };
}

function originalLocation(map, line, column, sources) {
  if (!map) return;
  try {
    const original = originalPositionFor(map, { line, column: column - 1 });
    if (original.source == null) return;
    return sources.utf16(original.source, original.line, original.column + 1, {
      content: sourceContentFor(map, original.source),
    });
  } catch {
    // Mapping data may only be decoded when the first lookup is made.
  }
}

/**
 * Read an Error-like object with an engine-native stack. Maps are resolved once
 * per generated file. Browser callers can pass a local copy through loadMap.
 * @returns {import("./contract.mjs").Diagnostic}
 */
export function readJavaScriptError(error, { cwd = process.cwd(), frames: captured, loadMap, remap = true } = {}) {
  const sources = sourceLocations(cwd);
  const maps = new Map();
  const frames = [];
  const input = captured ?? stackData(error);
  const stack = Array.isArray(input) ? input.map(structuredFrame)
    : typeof input === "string" && input.trim() ? ErrorStackParser.parse({ stack: input }) : [];
  for (const { functionName, fileName, lineNumber, columnNumber } of stack) {
    if (!fileName || !lineNumber) {
      if (functionName) frames.push({ functionName });
      continue;
    }
    const file = fileIdentity(fileName, cwd);
    const frame = {
      functionName,
      location: columnNumber
        ? sources.utf16(file, lineNumber, columnNumber)
        : { file, line: lineNumber },
    };
    if (remap && columnNumber) {
      if (!maps.has(file)) maps.set(file, resolveMap(file, loadMap, sources.source));
      const original = originalLocation(maps.get(file), lineNumber, columnNumber, sources);
      if (original) {
        frame.generated = frame.location;
        frame.location = original;
      }
    }
    frames.push(frame);
  }
  const primary = frames.find(({ location }) => location)?.location;
  return nativeDiagnostic({
    source: "javascript", phase: "runtime", severity: "error",
    message: error.message ?? String(error),
    locations: primary ? [primary] : [], evidence: [],
    traces: frames.length ? [{ frames }] : [],
  }, sources);
}
