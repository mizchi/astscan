//! Black-box tests: a rule file and a source tree in, JSON lines out.
//!
//! The expected values were read off ast-grep 0.45.3 for the same input;
//! `tools/diff.mjs` is the test that compares the two tools wholesale.

use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct Case {
  dir: PathBuf,
}

struct Run {
  code: i32,
  lines: Vec<Value>,
  stderr: String,
}

impl Case {
  fn new(name: &str, rules: &str, files: &[(&str, &str)]) -> Self {
    Self::new_in(Path::new(env!("CARGO_TARGET_TMPDIR")).join(name), rules, files)
  }

  fn new_in(dir: PathBuf, rules: &str, files: &[(&str, &str)]) -> Self {
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("rules.yml"), rules).unwrap();
    for (path, text) in files {
      let path = dir.join("src").join(path);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(path, text).unwrap();
    }
    Case { dir }
  }

  fn run(&self, json: &str) -> Run {
    self.run_with(json, &[])
  }

  fn run_with(&self, json: &str, options: &[&str]) -> Run {
    self.run_paths(json, options, &["src"])
  }

  fn run_paths(&self, json: &str, options: &[&str], paths: &[&str]) -> Run {
    let output = format!("--json={json}");
    self.run_scan(["--rule", "rules.yml", &output].into_iter().chain(options.iter().copied()), paths, None)
  }

  fn run_scan<'a>(&self, options: impl IntoIterator<Item = &'a str>, paths: &[&str], stdin: Option<&str>) -> Run {
    let mut command = self.command("scan");
    command.args(options).arg("--").args(paths);
    let out = output(command, stdin);
    let mut lines: Vec<Value> = String::from_utf8(out.stdout).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    lines.sort_by_key(|l| {
      (l["file"].as_str().map(str::to_string), l["range"]["byteOffset"]["start"].as_u64(), l["ruleId"].as_str().map(str::to_string))
    });
    Run { code: out.status.code().unwrap(), lines, stderr: String::from_utf8(out.stderr).unwrap() }
  }

  fn command(&self, subcommand: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_astscan"));
    command.current_dir(&self.dir).arg(subcommand);
    command
  }
}

fn output(mut command: Command, stdin: Option<&str>) -> std::process::Output {
  if let Some(input) = stdin {
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
  } else {
    command.output().unwrap()
  }
}

fn rule(id: &str, language: &str, body: &str) -> String {
  format!("id: {id}\nlanguage: {language}\nmessage: m\nseverity: hint\n{body}\n")
}

fn texts(run: &Run) -> Vec<&str> {
  run.lines.iter().map(|l| l["text"].as_str().unwrap()).collect()
}

#[test]
fn debug_infers_the_grammar_and_reports_ast_fields_and_utf8_offsets() {
  let source = "const x: string = \"日本語\";";
  let case = Case::new("debug-typescript", "", &[("a.ts", source)]);
  let text = case.command("debug").arg("src/a.ts").output().unwrap();
  assert!(text.status.success(), "{}", String::from_utf8_lossy(&text.stderr));
  let sexp = String::from_utf8(text.stdout).unwrap();
  assert!(sexp.contains("type: (type_annotation"), "{sexp}");
  assert!(sexp.contains("value: (string"), "{sexp}");

  let json = case.command("debug").args(["--json", "src/a.ts"]).output().unwrap();
  assert!(json.status.success(), "{}", String::from_utf8_lossy(&json.stderr));
  let ast: Value = serde_json::from_slice(&json.stdout).unwrap();
  assert_eq!(ast["language"], "TypeScript");
  assert_eq!(ast["hasErrors"], false);
  let nodes = ast["nodes"].as_array().unwrap();
  assert_eq!(nodes[0]["kind"], "program");
  assert_eq!(nodes[0]["parent"], Value::Null);
  let string = nodes.iter().find(|node| node["kind"] == "string" && node["named"] == true).unwrap();
  assert_eq!(string["field"], "value");
  let parent = string["parent"].as_u64().unwrap() as usize;
  assert_eq!(nodes[parent]["kind"], "variable_declarator");
  assert_eq!(nodes[parent]["id"], parent);
  let start = string["startByte"].as_u64().unwrap() as usize;
  let end = string["endByte"].as_u64().unwrap() as usize;
  assert_eq!(&source[start..end], "\"日本語\"");
  assert!(nodes.iter().any(|node| node["kind"] == "string" && node["named"] == false));
  assert!(nodes.iter().any(|node| node["kind"] == ":" && node["named"] == false));
}

#[test]
fn debug_accepts_stdin_and_exposes_syntax_errors() {
  let case = Case::new("debug-stdin", "", &[]);
  let missing = case.command("debug").arg("-").output().unwrap();
  assert_eq!(missing.status.code(), Some(2));
  assert!(String::from_utf8_lossy(&missing.stderr).contains("`--lang` is required for stdin"));

  let mut command = case.command("debug");
  command.args(["-l", "js", "--json", "-"]);
  let result = output(command, Some("let ="));
  assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
  let ast: Value = serde_json::from_slice(&result.stdout).unwrap();
  assert_eq!(ast["language"], "JavaScript");
  assert_eq!(ast["hasErrors"], true);
  assert!(ast["nodes"].as_array().unwrap().iter().any(|node| node["kind"] == "ERROR"));
}

