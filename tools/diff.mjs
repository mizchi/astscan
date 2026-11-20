#!/usr/bin/env node
// Differential test: run ast-grep and astscan with the same rule file over the
// same paths and compare every match they report, field by field.
//
//   node tools/diff.mjs --rule rules.yml [--config sgconfig.yml] [--cwd dir] [--] paths...
//
// The reference binary comes from AST_GREP (default `ast-grep`), the candidate
// from ASTSCAN (default target/release/astscan), with ASTSCAN_ARGS added to its
// options. Exits 1 on any difference.
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createInterface } from "node:readline";
import { resolve } from "node:path";

const argv = process.argv.slice(2);
const opt = { rule: null, config: null, cwd: process.cwd(), paths: [], show: 5 };
for (let i = 0; i < argv.length; i++) {
  const a = argv[i];
  if (a === "--rule") opt.rule = resolve(argv[++i]);
  else if (a === "--config") opt.config = resolve(argv[++i]);
  else if (a === "--cwd") opt.cwd = resolve(argv[++i]);
  else if (a === "--show") opt.show = Number(argv[++i]);
  else if (a === "--") { opt.paths.push(...argv.slice(i + 1)); break; }
  else opt.paths.push(a);
}
if (!opt.rule) { console.error("--rule is required"); process.exit(2); }
if (opt.paths.length === 0) opt.paths.push(".");

const reference = process.env.AST_GREP ?? "ast-grep";
const candidate = resolve(process.env.ASTSCAN ?? "target/release/astscan");

// Object key order is not part of either tool's contract: hash maps print in any order.
const canonical = (v) => Array.isArray(v) ? v.map(canonical)
  : v && typeof v === "object" ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, canonical(v[k])]))
  : v;

// Extra options for astscan only, e.g. ASTSCAN_ARGS="--parser=fast".
const candidateOptions = (process.env.ASTSCAN_ARGS ?? "").split(" ").filter(Boolean);

function run(bin, options = []) {
  return new Promise((done, fail) => {
    const args = ["scan", ...(opt.config ? ["-c", opt.config] : []), "--rule", opt.rule, "--json=stream", ...options, "--", ...opt.paths];
    const started = performance.now();
    const child = spawn(bin, args, { cwd: opt.cwd, stdio: ["ignore", "pipe", "pipe"] });
    const seen = new Map();
    let stderr = "", lines = 0;
    child.stderr.on("data", (d) => { stderr += d; });
    createInterface({ input: child.stdout, crlfDelay: Infinity }).on("line", (line) => {
      if (line === "") return;
      lines += 1;
      const j = JSON.parse(line);
      const key = `${j.file}\t${j.ruleId}\t${j.range.byteOffset.start}-${j.range.byteOffset.end}`;
      const hash = createHash("sha1").update(JSON.stringify(canonical(j))).digest("base64");
      // The same (file, rule, range) twice is itself a finding, so keep every hash.
      seen.set(key, seen.has(key) ? `${seen.get(key)}+${hash}` : hash);
    }).on("close", () => {
      child.on("close", (code) => done({ seen, lines, code, stderr, ms: performance.now() - started }));
      if (child.exitCode !== null) done({ seen, lines, code: child.exitCode, stderr, ms: performance.now() - started });
    });
    child.on("error", fail);
  });
}

const [a, b] = [await run(reference), await run(candidate, candidateOptions)];
console.log(`reference ${reference}: ${a.lines} matches, exit ${a.code}, ${a.ms.toFixed(0)}ms (incl. reading)`);
console.log(`candidate ${candidate}: ${b.lines} matches, exit ${b.code}, ${b.ms.toFixed(0)}ms (incl. reading)`);

const missing = [], extra = [], changed = [];
for (const [k, h] of a.seen) { if (!b.seen.has(k)) missing.push(k); else if (b.seen.get(k) !== h) changed.push(k); }
for (const k of b.seen.keys()) if (!a.seen.has(k)) extra.push(k);

const byRule = (keys) => {
  const m = new Map();
  for (const k of keys) { const r = k.split("\t")[1]; m.set(r, (m.get(r) ?? 0) + 1); }
  return [...m].sort((x, y) => y[1] - x[1]).map(([r, n]) => `    ${n}\t${r}`).join("\n");
};
const report = (name, keys) => {
  if (keys.length === 0) return;
  console.log(`\n${name}: ${keys.length}\n${byRule(keys)}`);
  for (const k of keys.slice(0, opt.show)) console.log(`  e.g. ${k.replaceAll("\t", "  ")}`);
};
report("missing (ast-grep reports, astscan does not)", missing);
report("extra (astscan reports, ast-grep does not)", extra);
report("changed (same match, different content)", changed);
if (a.code !== b.code) console.log(`\nexit codes differ: ${a.code} vs ${b.code}`);
const ok = missing.length + extra.length + changed.length === 0 && a.code === b.code && a.lines === b.lines;
console.log(ok ? "\nidentical" : "\nDIFFERENT");
process.exit(ok ? 0 : 1);
