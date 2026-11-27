import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { diagnosticsInRanges } from "./diagnostics/contract.mjs";
import { createGoAdapter, readGoPanic } from "./diagnostics/go.mjs";
import { readJavaScriptError } from "./diagnostics/javascript.mjs";
import { createRustAdapter, readRustPanic } from "./diagnostics/rust.mjs";
import { createTypeScriptDiagnostics } from "./diagnostics/typescript.mjs";
import { scan } from "./scan.mjs";

function fixture(t) {
  const cwd = realpathSync(mkdtempSync(join(tmpdir(), "astscan-diagnostic-")));
  t.after(() => rmSync(cwd, { recursive: true, force: true }));
  const write = (name, text) => {
    const file = join(cwd, name);
    mkdirSync(dirname(file), { recursive: true });
    writeFileSync(file, text);
    return file;
  };
  return { cwd, write };
}

function touched(diagnostic, file, source, token) {
  const start = Buffer.byteLength(source.slice(0, source.indexOf(token)));
  const changed = [{ file, start, end: start + Buffer.byteLength(token) }];
  assert.deepEqual(diagnosticsInRanges([diagnostic], changed), [diagnostic]);
  assert.equal(diagnostic.locations[0].bytes.start, start);
}

test("tsrs keeps nested explanations and links back to the expected type", (t) => {
  const { write } = fixture(t);
  const declaration = write("types.ts", "export interface Target { tag: number }\n");
  const source = '\ufeffimport { Target } from "./types";\nconst emoji = "🙂"; const value: Target = { tag: "bad" };\n' +
    'const nested: { payload: { tag: number } } = {} as { payload: { tag: string } };\n';
  const file = write("main.ts", source);
  const config = write("tsconfig.json", '{"compilerOptions":{"strict":true},"files":["main.ts"]}');
  const adapter = createTypeScriptDiagnostics(config);
  t.after(() => adapter.close());

  const batch = adapter.check("v1");
  const [linked, nested] = batch.diagnostics.filter(({ code }) => code === "2322");
  assert.equal(linked.phase, "compile");
  assert.equal(linked.severity, 1);
  assert.equal(linked.uri, pathToFileURL(file).href);
  assert.equal(linked.range.start.line, 1);
  assert.equal(linked.relatedInformation[0].location.uri, pathToFileURL(declaration).href);
  touched(linked, file, source, 'tag: "bad"');
  assert.equal(linked.evidence.find(({ kind }) => kind === "related").locations[0].file, declaration);
  const explanation = nested.evidence.find(({ kind }) => kind === "explanation");
  assert.match(explanation.message, /payload.tag/);
  assert.match(explanation.evidence[0].message, /string.*number/);
  assert.equal(adapter.check("v1"), batch);
  write("types.ts", "export interface Target { tag: string }\n");
  assert.equal(adapter.check("v2").diagnostics.filter(({ code }) => code === "2322").length, 1);
});

test("Go type diagnostics retain byte points and related declarations", (t) => {
  const { cwd, write } = fixture(t);
  write("go.mod", "module example.com/fixture\n\ngo 1.24\n");
  const source = 'package main\nimport "fmt"\nvar duplicate int\nvar duplicate string\n' +
    'func take(n int) {}\nfunc main() { fmt.Println("🙂"); take("bad") }\n';
  const file = write("main.go", source);
  const adapter = createGoAdapter({ cwd });
  const result = adapter.check("v1");
  const mismatch = result.diagnostics.find(({ message }) => message.includes("cannot use"));
  touched(mismatch, file, source, '"bad"');
  const duplicate = result.diagnostics.find(({ message }) => message.includes("redeclared"));
  assert.equal(duplicate.evidence[0].kind, "related");
  assert.equal(duplicate.evidence[0].locations[0].line, 3);
  const fullBuild = createGoAdapter({ cwd, mode: "build" }).check("v1");
  touched(fullBuild.diagnostics.find(({ message }) => message.includes("cannot use")), file, source, '"bad"');
  write("main.go", 'package main\nfunc fail() { panic("boom\\ncontext") }\nfunc main() { fail() }\n');
  assert.deepEqual(adapter.check("v2").diagnostics, []);
  const run = spawnSync("go", ["run", "."], { cwd, encoding: "utf8" });
  assert.ok(run.status > 0, run.stderr);
  const panic = readGoPanic(run.stderr, { cwd });
  assert.equal(panic.message, "boom\ncontext");
  assert.deepEqual(panic.traces[0].frames.slice(0, 2).map(({ functionName }) => functionName), ["main.fail", "main.main"]);
  assert.equal(panic.locations[0].file, file);
});

