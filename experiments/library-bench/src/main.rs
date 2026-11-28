use ast_grep_config::{CombinedScan, GlobalRules, from_yaml_string};
use ast_grep_core::tree_sitter::LanguageExt;
use ast_grep_language::SupportLang;
use astscan_core::{LiteralPrefilter, Registry, RuleSet, ScanOptimizations, Scanner};
use cpu_time::ProcessTime;
use serde_json::{Value, json};
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

const CALLS: [&str; 16] = [
  "fmt.Errorf",
  "fmt.Sprintf",
  "fmt.Fprintf",
  "t.Errorf",
  "t.Fatal",
  "t.Fatalf",
  "errors.New",
  "strings.Contains",
  "strings.NewReader",
  "context.Background",
  "http.NewRequest",
  "io.Copy",
  "io.ReadAll",
  "time.Now",
  "len",
  "append",
];
const WARMUPS: usize = 3;
const SAMPLES: usize = 15;

// Rule index, start byte, and exclusive end byte.
type Hit = (usize, usize, usize);
type Scan<'a> = dyn FnMut(&str, &mut Vec<Hit>) + 'a;

struct Source {
  path: PathBuf,
  text: String,
}

fn visit_go_files(directory: &Path, paths: &mut Vec<PathBuf>) -> std::io::Result<()> {
  for entry in fs::read_dir(directory)? {
    let entry = entry?;
    let kind = entry.file_type()?;
    let path = entry.path();
    if kind.is_dir() {
      visit_go_files(&path, paths)?;
    } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "go") {
      paths.push(path);
    }
  }
  Ok(())
}

fn yaml(names: &[&str]) -> String {
  let mut yaml = String::new();
  for (i, name) in names.iter().enumerate() {
    let id = char::from(b'A' + i as u8);
    yaml.push_str(&format!(
      "---\nid: {id}\nlanguage: Go\nseverity: warning\nrule:\n  pattern:\n    context: 'package p; func bench() {{ {name}($$$) }}'\n    selector: call_expression\n"
    ));
  }
  yaml
}

fn query_patterns(names: &[&str]) -> String {
  let mut patterns = String::new();
  for name in names {
    let pattern = if let Some((receiver, member)) = name.split_once('.') {
      format!(
        r#"((call_expression function: (selector_expression
              operand: (identifier) @receiver
              field: (field_identifier) @member)) @call
             (#eq? @receiver "{receiver}") (#eq? @member "{member}"))"#
      )
    } else {
      format!(r#"((call_expression function: (identifier) @name) @call (#eq? @name "{name}"))"#)
    };
    patterns.push_str(&pattern);
    patterns.push('\n');
  }
  patterns
}

fn rule_index(id: &str) -> usize {
  usize::from(id.as_bytes()[0] - b'A')
}

fn scan_astscan(scanner: &mut Scanner<'_>, source: &str, hits: &mut Vec<Hit>) {
  scanner
    .scan("Go", source, |hit| {
      let bytes = hit.range();
      hits.extend(hit.rules().map(|rule| (rule_index(&rule.id), bytes.start as usize, bytes.end as usize)));
    })
    .unwrap();
}

struct Engine<'a> {
  name: &'static str,
  scan: &'a mut Scan<'a>,
  wall_ms: Vec<f64>,
  cpu_ms: Vec<f64>,
}

impl<'a> Engine<'a> {
  fn new(name: &'static str, scan: &'a mut Scan<'a>) -> Self {
    Self { name, scan, wall_ms: Vec::with_capacity(SAMPLES), cpu_ms: Vec::with_capacity(SAMPLES) }
  }
}

