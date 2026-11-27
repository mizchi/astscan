import { execFileSync } from "node:child_process";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

export function scan(astscan, cwd, rule, paths) {
  const rulePath = fileURLToPath(new URL(rule, import.meta.url));
  const stdout = execFileSync(resolve(astscan), ["scan", "--rule", rulePath, "--json=slim", "--", ...paths], {
    cwd,
    encoding: "utf8",
  });
  return stdout.split("\n").filter(Boolean).map((line) => JSON.parse(line));
}

export function capture(match, name) {
  const range = match.metaVariables?.single?.[name]?.range.byteOffset;
  if (!range) throw new Error(`Missing ${name} capture in ${match.file}`);
  return range;
}