test("Cargo diagnostics retain rustc spans and runtime panic frames use the same shape", (t) => {
  const { cwd, write } = fixture(t);
  write("Cargo.toml", '[workspace]\nmembers = ["member"]\nresolver = "2"\n');
  write("member/Cargo.toml", '[package]\nname = "fixture"\nversion = "0.0.0"\nedition = "2021"\n');
  const source = '\ufefffn main() {\r\n    let s = String::from("🙂");\r\n    drop(s); println!("{s}");\r\n}\r\n';
  const file = write("member/src/main.rs", source);
  const member = join(cwd, "member");
  const adapter = createRustAdapter({ cwd: member, cargoArgs: ["--offline"] });
  const result = adapter.check("v1");
  const moved = result.diagnostics.find(({ code }) => code === "E0382");
  const start = Buffer.byteLength(source.slice(0, source.indexOf("{s}") + 1));
  assert.deepEqual(moved.locations[0].bytes, { start, end: start + 1 });
  assert.deepEqual(diagnosticsInRanges([moved], [{ file, start, end: start + 1 }]), [moved]);
  assert.match(moved.evidence.find(({ kind }) => kind === "related").message, /moved here/);
  assert.ok(moved.evidence.some(({ kind }) => kind === "help"));
  write("member/src/main.rs", 'fn fail() { panic!("boom"); }\nfn main() { fail(); }\n');
  assert.deepEqual(adapter.check("v2").diagnostics, []);
  const run = spawnSync("cargo", ["run", "--offline", "--quiet"], {
    cwd: member,
    env: { ...process.env, RUST_BACKTRACE: "1" },
    encoding: "utf8",
  });
  assert.ok(run.status > 0, run.stderr);
  const panic = readRustPanic(run.stderr, { cwd });
  assert.equal(panic.message, "boom");
  assert.equal(panic.locations[0].file, file);
  assert.ok(panic.traces[0].frames.some(({ functionName }) => functionName === "fixture::fail"));
});