#[test]
fn patterns_and_cypher_can_be_batched_on_the_command_line() {
  let case = Case::new("direct-queries", "", &[("a.js", "console.log(\"hello\");\ndebugger;\n")]);
  let options = ["-l", "js", "-p", "console.log($ARG)", "--cypher", "MATCH (d:debugger_statement) RETURN d", "--cache", "cache", "--stats"];
  let run = case.run_scan(options, &["src"], None);
  assert_eq!(run.code, 0, "{}", run.stderr);
  assert_eq!(texts(&run), ["console.log(\"hello\")", "debugger;"]);
  assert_eq!(run.lines[0]["ruleId"], "pattern-1");
  assert_eq!(run.lines[0]["metaVariables"]["single"]["ARG"]["text"], "\"hello\"");
  assert_eq!(run.lines[1]["ruleId"], "cypher-2");
  assert!(run.stderr.contains("2 rules in 2 groups, 1 files"), "{}", run.stderr);
  let warm = case.run_scan(options, &["src"], None);
  assert_eq!(warm.lines, run.lines);
  assert_eq!(stat(&warm, "cache:"), "1 hits, 0 misses");
  let mut changed = options;
  changed[3] = "alert($ARG)";
  let changed = case.run_scan(changed, &["src"], None);
  assert_eq!(texts(&changed), ["debugger;"]);
  assert_eq!(stat(&changed, "cache:"), "0 hits, 1 misses");
}

#[test]
fn json_batch_is_equivalent_to_yaml_file_for_inline_and_stdin_inputs() {
  let yaml = [
    "id: numeric-log\nlanguage: JavaScript\nseverity: error\nrule: { pattern: 'console.log($ARG)' }\nconstraints: { ARG: { kind: number } }\n",
    "id: go-fn\nlanguage: Go\ncypher: 'MATCH (f:function_declaration) RETURN f'\n",
  ]
  .join("---\n");
  let batch = json!([
    {
      "id": "numeric-log", "language": "JavaScript", "severity": "error",
      "rule": {"pattern": "console.log($ARG)"}, "constraints": {"ARG": {"kind": "number"}}
    },
    {"id": "go-fn", "language": "Go", "cypher": "MATCH (f:function_declaration) RETURN f"},
  ])
  .to_string();
  let case = Case::new("inline-json", &yaml, &[("a.js", "console.log(1); console.log(x);"), ("b.go", "package p\nfunc f() {}\n")]);
  let file = case.run("stream");
  assert_eq!(file.code, 1);
  assert_eq!(texts(&file), ["console.log(1)", "func f() {}"]);
  std::fs::remove_file(case.dir.join("rules.yml")).unwrap();
  for run in [
    case.run_scan(["--inline-rules", &batch], &["src"], None),
    case.run_scan(["--rule", "-"], &["src"], Some(&batch)),
    case.run_scan(["--rule", "-"], &["src"], Some(&yaml)),
  ] {
    assert_eq!(run.code, file.code, "{}", run.stderr);
    assert_eq!(run.lines, file.lines);
  }
}

#[test]
fn inline_queries_require_a_language_and_exclusive_rule_input() {
  let case = Case::new("inline-arguments", "", &[]);
  let missing = case.run_scan(["--pattern", "x"], &["src"], None);
  assert_eq!(missing.code, 2);
  assert!(missing.stderr.contains("`--lang` is required"), "{}", missing.stderr);
  let mixed = case.run_scan(["--rule", "rules.yml", "--inline-rules", "[]"], &["src"], None);
  assert_eq!(mixed.code, 2);
  assert!(mixed.stderr.contains("choose one rule input"), "{}", mixed.stderr);
}

#[test]
fn pattern_captures_single_and_multi_metavariables() {
  let case = Case::new(
    "pattern",
    &rule("log", "TypeScript", "rule:\n  pattern: console.log($A, $$$REST)"),
    &[("a.ts", "console.log(one, two, three);\nconsole.log(solo);\nconsole.warn(x, y);\n")],
  );
  let run = case.run("stream");
  assert_eq!(texts(&run), ["console.log(one, two, three)"]);
  let vars = &run.lines[0]["metaVariables"];
  assert_eq!(vars["single"]["A"]["text"], "one");
  let rest: Vec<&str> = vars["multi"]["REST"].as_array().unwrap().iter().map(|n| n["text"].as_str().unwrap()).collect();
  assert_eq!(rest, ["two", ",", "three"]);
}