fn verify(engines: &mut [Engine<'_>], corpus: &[Source], rule_count: usize) -> Vec<usize> {
  let mut totals = vec![0; rule_count];
  let mut expected = Vec::new();
  let mut actual = Vec::new();
  for source in corpus {
    expected.clear();
    (engines[0].scan)(&source.text, &mut expected);
    expected.sort_unstable();
    for &(rule, _, _) in &expected {
      totals[rule] += 1;
    }
    for engine in &mut engines[1..] {
      actual.clear();
      (engine.scan)(&source.text, &mut actual);
      actual.sort_unstable();
      assert_eq!(expected, actual, "{} differs on {}", engine.name, source.path.display());
    }
  }
  assert!(totals.iter().all(|&count| count > 0), "every rule must match at least once");
  totals
}

fn summarize(samples: &[f64]) -> Value {
  let mut sorted = samples.to_vec();
  sorted.sort_by(f64::total_cmp);
  let mean = samples.iter().sum::<f64>() / samples.len() as f64;
  let variance = samples.iter().map(|ms| (ms - mean).powi(2)).sum::<f64>() / (samples.len() - 1) as f64;
  json!({
    "mean_ms": mean, "stddev_ms": variance.sqrt(), "median_ms": sorted[sorted.len() / 2],
    "samples_ms": samples,
  })
}

fn measure(corpus: &[Source], names: &[&str]) -> Value {
  let yaml = yaml(names);
  let rules = RuleSet::compile(&yaml, &mut Registry::default()).unwrap();
  let mut astscan = Scanner::new(&rules);
  let mut astscan_all =
    Scanner::new(&rules).with_optimizations(ScanOptimizations { literal_prefilter: LiteralPrefilter::Off, ..ScanOptimizations::default() });

  let grep_rules = from_yaml_string::<SupportLang>(&yaml, &GlobalRules::default()).unwrap();
  let grep = CombinedScan::new(grep_rules.iter().collect());
  let go = SupportLang::Go;

  let language = tree_sitter_go::LANGUAGE.into();
  let mut parser = Parser::new();
  parser.set_language(&language).unwrap();
  let query = Query::new(&language, &query_patterns(names)).unwrap();
  let call_capture = query.capture_index_for_name("call").unwrap();
  let mut cursor = QueryCursor::new();

  let mut scan = |source: &str, hits: &mut Vec<Hit>| scan_astscan(&mut astscan, source, hits);
  let mut scan_all = |source: &str, hits: &mut Vec<Hit>| scan_astscan(&mut astscan_all, source, hits);
  let mut grep_scan = |source: &str, hits: &mut Vec<Hit>| {
    let root = go.ast_grep(source);
    for (rule, matches) in grep.scan(&root, false).matches {
      let id = rule_index(&rule.id);
      hits.extend(matches.into_iter().map(|hit| {
        let bytes = hit.range();
        (id, bytes.start, bytes.end)
      }));
    }
  };
  let mut query_scan = |source: &str, hits: &mut Vec<Hit>| {
    let source = source.as_bytes();
    let tree = parser.parse(source, None).unwrap();
    let mut matches = cursor.matches(&query, tree.root_node(), source);
    while let Some(hit) = matches.next() {
      let node = hit.captures().iter().find(|capture| capture.index == call_capture).unwrap().node;
      hits.push((hit.pattern_index, node.start_byte(), node.end_byte()));
    }
    assert!(!cursor.did_exceed_match_limit());
  };
  let mut engines = [
    Engine::new("astscan-core", &mut scan),
    Engine::new("astscan-core (prefilter off)", &mut scan_all),
    Engine::new("ast-grep CombinedScan", &mut grep_scan),
    Engine::new("Tree-sitter QueryCursor", &mut query_scan),
  ];

  let totals = verify(&mut engines, corpus, names.len());
  let expected_count: usize = totals.iter().sum();
  let mut hits = Vec::new();
  let engine_count = engines.len();
  // Rotate which engine starts each round to spread machine load across engines.
  for round in 0..WARMUPS + SAMPLES {
    for offset in 0..engine_count {
      let engine = &mut engines[(round + offset) % engine_count];
      let wall = Instant::now();
      let cpu = ProcessTime::now();
      let mut count = 0;
      for source in corpus {
        hits.clear();
        (engine.scan)(black_box(&source.text), &mut hits);
        count += black_box(hits.as_slice()).len();
      }
      let cpu_ms = cpu.elapsed().as_secs_f64() * 1000.0;
      let wall_ms = wall.elapsed().as_secs_f64() * 1000.0;
      assert_eq!(count, expected_count, "{}", engine.name);
      if round >= WARMUPS {
        engine.wall_ms.push(wall_ms);
        engine.cpu_ms.push(cpu_ms);
      }
    }
  }

  json!({
    "rules": names,
    "matches": expected_count,
    "matches_per_rule": totals,
    "engines": engines.iter().map(|engine| {
      json!({ "engine": engine.name, "wall": summarize(&engine.wall_ms), "cpu": summarize(&engine.cpu_ms) })
    }).collect::<Vec<_>>(),
  })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let mut args = std::env::args_os().skip(1);
  let path =
    args.next().map(PathBuf::from).filter(|_| args.next().is_none()).ok_or("usage: astscan-library-bench <Go source directory>")?;
  let mut files = Vec::new();
  visit_go_files(&path, &mut files)?;
  files.sort_unstable();
  let corpus = files.into_iter().map(|path| fs::read_to_string(&path).map(|text| Source { path, text })).collect::<Result<Vec<_>, _>>()?;
  let bytes: usize = corpus.iter().map(|source| source.text.len()).sum();
  let results = json!({
    "files": corpus.len(),
    "source_bytes": bytes,
    "warmups": WARMUPS,
    "samples": SAMPLES,
    "workloads": [measure(&corpus, &CALLS[..1]), measure(&corpus, &CALLS)],
  });
  println!("{}", serde_json::to_string_pretty(&results)?);
  Ok(())
}