test("live JS stacks trace external and inline maps back to TypeScript", async (t) => {
  const { cwd, write } = fixture(t);
  const source = 'function fail(message: string) { throw new Error(message); }\nfail("boom");\n';
  const original = write("src/source.ts", source);
  const output = stripTypeScriptTypes(source, { mode: "transform", sourceMap: true, sourceUrl: original });
  const [, generatedCode, encodedMap] = output.match(/^(.*)\/\/# sourceMappingURL=data:application\/json;base64,(\S+)\s*$/s);
  const map = { ...JSON.parse(Buffer.from(encodedMap, "base64")), sources: ["../src/source.ts"], sourcesContent: [source] };
  write("dist/bundle.js.map", JSON.stringify(map));
  const inline = Buffer.from(JSON.stringify(map)).toString("base64");
  for (const [name, reference] of [
    ["external", "bundle.js.map"],
    ["inline", `data:application/json;base64,${inline}`],
    ["bad", "missing.js.map"],
  ]) {
    const generated = write(`dist/${name}.mjs`, `${generatedCode}//# sourceMappingURL=${reference}\n`);
    let thrown;
    try {
      await import(pathToFileURL(generated));
    } catch (error) {
      thrown = error;
    }
    assert.equal(thrown.message, "boom");
    const diagnostic = readJavaScriptError(thrown, { cwd });
    const [fail, caller] = diagnostic.traces[0].frames;
    assert.equal(fail.functionName, "fail");
    if (name === "bad") {
      assert.equal(fail.location.file, generated);
      assert.equal(fail.generated, undefined);
    } else {
      assert.equal(fail.generated.file, generated);
      assert.equal(fail.location.file, original);
      assert.equal(fail.location.line, 1);
      assert.equal(caller.location.line, 2);
      assert.ok(fail.location.bytes.start > fail.generated.bytes.start);
    }
  }
});

test("browser stack frames can use caller-provided maps and embedded source", () => {
  const diagnostic = readJavaScriptError(
    { message: "boom", stack: "fail@https://example.test/dist/bundle.js:1:1" },
    {
      loadMap: (file) => {
        assert.equal(file, "https://example.test/dist/bundle.js");
        return {
          url: "https://example.test/dist/bundle.js.map",
          map: {
            version: 3,
            sections: [{
              offset: { line: 0, column: 0 },
              map: { version: 3, sources: ["../src/app.ts"], names: [], mappings: "AAAE", sourcesContent: ["🙂throw"] },
            }],
          },
        };
      },
    },
  );
  const at = { line: 0, character: 2 };
  assert.deepEqual(diagnostic.locations[0], {
    file: "https://example.test/src/app.ts", line: 1, column: 3,
    range: { start: at, end: at }, bytes: { start: 4, end: 4 },
  });
  assert.deepEqual(diagnostic.range, { start: at, end: at });
  assert.equal(diagnostic.traces[0].frames[0].generated.column, 1);
});

test("live V8 CallSites take precedence over a custom stack formatter", () => {
  const previous = Error.prepareStackTrace;
  const format = () => "a string with no frame information";
  Error.prepareStackTrace = format;
  try {
    const fail = () => new Error("boom");
    const error = fail();
    const diagnostic = readJavaScriptError(error);
    assert.equal(diagnostic.traces[0].frames[0].functionName, "fail");
    assert.equal(diagnostic.locations[0].file, realpathSync(new URL(import.meta.url)));
    assert.equal(error.stack, "a string with no frame information");
    assert.equal(Error.prepareStackTrace, format);
  } finally {
    Error.prepareStackTrace = previous;
  }
});

test("Go reports physical byte positions for source with a line directive", (t) => {
  const { cwd, write } = fixture(t);
  write("go.mod", "module example.com/fixture\n\ngo 1.24\n");
  const source = '//line generated.go:100\npackage fixture\nvar bad int = "🙂"\n';
  const file = write("actual.go", source);
  const compiler = createGoAdapter({ cwd });
  t.after(() => compiler.close());
  const { diagnostics } = compiler.check("v1");
  touched(diagnostics.find(({ message }) => message.includes("cannot use")), file, source, '"🙂"');

  const broken = '//line generated.go:100\npackage fixture\nvar bad = )\n';
  write("actual.go", broken);
  const syntax = compiler.check("v2").diagnostics.find(({ message }) => message.includes("expected operand"));
  touched(syntax, file, broken, ")");
});

test("a Go recovery can export native frames without a panic log", (t) => {
  const { cwd, write } = fixture(t);
  const module = "github.com/mizchi/astscan/experiments/typed-lint/diagnostics/go-native";
  const helper = fileURLToPath(new URL("./diagnostics/go-native/", import.meta.url));
  write("go.mod", `module example.com/native\n\ngo 1.26.0\n\nrequire ${module} v0.0.0\nreplace ${module} => ${JSON.stringify(helper)}\n`);
  const file = write("main.go", `package main
import ("encoding/json"; "os"; "${module}/panictrace")
func fail() { panic("native go") }
func main() {
  defer func() { if value := recover(); value != nil { json.NewEncoder(os.Stdout).Encode(panictrace.Capture(value)) } }()
  fail()
}
`);
  const proc = spawnSync("go", ["run", "."], { cwd, encoding: "utf8" });
  assert.equal(proc.status, 0, proc.stderr);
  const diagnostic = readGoPanic(JSON.parse(proc.stdout), { cwd });
  assert.equal(diagnostic.message, "native go");
  assert.equal(diagnostic.locations[0].file, file);
  assert.equal(diagnostic.locations[0].line, 3);
  assert.equal(diagnostic.traces[0].frames[0].functionName, "main.fail");
});

test("a Rust panic hook exports the panic location and resolved frames as data", (t) => {
  const { cwd, write } = fixture(t);
  const helper = fileURLToPath(new URL("./diagnostics/rust-panic/", import.meta.url));
  write("Cargo.toml", `[package]
name = "native_fixture"
version = "0.0.0"
edition = "2024"
[dependencies]
astscan-panic-trace = { path = ${JSON.stringify(helper)} }
serde_json = "1"
`);
  const file = write("src/main.rs", `fn fail() { panic!("native rust"); }
fn main() {
    std::panic::set_hook(Box::new(|info| {
        println!("{}", serde_json::to_string(&astscan_panic_trace::capture(info)).unwrap());
    }));
    fail();
}
`);
  const proc = spawnSync("cargo", ["run", "--quiet"], { cwd, encoding: "utf8" });
  assert.equal(proc.status, 101, proc.stderr);
  const diagnostic = readRustPanic(JSON.parse(proc.stdout), { cwd });
  assert.equal(diagnostic.message, "native rust");
  assert.equal(diagnostic.locations[0].file, file);
  assert.equal(diagnostic.locations[0].line, 1);
  assert.ok(diagnostic.traces[0].frames.some(({ functionName }) => functionName?.includes("native_fixture::fail")));
});

test("slim astscan matches join compiler diagnostics by file and byte span", { skip: !process.env.ASTSCAN }, (t) => {
  const { cwd, write } = fixture(t);
  const config = write("tsconfig.json", '{"compilerOptions":{"strict":true},"files":["main.ts"]}');
  write("main.ts", 'const good: string = "🙂";\nconst bad: number = "text";\n');
  const rule = write("rule.yml", "id: variable\nlanguage: TypeScript\nseverity: hint\nrule: { kind: variable_declarator }\n");
  const matches = scan(process.env.ASTSCAN, cwd, rule, ["main.ts"]);
  assert.equal(matches.length, 2);
  const compiler = createTypeScriptDiagnostics(config);
  t.after(() => compiler.close());
  const { diagnostics } = compiler.check("v1");
  const atMatch = ({ file, range }) => [{ file, ...range.byteOffset }];
  assert.deepEqual(diagnosticsInRanges(diagnostics, atMatch(matches[0]), { cwd }), []);
  assert.deepEqual(diagnosticsInRanges(diagnostics, atMatch(matches[1]), { cwd }).map(({ code }) => code), ["2322"]);
});