#[test]
fn added_languages_match_patterns_and_capture_from_directory_inputs() {
  let samples = [
    ("a.mbt", "moonbit", "MoonBit", "fn $VALUE() -> Int { 42 }", "fn answer() -> Int { 42 }\n", "answer"),
    ("a.zig", "zig", "Zig", "const $VALUE = @import(\"std\");", "const std = @import(\"std\");\n", "std"),
    ("a.toml", "toml", "Toml", "$VALUE = 1", "enabled = 1\nsize = 2\n", "enabled"),
    ("a.sql", "sql", "Sql", "SELECT id FROM $VALUE", "SELECT id FROM users;\n", "users"),
    ("a.xml", "xml", "Xml", "<$VALUE/>", "<root><widget/></root>\n", "widget"),
  ];
  let rules = samples
    .iter()
    .map(|(_, alias, name, pattern, _, _)| rule(name, alias, &format!("rule:\n  pattern: '{pattern}'")))
    .collect::<Vec<_>>()
    .join("---\n");
  let files = samples.iter().map(|&(file, _, _, _, source, _)| (file, source)).collect::<Vec<_>>();
  let case = Case::new("additional-languages", &rules, &files);
  let stream = case.run_with("stream", &["--fail-on-skipped"]);
  assert_eq!(stream.code, 0, "{}", stream.stderr);
  assert_eq!(stream.lines.len(), samples.len(), "{:?}", stream.lines);
  for (file, _, name, _, _, captured) in samples {
    let found = stream.lines.iter().find(|line| line["file"] == format!("src/{file}")).unwrap();
    assert_eq!(found["language"], name);
    assert_eq!(found["ruleId"], name);
    assert_eq!(found["metaVariables"]["single"]["VALUE"]["text"], captured);
  }
  let fast = case.run_with("stream", &["--parser=fast", "--fail-on-skipped"]);
  assert_eq!(fast.code, 0, "{}", fast.stderr);
  assert_eq!(fast.lines, stream.lines);
}

#[test]
fn skipped_inputs_are_reported_even_on_a_warm_cache() {
  let case = Case::new("scan-skips", &rule("log", "JavaScript", "rule:\n  pattern: console.log($X)"), &[("good.js", "console.log(1);\n")]);
  std::fs::write(case.dir.join("src/non-utf8.js"), b"console.log(2);\n// \xff").unwrap();
  std::fs::write(case.dir.join("src/large.js"), "console.log(3);\n".repeat(200_001)).unwrap();
  let paths = ["src/good.js", "src/non-utf8.js", "src/large.js"];
  let cold = case.run_paths("stream", &["--cache", "cache"], &paths);
  assert_eq!(cold.code, 0);
  assert_eq!(texts(&cold), ["console.log(1)"]);
  assert!(cold.stderr.contains("src/non-utf8.js: input is not UTF-8"), "{}", cold.stderr);
  assert!(cold.stderr.contains("src/large.js: input exceeds both"), "{}", cold.stderr);
  let warm = case.run_paths("stream", &["--cache", "cache", "--fail-on-skipped", "--stats"], &paths);
  assert_eq!(warm.code, 2, "{}", warm.stderr);
  assert_eq!(warm.lines, cold.lines);
  assert!(warm.stderr.contains("2 skipped inputs"), "{}", warm.stderr);
  assert!(warm.stderr.contains("cache: 1 hits, 2 misses"), "{}", warm.stderr);
}

#[test]
fn a_missing_path_is_reported_and_takes_priority_over_an_error_match() {
  let rules = rule("log", "JavaScript", "rule:\n  pattern: console.log($X)").replace("severity: hint", "severity: error");
  let case = Case::new("missing-input", &rules, &[("good.js", "console.log(1);")]);
  let paths = ["src/good.js", "src/missing.js"];
  let regular = case.run_paths("stream", &[], &paths);
  assert_eq!(regular.code, 1);
  assert!(regular.stderr.contains("src/missing.js"), "{}", regular.stderr);
  let strict = case.run_paths("stream", &["--fail-on-skipped"], &paths);
  assert_eq!(strict.code, 2);
  assert_eq!(strict.lines, regular.lines);
}

#[test]
fn a_repeated_metavariable_must_match_the_same_code() {
  let case =
    Case::new("backref", &rule("same", "JavaScript", "rule:\n  pattern: $A == $A"), &[("a.js", "x == x;\nx == y;\nf(1) == f(1);\n")]);
  assert_eq!(texts(&case.run("stream")), ["x == x", "f(1) == f(1)"]);
}

#[test]
fn stream_output_has_the_fields_of_ast_grep() {
  let case =
    Case::new("shape", &rule("call", "JavaScript", "rule:\n  pattern: foo($X)"), &[("a.js", "if (ok) {\n  é; foo(\"q\\\\\");\n}\n")]);
  let run = case.run("stream");
  assert_eq!(run.code, 0);
  let m = &run.lines[0];
  assert_eq!(m["text"], "foo(\"q\\\\\")");
  assert_eq!(m["file"], "src/a.js");
  assert_eq!(m["language"], "JavaScript");
  assert_eq!(m["lines"], "  é; foo(\"q\\\\\");");
  // Columns and counts are in characters: `é` is two bytes and one column.
  assert_eq!(m["range"]["byteOffset"]["start"], 16);
  assert_eq!(m["range"]["start"]["line"], 1);
  assert_eq!(m["range"]["start"]["column"], 5);
  assert_eq!(m["charCount"]["leading"], 5);
  assert_eq!(m["charCount"]["trailing"], 1);
  assert_eq!(m["ruleId"], "call");
  assert_eq!(m["severity"], "hint");
  assert_eq!(m["note"], Value::Null);
  assert_eq!(m["message"], "m");
  assert_eq!(m["labels"][0]["style"], "primary");
  assert_eq!(m["labels"][0]["text"], m["text"]);
  assert_eq!(m["metaVariables"]["transformed"], serde_json::json!({}));
}

