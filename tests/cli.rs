//! Black-box tests: a rule file and a source tree in, JSON lines out.
//!
//! The expected values were read off ast-grep 0.45.3 for the same input;
//! `tools/diff.mjs` is the test that compares the two tools wholesale.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
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
    let out = Command::new(env!("CARGO_BIN_EXE_astscan"))
      .current_dir(&self.dir)
      .args(["scan", "--rule", "rules.yml", &format!("--json={json}")])
      .args(options)
      .args(["--", "src"])
      .output()
      .unwrap();
    let mut lines: Vec<Value> = String::from_utf8(out.stdout).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    lines.sort_by_key(|l| {
      (l["file"].as_str().map(str::to_string), l["range"]["byteOffset"]["start"].as_u64(), l["ruleId"].as_str().map(str::to_string))
    });
    Run { code: out.status.code().unwrap(), lines, stderr: String::from_utf8(out.stderr).unwrap() }
  }
}

fn rule(id: &str, language: &str, body: &str) -> String {
  format!("id: {id}\nlanguage: {language}\nmessage: m\nseverity: hint\n{body}\n")
}

fn texts(run: &Run) -> Vec<&str> {
  run.lines.iter().map(|l| l["text"].as_str().unwrap()).collect()
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
