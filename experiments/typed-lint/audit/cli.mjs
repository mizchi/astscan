import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { createTypeScriptAudit } from "./index.mjs";

const [binary, tsconfig, candidateRules, policyFile, ...inputs] = process.argv.slice(2);
if (!policyFile) throw new Error("Usage: node audit/cli.mjs <astscan> <tsconfig> <candidates.yml> <policy.json> [<project-relative-path>...]");
const policy = JSON.parse(readFileSync(policyFile, "utf8"));
const stdout = execFileSync(resolve(binary), ["scan", "--rule", resolve(candidateRules), "--json=slim", "--fail-on-skipped", "--", ...inputs], {
  cwd: dirname(resolve(tsconfig)), encoding: "utf8", maxBuffer: 32 * 1024 * 1024,
});
const matches = stdout.split("\n").filter(Boolean).map((line) => JSON.parse(line));
const audit = createTypeScriptAudit(tsconfig);
try {
  const result = audit.check("cli", matches, policy, { scanComplete: true });
  console.log(JSON.stringify(result, null, 2));
  process.exitCode = result.coverage.state === "partial" ? 2 : result.diagnostics.length > 0 ? 1 : 0;
} finally {
  audit.close();
}