#[test]
fn relational_rules_follow_stop_by_and_field() {
  let rules = [
    rule("named-fn", "TypeScript", "rule:\n  kind: function_declaration\n  has:\n    field: name\n    pattern: $NAME"),
    rule("await-in-fn", "TypeScript", "rule:\n  kind: await_expression\n  inside:\n    kind: function_declaration\n    stopBy: end"),
    rule("await-direct", "TypeScript", "rule:\n  kind: await_expression\n  inside:\n    kind: function_declaration"),
    rule("deep-number", "TypeScript", "rule:\n  kind: function_declaration\n  has:\n    kind: number\n    stopBy: end"),
    rule("after-comment", "TypeScript", "rule:\n  kind: function_declaration\n  follows:\n    kind: comment"),
    rule("before-fn", "TypeScript", "rule:\n  kind: comment\n  precedes:\n    kind: function_declaration\n    stopBy: end"),
  ]
  .join("---\n");
  let src = "// one\nconst a = 1;\n// two\nasync function f() { const v = await g(2); }\nfunction h() {}\n";
  let run = Case::new("relational", &rules, &[("a.ts", src)]).run("stream");
  let by = |id: &str| run.lines.iter().filter(|l| l["ruleId"] == id).map(|l| l["text"].as_str().unwrap()).collect::<Vec<_>>();
  assert_eq!(by("named-fn").len(), 2);
  assert_eq!(run.lines.iter().find(|l| l["ruleId"] == "named-fn").unwrap()["metaVariables"]["single"]["NAME"]["text"], "f");
  assert_eq!(by("await-in-fn"), ["await g(2)"]);
  assert!(by("await-direct").is_empty(), "the await is not a direct child of the function");
  assert_eq!(by("deep-number").len(), 1);
  assert_eq!(by("after-comment").len(), 1, "only `f` directly follows a comment");
  assert_eq!(by("before-fn"), ["// one", "// two"]);
  // A relational sub-rule's node is reported as a secondary label, as ast-grep does.
  let labelled = run.lines.iter().find(|l| l["ruleId"] == "await-in-fn").unwrap();
  assert_eq!(labelled["labels"][1]["style"], "secondary");
  assert!(labelled["metaVariables"]["multi"]["secondary"].is_array());
}

#[test]
fn cypher_nested_exists_filters_the_returned_ast_node() {
  let rules = [
    rule(
      "loop-without-await",
      "JavaScript",
      "cypher: |
  MATCH (f:function_declaration)
  WHERE EXISTS {
    MATCH (f)-[:DESCENDANT]->(l:for_statement)
    WHERE NOT EXISTS { MATCH (l)-[:DESCENDANT]->(:await_expression) }
  } AND EXISTS { MATCH (f)-[:CHILD]->(:identifier) }
    AND NOT EXISTS { MATCH (f)-[:CHILD]->(:await_expression) }
  RETURN f",
    ),
    rule("call", "JavaScript", "rule:\n  pattern: work()"),
  ]
  .join("---\n");
  let good = "async function good() { for (;;) { work(); } for (;;) { work(); } for (;;) { await work(); } }";
  let bad = "async function bad() { for (;;) { await work(); } }";
  let no_loop = "async function noLoop() { await work(); }";
  let input = format!("{no_loop}\n{bad}\n{good}\n");
  let case = Case::new("cypher-exists", &rules, &[("main.js", &input)]);
  let run = case.run_with("stream", &["--fail-on-skipped"]);
  assert_eq!(run.code, 0, "{}", run.stderr);
  let cypher: Vec<_> = run.lines.iter().filter(|l| l["ruleId"] == "loop-without-await").collect();
  assert_eq!(cypher.len(), 1);
  assert_eq!(cypher[0]["text"], good);
  assert_eq!(cypher[0]["labels"].as_array().unwrap().len(), 1);
  assert_eq!(run.lines.iter().filter(|l| l["ruleId"] == "call").count(), 5);
  let fast = case.run_with("stream", &["--parser=fast", "--fail-on-skipped"]);
  assert_eq!(fast.code, 0, "{}", fast.stderr);
  assert_eq!(fast.lines, run.lines);
}

#[test]
fn cypher_rejects_unsupported_bindings_and_mixed_matchers() {
  let cases = [
    ("MATCH (f:function_declaration) WHERE EXISTS { MATCH (f)-[:CHILD]->(f:identifier) } RETURN f", "already bound"),
    ("MATCH (f:function_declaration) WHERE EXISTS { MATCH (x)-[:CHILD]->(:identifier) } RETURN f", "enclosing variable"),
    ("MATCH (f:function_declaration) WHERE EXISTS { MATCH (f)-[:PARENT]->(:identifier) } RETURN f", "CHILD or DESCENDANT"),
    ("MATCH (f:function_declaration) RETURN x", "RETURN must use `f`"),
    ("MATCH (f:no_such_kind) RETURN f", "unknown AST kind `no_such_kind`"),
    ("MATCH (f:function_declaration) RETURN f\nrule: {kind: function_declaration}", "use either `rule` or `cypher`"),
    (
      "MATCH (call:call_expression) WHERE PATTERN(\"log($VALUE)\") RETURN call\nconstraints:\n  MISSING: {kind: identifier}",
      "constraint `MISSING` needs a single capture in PATTERN",
    ),
  ];
  for (index, (query, diagnostic)) in cases.into_iter().enumerate() {
    let rules = rule("bad-query", "JavaScript", &format!("cypher: |\n  {query}"));
    let case = Case::new(&format!("bad-cypher-{index}"), &rules, &[("main.js", "function f() {}")]);
    let run = case.run("stream");
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(run.lines.is_empty());
    assert!(run.stderr.contains("bad-query") && run.stderr.contains(diagnostic), "{}", run.stderr);
  }
}

