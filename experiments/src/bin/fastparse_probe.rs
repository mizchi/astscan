//! Does the experimental driver agree with tree-sitter, and how fast is it?
//!
//! For every file of one language under the given directories: parse with
//! tree-sitter (plus the flattening astscan does today), parse with
//! `astscan-core's fast parser`, and compare the two flat trees node for node.
//!
//!   cargo run --release -p astscan-experiments --bin fastparse_probe -- <Language> <dir>...

use astscan_core::Registry;
use astscan_core::internals::{Bail, FastParser, FlatTree, first_difference};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() {
  let mut args = std::env::args().skip(1);
  let lang_name = args.next().expect("usage: fastparse_probe <Language> <dir>...");
  let roots: Vec<PathBuf> = args.map(PathBuf::from).collect();
  let mut registry = Registry::default();
  let lang = registry.grammar(&lang_name).expect("a built-in language");
  let mut files = Vec::new();
  let mut walk = ignore::WalkBuilder::new(&roots[0]);
  roots[1..].iter().for_each(|r| {
    walk.add(r);
  });
  for entry in walk.build().flatten() {
    if entry.file_type().is_some_and(|t| t.is_file()) && registry.name_for_path(entry.path()) == Some(lang.name.as_str()) {
      files.push(entry.into_path());
    }
  }

  let mut ts = tree_sitter::Parser::new();
  ts.set_language(lang.builtin_language().expect("built-in grammar")).unwrap();
  let mut fast = FastParser::new(lang.clone()).expect("a grammar the driver reads");
  let (mut reference, mut candidate) = (FlatTree::default(), FlatTree::default());
  let (mut ts_time, mut ts_time_on_ok, mut fast_time, mut fast_wasted) = (Duration::ZERO, Duration::ZERO, Duration::ZERO, Duration::ZERO);
  let (mut total, mut ok, mut ambiguous, mut error, mut wrong) = (0usize, 0usize, 0usize, 0usize, 0usize);
  let (mut bytes, mut ok_bytes) = (0usize, 0usize);
  // Twice over the corpus; the first pass warms caches and is not counted.
  for pass in 0..2 {
    for path in &files {
      let Ok(src) = std::fs::read_to_string(path) else { continue };
      if src.is_empty() || src.len() > 3_000_000 {
        continue;
      }
      let t = Instant::now();
      let tree = ts.parse(src.as_bytes(), None).unwrap();
      reference.build(tree);
      let ts_elapsed = t.elapsed();
      let t = Instant::now();
      let result = fast.parse(&src, &mut candidate);
      let fast_elapsed = t.elapsed();
      if pass == 0 {
        continue;
      }
      total += 1;
      bytes += src.len();
      ts_time += ts_elapsed;
      match result {
        Ok(()) => {
          if let Some(diff) = first_difference(&candidate, &reference) {
            wrong += 1;
            if wrong <= 5 {
              eprintln!("MISMATCH {}: {diff}", path.display());
            }
          } else {
            ok += 1;
            ok_bytes += src.len();
            ts_time_on_ok += ts_elapsed;
            fast_time += fast_elapsed;
          }
        }
        Err(Bail::Unsupported) => {
          ambiguous += 1;
          fast_wasted += fast_elapsed;
        }
        Err(_) => {
          error += 1;
          fast_wasted += fast_elapsed;
        }
      }
    }
  }
  let ms = |d: Duration| d.as_secs_f64() * 1e3;
  println!("{lang_name}: {total} files, {:.1} MB", bytes as f64 / 1e6);
  println!(
    "  identical tree: {ok} files ({:.1}% of bytes)   unsupported: {ambiguous}   handed back (needs error recovery): {error}   MISMATCH: {wrong}",
    ok_bytes as f64 * 100.0 / bytes.max(1) as f64
  );
  if ok > 0 {
    println!(
      "  on the identical files: tree-sitter + flatten {:.1} ms, own driver {:.1} ms  ->  {:.2}x  ({:.0} MB/s vs {:.0} MB/s)",
      ms(ts_time_on_ok),
      ms(fast_time),
      ms(ts_time_on_ok) / ms(fast_time),
      ok_bytes as f64 / 1e6 / ts_time_on_ok.as_secs_f64(),
      ok_bytes as f64 / 1e6 / fast_time.as_secs_f64(),
    );
  }
  println!("  all files, tree-sitter + flatten: {:.1} ms   time spent before bailing: {:.1} ms", ms(ts_time), ms(fast_wasted));
}
