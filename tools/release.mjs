import { execFileSync } from "node:child_process";
import { appendFileSync, readFileSync, writeFileSync } from "node:fs";

const [tag, notesPath, registry, ...extra] = process.argv.slice(2);
if (!/^v\d+\.\d+\.\d+(?:-[\w.-]+)?$/.test(tag) || !notesPath || (registry && registry !== "--registry") || extra.length) {
  throw new Error("Usage: node tools/release.mjs v<version> <notes-path> [--registry]");
}

const version = tag.slice(1);
const crates = [["astscan-core", "publish_core"], ["astscan", "publish_cli"]];
const { packages } = JSON.parse(execFileSync("cargo", ["metadata", "--format-version", "1", "--no-deps", "--locked"]));
for (const [name] of crates) {
  const crate = packages.find((item) => item.name === name);
  if (crate?.version !== version) {
    throw new Error(`${name} is ${crate?.version ?? "missing"}, expected ${version}`);
  }
}

const summary = readFileSync(`.github/releases/${tag}.md`, "utf8").trim();
const changelog = readFileSync("CHANGELOG.md", "utf8");
const section = changelog.split(/^## /m).slice(1).find((entry) => {
  const heading = entry.split("\n", 1)[0].trimEnd();
  return heading === version || heading.startsWith(`${version} (`);
});
const changes = section?.split("\n").slice(1).join("\n").trim();
if (!summary || !changes) throw new Error(`Missing release summary or changelog entries for ${tag}`);
writeFileSync(notesPath, `${summary}\n\n## Changes\n\n${changes}\n`);

if (registry === "--registry") {
  const states = await Promise.all(crates.map(async ([name, output]) => {
    const response = await fetch(`https://index.crates.io/as/ts/${name}`, { signal: AbortSignal.timeout(15_000) });
    if (response.status !== 404 && !response.ok) {
      throw new Error(`crates.io index returned HTTP ${response.status} for ${name}`);
    }
    const entry = response.ok
      ? (await response.text()).trim().split("\n").map((line) => JSON.parse(line)).find(({ vers }) => vers === version)
      : undefined;
    if (entry?.yanked) throw new Error(`${name} ${version} is yanked on crates.io`);
    return `${output}=${!entry}\n`;
  }));
  if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, states.join(""));
  else process.stdout.write(states.join(""));
}