#[test]
fn cypher_captures_arguments_and_checks_child_fields() {
  let rules = rule(
    "log-value",
    "TypeScript",
    r#"cypher: |
  MATCH (call:call_expression)
  WHERE PATTERN("$LOGGER($VALUE)")
    AND EXISTS { MATCH (call)-[:CHILD {field: "function"}]->(:identifier) }
  RETURN call
constraints:
  VALUE: { kind: identifier }"#,
  );
  let case = Case::new("cypher-captures", &rules, &[("main.ts", "log(é); log(1); logger.log(é); log(1, 2);")]);
  let run = case.run("stream");
  assert_eq!(run.code, 0, "{}", run.stderr);
  assert_eq!(texts(&run), ["log(é)"]);
  assert_eq!(run.lines[0]["metaVariables"]["single"]["VALUE"]["text"], "é");
}

#[test]
fn composite_rules_constraints_utils_and_nth_child() {
  let rules = [
    rule(
      "leveled",
      "JavaScript",
      "rule:\n  pattern: $LOGGER.$LEVEL($$$ARGS)\nconstraints:\n  LEVEL:\n    regex: ^(warn|error)$\n  LOGGER:\n    regex: (?i)log",
    ),
    rule(
      "util",
      "JavaScript",
      "rule:\n  all:\n    - matches: is-call\n    - not:\n        has:\n          kind: arguments\n          has:\n            kind: string\nutils:\n  is-call:\n    kind: call_expression",
    ),
    rule("first-arg", "JavaScript", "rule:\n  kind: identifier\n  nthChild: 1\n  inside:\n    kind: arguments"),
    rule("any-of", "JavaScript", "rule:\n  any:\n    - kind: string\n    - kind: number"),
  ]
  .join("---\n");
  let src = "logger.warn(a, b);\nlogger.info(c);\nconsole.error('x', 1);\n";
  let run = Case::new("composite", &rules, &[("a.js", src)]).run("stream");
  let by = |id: &str| run.lines.iter().filter(|l| l["ruleId"] == id).map(|l| l["text"].as_str().unwrap()).collect::<Vec<_>>();
  assert_eq!(by("leveled"), ["logger.warn(a, b)"]);
  assert_eq!(by("util"), ["logger.warn(a, b)", "logger.info(c)"]);
  assert_eq!(by("first-arg"), ["a", "c"]);
  assert_eq!(by("any-of"), ["'x'", "1"]);
}

#[test]
fn rules_with_one_matcher_run_once_and_report_every_id() {
  let rules = [
    rule("a@JavaScript", "JavaScript", "rule:\n  kind: debugger_statement"),
    rule("a@Jsx", "Jsx", "rule:\n  kind: debugger_statement"),
    rule("b@JavaScript", "JavaScript", "rule:\n  kind: number"),
  ]
  .join("---\n");
  let case = Case::new("groups", &rules, &[("a.js", "debugger;\n")]);
  let slim = case.run("slim");
  assert_eq!(slim.lines.len(), 1);
  assert_eq!(slim.lines[0]["ruleIds"], serde_json::json!(["a@JavaScript", "a@Jsx"]));
  assert_eq!(slim.lines[0]["language"], "JavaScript");
  assert!(slim.lines[0].get("text").is_none(), "slim output carries no source text");
  assert_eq!(slim.lines[0]["range"]["byteOffset"], serde_json::json!({"start": 0, "end": 9}));
  let stream = case.run("stream");
  let ids: Vec<&str> = stream.lines.iter().map(|l| l["ruleId"].as_str().unwrap()).collect();
  assert_eq!(ids, ["a@JavaScript", "a@Jsx"]);
}

#[test]
fn respects_gitignore_outside_a_repository() {
  let case = Case::new_in(
    std::env::temp_dir().join(format!("astscan-gitignore-test-{}", std::process::id())),
    &rule("dbg", "TypeScript", "rule:\n  kind: debugger_statement"),
    &[
      ("kept.ts", "debugger;\n"),
      ("generated/skipped.ts", "debugger;\n"),
      ("generated/kept.ts", "debugger;\n"),
      ("excluded/skipped.ts", "debugger;\n"),
    ],
  );
  std::fs::write(case.dir.join(".gitignore"), "src/generated/*\n!src/generated/kept.ts\nsrc/excluded/\n").unwrap();
  let run = case.run("stream");
  std::fs::remove_dir_all(&case.dir).unwrap();

  assert_eq!(run.code, 0, "{}", run.stderr);
  let files: Vec<_> = run.lines.iter().map(|line| line["file"].as_str().unwrap()).collect();
  assert_eq!(files, ["src/generated/kept.ts", "src/kept.ts"]);
}

