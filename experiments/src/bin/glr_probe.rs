//! How much of a source tree could a single-stack LR driver parse?
//!
//! Parses every file with tree-sitter's own logger attached and classifies
//! it: `clean` files never held more than one stack version and never hit
//! error recovery, so a deterministic driver would produce their tree
//! without help. The rest need GLR (`split`) or error recovery (`error`).
//!
//!   cargo run --release -p astscan-experiments --bin glr_probe -- <dir>...

use astscan_core::Registry;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tree_sitter::{LogType, Parser};

#[derive(Default, Clone, Copy)]
struct Tally {
  files: usize,
  bytes: usize,
  split_files: usize,
  split_bytes: usize,
  error_files: usize,
  error_bytes: usize,
  /// Parser steps in total, and those taken while several versions were alive.
  steps: usize,
  split_steps: usize,
  /// Runs of consecutive multi-version steps: how many places a split starts.
  split_runs: usize,
}

#[derive(Default)]
struct FileLog {
  steps: usize,
  split_steps: usize,
  split_runs: usize,
  in_split: bool,
  error: bool,
}

fn main() {
  let roots: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
  if roots.is_empty() {
    eprintln!("usage: glr_probe <dir>...");
    std::process::exit(2);
  }
  let mut registry = Registry::default();
  let mut files: Vec<(PathBuf, String)> = Vec::new();
  let mut walk = ignore::WalkBuilder::new(&roots[0]);
  roots[1..].iter().for_each(|r| {
    walk.add(r);
  });
  for entry in walk.build().flatten() {
    if !entry.file_type().is_some_and(|t| t.is_file()) {
      continue;
    }
    if let Some(name) = registry.name_for_path(entry.path()) {
      files.push((entry.into_path(), name.to_string()));
    }
  }
  let langs: BTreeMap<String, tree_sitter::Language> = files
    .iter()
    .map(|(_, n)| n.clone())
    .collect::<std::collections::BTreeSet<_>>()
    .into_iter()
    .filter_map(|n| registry.grammar(&n).map(|l| (n, l.builtin_language().expect("built-in grammar").clone())))
    .collect();

  let next = AtomicUsize::new(0);
  let totals: Mutex<BTreeMap<String, Tally>> = Mutex::new(BTreeMap::new());
  let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
  std::thread::scope(|scope| {
    for _ in 0..threads {
      scope.spawn(|| {
        let mut local: BTreeMap<String, Tally> = BTreeMap::new();
        loop {
          let i = next.fetch_add(1, Ordering::Relaxed);
          let Some((path, lang)) = files.get(i) else { break };
          let Ok(src) = std::fs::read(path) else { continue };
          if src.is_empty() || std::str::from_utf8(&src).is_err() || src.len() > 3_000_000 {
            continue;
          }
          let mut parser = Parser::new();
          parser.set_language(&langs[lang]).unwrap();
          let mut log = FileLog::default();
          // SAFETY: the logger is removed before `log` goes out of scope.
          unsafe {
            parser.set_logger_unchecked(Some(Box::new(|kind, msg| {
              if kind != LogType::Parse {
                return;
              }
              if let Some(rest) = msg.strip_prefix("process version:") {
                log.steps += 1;
                let multi = rest.split_once("version_count:").is_some_and(|(_, r)| !r.starts_with("1,"));
                if multi {
                  log.split_steps += 1;
                  if !log.in_split {
                    log.split_runs += 1;
                  }
                }
                log.in_split = multi;
              } else if msg.starts_with("recover_") || msg.starts_with("skip_token") {
                // `detect_error` alone is not one: a losing branch of a split ends that way.
                log.error = true;
              }
            })));
          }
          let tree = parser.parse(&src, None);
          unsafe { parser.set_logger_unchecked(None) };
          let has_error = log.error || tree.is_none_or(|t| t.root_node().has_error());
          let t = local.entry(lang.clone()).or_default();
          t.files += 1;
          t.bytes += src.len();
          t.steps += log.steps;
          t.split_steps += log.split_steps;
          t.split_runs += log.split_runs;
          if has_error {
            t.error_files += 1;
            t.error_bytes += src.len();
          } else if log.split_steps > 0 {
            t.split_files += 1;
            t.split_bytes += src.len();
          }
        }
        let mut totals = totals.lock().unwrap();
        for (lang, t) in local {
          let sum = totals.entry(lang).or_default();
          sum.files += t.files;
          sum.bytes += t.bytes;
          sum.split_files += t.split_files;
          sum.split_bytes += t.split_bytes;
          sum.error_files += t.error_files;
          sum.error_bytes += t.error_bytes;
          sum.steps += t.steps;
          sum.split_steps += t.split_steps;
          sum.split_runs += t.split_runs;
        }
      });
    }
  });

  let pct = |a: usize, b: usize| if b == 0 { 0.0 } else { a as f64 * 100.0 / b as f64 };
  println!(
    "{:<12} {:>6} {:>8}  {:>12} {:>12} {:>12}  {:>11} {:>10}",
    "language", "files", "MB", "clean bytes", "split bytes", "error bytes", "split steps", "splits/KB"
  );
  for (lang, t) in totals.lock().unwrap().iter() {
    let clean = t.bytes - t.split_bytes - t.error_bytes;
    println!(
      "{:<12} {:>6} {:>8.1}  {:>11.1}% {:>11.1}% {:>11.1}%  {:>10.2}% {:>10.2}",
      lang,
      t.files,
      t.bytes as f64 / 1e6,
      pct(clean, t.bytes),
      pct(t.split_bytes, t.bytes),
      pct(t.error_bytes, t.bytes),
      pct(t.split_steps, t.steps),
      t.split_runs as f64 * 1000.0 / t.bytes.max(1) as f64,
    );
  }
}