#[test]
fn ignore_comments_suppress_the_next_line_or_the_named_rule() {
  let rules =
    [rule("dbg", "JavaScript", "rule:\n  kind: debugger_statement"), rule("num", "JavaScript", "rule:\n  kind: number")].join("---\n");
  let src = "let keep = 0;\n// ast-grep-ignore\ndebugger; f(1);\n// ast-grep-ignore: num\ndebugger; f(2);\ndebugger; f(3); // ast-grep-ignore: dbg\n";
  let run = Case::new("suppress", &rules, &[("a.js", src)]).run("stream");
  let seen: Vec<(u64, &str)> =
    run.lines.iter().map(|l| (l["range"]["start"]["line"].as_u64().unwrap(), l["ruleId"].as_str().unwrap())).collect();
  assert_eq!(seen, [(0, "num"), (4, "dbg"), (5, "num")]);
}

#[test]
fn a_bare_ignore_on_the_first_line_silences_the_file() {
  let case = Case::new(
    "suppress-file",
    &rule("dbg", "JavaScript", "rule:\n  kind: debugger_statement"),
    &[("a.js", "// ast-grep-ignore\n\ndebugger;\n")],
  );
  assert!(case.run("stream").lines.is_empty());
}

#[test]
fn scripts_inside_html_are_scanned_as_their_language() {
  let rules =
    [rule("js-call", "JavaScript", "rule:\n  pattern: go($X)"), rule("ts-call", "TypeScript", "rule:\n  pattern: go($X)")].join("---\n");
  let html = "<html><body>\n<script>go(1)</script>\n<script lang=\"ts\">go(2 as number)</script>\n<p>go(3)</p>\n</body></html>\n";
  let run = Case::new("html", &rules, &[("page.html", html)]).run("stream");
  let seen: Vec<(&str, &str)> = run.lines.iter().map(|l| (l["ruleId"].as_str().unwrap(), l["text"].as_str().unwrap())).collect();
  assert_eq!(seen, [("js-call", "go(1)"), ("ts-call", "go(2 as number)")]);
  assert_eq!(run.lines[0]["range"]["start"]["line"], 1, "positions are those of the HTML file");
}

#[test]
fn a_language_that_cannot_spell_dollar_gets_its_expando() {
  // A bare Go call does not parse as a file, so its pattern names the node it means.
  let go = "rule:\n  pattern:\n    context: 'func f() { fmt.Println($$$A) }'\n    selector: call_expression";
  let rules = [rule("py", "Python", "rule:\n  pattern: print($A)"), rule("rs", "Rust", "rule:\n  pattern: Some($A)"), rule("go", "Go", go)]
    .join("---\n");
  let files =
    [("a.py", "print(value)\n"), ("a.rs", "fn f() { let _ = Some(1); }\n"), ("a.go", "package p\nfunc f() { fmt.Println(1, 2) }\n")];
  let run = Case::new("expando", &rules, &files).run("stream");
  let mut seen: Vec<(&str, &str)> = run.lines.iter().map(|l| (l["ruleId"].as_str().unwrap(), l["text"].as_str().unwrap())).collect();
  seen.sort();
  assert_eq!(seen, [("go", "fmt.Println(1, 2)"), ("py", "print(value)"), ("rs", "Some(1)")]);
}

#[test]
fn an_invalid_rule_fails_the_run_and_names_the_reason() {
  let bad_kind = Case::new("bad-kind", &rule("x", "TypeScript", "rule:\n  kind: no_such_kind"), &[("a.ts", "1;\n")]).run("stream");
  assert_eq!(bad_kind.code, 2);
  assert!(bad_kind.stderr.contains("Kind `no_such_kind` is invalid"), "{}", bad_kind.stderr);
  let bad_lang = Case::new("bad-lang", &rule("x", "Klingon", "rule:\n  kind: number"), &[("a.ts", "1;\n")]).run("stream");
  assert_eq!(bad_lang.code, 2);
  assert!(bad_lang.stderr.contains("Klingon is not supported"), "{}", bad_lang.stderr);
}

#[test]
fn a_rule_no_kind_can_key_is_skipped_and_said() {
  let run = Case::new("no-kind", &rule("only-regex", "JavaScript", "rule:\n  regex: foo"), &[("a.js", "foo;\n")]).run("stream");
  assert_eq!(run.code, 0);
  assert!(run.lines.is_empty());
  assert!(run.stderr.contains("rule `only-regex` must have kind"), "{}", run.stderr);
}

#[test]
fn an_error_severity_match_exits_one() {
  let rules = "id: e\nlanguage: JavaScript\nmessage: m\nseverity: error\nrule:\n  kind: debugger_statement\n";
  let case = Case::new("severity", rules, &[("a.js", "debugger;\n")]);
  assert_eq!(case.run("stream").code, 1);
  assert_eq!(case.run("stream").lines[0]["severity"], "error");
}

#[test]
fn files_outside_the_rules_languages_and_unreadable_text_are_skipped() {
  let case =
    Case::new("skips", &rule("num", "JavaScript", "rule:\n  kind: number"), &[("a.js", "1;\n"), ("b.py", "2\n"), ("empty.js", "")]);
  std::fs::write(case.dir.join("src/binary.js"), [0x31u8, 0x3b, 0xff, 0xfe, 0x0a]).unwrap();
  let run = case.run("stream");
  assert_eq!(run.lines.len(), 1);
  assert_eq!(run.lines[0]["file"], "src/a.js");
}

#[test]
fn columns_on_one_long_non_ascii_line_are_character_offsets() {
  let line = format!("{}debugger;\n", "'é'; ".repeat(500));
  let run = Case::new("long-line", &rule("dbg", "JavaScript", "rule:\n  kind: debugger_statement"), &[("a.js", &line)]).run("slim");
  assert_eq!(run.lines[0]["range"]["byteOffset"]["start"], 500 * 6);
  assert_eq!(run.lines[0]["range"]["start"]["column"], 500 * 5);
}

#[test]
fn the_fast_parser_reports_what_tree_sitter_reports() {
  let rules = [
    rule("ts-fn", "TypeScript", "rule:\n  kind: function_declaration\n  has:\n    field: name\n    pattern: $NAME"),
    rule("ts-call", "TypeScript", "rule:\n  pattern: $F<$T>($$$ARGS)"),
    rule("py-def", "Python", "rule:\n  kind: function_definition\n  has:\n    field: name\n    pattern: $NAME"),
    rule("rs-raw", "Rust", "rule:\n  kind: raw_string_literal"),
    rule("go-call", "Go", "rule:\n  kind: call_expression"),
    rule("js-num", "JavaScript", "rule:\n  kind: number"),
  ]
  .join("---\n");
  let files = [
    // Generics against comparisons, and an arrow against a parenthesised expression: GLR forks.
    ("a.ts", "function f<T>(x: T) { return g<T>(x) }\nconst h = (a) => a < 1 > (2);\nconst k = (a, b);\n"),
    // Indentation comes from an external scanner that carries state.
    ("a.py", "def outer():\n    def inner():\n        return 1\n    return inner\n\nclass C:\n    def m(self):\n        pass\n"),
    ("a.rs", "fn f() -> &'static str { r#\"raw \"quoted\" text\"# }\n"),
    ("a.go", "package p\n\nfunc f() { g(T{1}); h(x) }\n"),
    // A syntax error: the fast parser hands this file back, and it is still reported.
    ("broken.js", "let a = 1;\nfunction ( { 2 \nlet b = 3;\n"),
  ];
  let case = Case::new("fast-parser", &rules, &files);
  let reference = case.run("stream");
  let fast = case.run_with("stream", &["--parser=fast"]);
  assert_eq!(fast.code, reference.code);
  assert_eq!(fast.lines, reference.lines);
  let ids: std::collections::BTreeSet<&str> = fast.lines.iter().map(|l| l["ruleId"].as_str().unwrap()).collect();
  assert_eq!(ids.into_iter().collect::<Vec<_>>(), ["go-call", "js-num", "py-def", "rs-raw", "ts-call", "ts-fn"]);
}

fn stat(run: &Run, label: &str) -> String {
  run
    .stderr
    .lines()
    .find_map(|l| l.trim().strip_prefix("astscan:").and_then(|l| l.trim().strip_prefix(label)))
    .unwrap_or("")
    .trim()
    .to_string()
}

#[test]
fn a_cached_run_reports_what_a_cold_run_reports() {
  let rules =
    [rule("log", "TypeScript", "rule:\n  pattern: console.log($A)"), rule("dbg", "TypeScript", "rule:\n  kind: debugger_statement")]
      .join("---\n");
  let case =
    Case::new("cache", &rules, &[("a.ts", "console.log(1);\ndebugger;\n"), ("b.ts", "console.log(2);\n"), ("c.ts", "let x = 1;\n")]);
  let plain = case.run("stream");
  let options = ["--cache", "cache-dir", "--stats"];
  let cold = case.run_with("stream", &options);
  assert_eq!(cold.lines, plain.lines);
  assert_eq!(stat(&cold, "cache:"), "0 hits, 3 misses");
  let warm = case.run_with("stream", &options);
  assert_eq!(warm.lines, plain.lines);
  assert_eq!(stat(&warm, "cache:"), "3 hits, 0 misses");

  // One file changes: only that file is scanned again, and its new matches show.
  std::fs::write(case.dir.join("src/c.ts"), "debugger;\n").unwrap();
  let edited = case.run_with("stream", &options);
  assert_eq!(stat(&edited, "cache:"), "2 hits, 1 misses");
  assert_eq!(edited.lines, case.run("stream").lines);
  assert_eq!(edited.lines.len(), plain.lines.len() + 1);

  // The cache holds matches, not lines: the other output shape is written from the same entries.
  let slim = case.run_with("slim", &options);
  assert_eq!(stat(&slim, "cache:"), "3 hits, 0 misses");
  assert_eq!(slim.lines, case.run("slim").lines);
}

#[test]
fn a_changed_rule_file_does_not_reuse_the_cache() {
  let case = Case::new("cache-rules", &rule("r", "TypeScript", "rule:\n  pattern: foo($A)"), &[("a.ts", "foo(1); bar(2);\n")]);
  let options = ["--cache", "cache-dir", "--stats"];
  assert_eq!(texts(&case.run_with("stream", &options)), ["foo(1)"]);
  std::fs::write(case.dir.join("rules.yml"), rule("r", "TypeScript", "rule:\n  pattern: bar($A)")).unwrap();
  let run = case.run_with("stream", &options);
  assert_eq!(texts(&run), ["bar(2)"]);
  assert_eq!(stat(&run, "cache:"), "0 hits, 1 misses");
}

#[test]
fn a_cached_run_keeps_the_exit_code() {
  let rules = "id: e\nlanguage: TypeScript\nmessage: m\nseverity: error\nrule:\n  kind: debugger_statement\n";
  let case = Case::new("cache-exit", rules, &[("a.ts", "debugger;\n")]);
  let options = ["--cache", "cache-dir", "--stats"];
  assert_eq!(case.run_with("stream", &options).code, 1);
  let warm = case.run_with("stream", &options);
  assert_eq!(stat(&warm, "cache:"), "1 hits, 0 misses");
  assert_eq!(warm.code, 1);
}

#[test]
fn a_damaged_cache_is_not_trusted() {
  let case =
    Case::new("cache-damage", &rule("r", "TypeScript", "rule:\n  pattern: foo($A)"), &[("a.ts", "foo(1);\n"), ("b.ts", "foo(2);\n")]);
  let options = ["--cache", "cache-dir", "--stats"];
  let cold = case.run_with("stream", &options);
  let file = std::fs::read_dir(case.dir.join("cache-dir")).unwrap().next().unwrap().unwrap().path();
  let bytes = std::fs::read(&file).unwrap();

  // Cut short in the middle of the last record: the records before it still serve.
  std::fs::write(&file, &bytes[..bytes.len() - 10]).unwrap();
  let torn = case.run_with("stream", &options);
  assert_eq!(torn.lines, cold.lines);
  assert_eq!(stat(&torn, "cache:"), "1 hits, 1 misses");
  // The torn record was cut away and rewritten, so everything is served again.
  assert_eq!(stat(&case.run_with("stream", &options), "cache:"), "2 hits, 0 misses");

  // A flipped byte inside a stored result: its checksum no longer holds.
  let mut bytes = std::fs::read(&file).unwrap();
  let last = bytes.len() - 5;
  bytes[last] ^= 0x20;
  std::fs::write(&file, &bytes).unwrap();
  let flipped = case.run_with("stream", &options);
  assert_eq!(flipped.lines, cold.lines);
  assert_eq!(stat(&flipped, "cache:"), "1 hits, 1 misses");

  // Not a cache file at all.
  std::fs::write(&file, b"garbage").unwrap();
  let garbage = case.run_with("stream", &options);
  assert_eq!(garbage.lines, cold.lines);
  assert_eq!(stat(&garbage, "cache:"), "0 hits, 2 misses");
  assert_eq!(stat(&case.run_with("stream", &options), "cache:"), "2 hits, 0 misses");
}

#[test]
fn a_cached_run_rewrites_metavariables_labels_and_suppressions() {
  let rules = [
    rule("call", "TypeScript", "rule:\n  pattern: f($A, $$$REST)"),
    rule("call-too", "TypeScript", "rule:\n  pattern: f($A, $$$REST)"),
    rule("in-fn", "TypeScript", "rule:\n  kind: return_statement\n  inside:\n    kind: function_declaration\n    stopBy: end"),
    rule("css", "Css", "rule:\n  kind: declaration"),
    rule("js", "JavaScript", "rule:\n  pattern: alert($X)"),
  ]
  .join("---\n");
  let files = [
    ("a.ts", "f(1, 2, 3);\n// ast-grep-ignore: call\nf(4, 5);\nfunction g() { return f(\"é\", 日本); }\n"),
    ("page.html", "<html><style>p { color: red; }</style><script>alert(1)</script></html>\n"),
  ];
  let case = Case::new("cache-shapes", &rules, &files);
  let options = ["--cache", "cache-dir", "--stats"];
  for json in ["stream", "slim"] {
    let plain = case.run(json);
    assert!(plain.lines.len() >= 6, "{} lines", plain.lines.len());
    let cached = case.run_with(json, &options);
    assert_eq!(cached.lines, plain.lines);
    let again = case.run_with(json, &options);
    assert_eq!(again.lines, plain.lines);
    assert_eq!(stat(&again, "cache:"), "2 hits, 0 misses");
  }
}

#[test]
fn a_moved_or_copied_file_is_answered_from_the_cache_under_its_new_name() {
  let rules =
    [rule("ts", "TypeScript", "rule:\n  pattern: foo($A)"), rule("js", "JavaScript", "rule:\n  pattern: foo($$$A)")].join("---\n");
  let case = Case::new("cache-moved", &rules, &[("a.ts", "foo(1);\n")]);
  let options = ["--cache", "cache-dir", "--stats"];
  case.run_with("stream", &options);
  std::fs::rename(case.dir.join("src/a.ts"), case.dir.join("src/moved.ts")).unwrap();
  std::fs::copy(case.dir.join("src/moved.ts"), case.dir.join("src/copy.ts")).unwrap();
  // The same bytes as JavaScript are another question, with another answer.
  std::fs::copy(case.dir.join("src/moved.ts"), case.dir.join("src/same.js")).unwrap();
  let run = case.run_with("stream", &options);
  assert_eq!(stat(&run, "cache:"), "2 hits, 1 misses");
  assert_eq!(run.lines, case.run("stream").lines);
  let files: Vec<(&str, &str)> = run.lines.iter().map(|l| (l["file"].as_str().unwrap(), l["ruleId"].as_str().unwrap())).collect();
  assert_eq!(files, [("src/copy.ts", "ts"), ("src/moved.ts", "ts"), ("src/same.js", "js")]);
}
