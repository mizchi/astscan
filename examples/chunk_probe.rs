//! How much of a changed file is unchanged, at which granularity?
//!
//! An experiment for a content-addressed store. Replays the last N commits of
//! a Git repository in order and, for every added or modified source file,
//! cuts the new version into chunks under several schemes. A chunk whose hash
//! the store has seen costs nothing; the rest is new bytes. Finer chunks
//! reuse more but need a longer list per file version, so both are reported.
//!
//!   cargo run --release --example chunk_probe -- <repo> [commits=2000] [Language,Language...]
//!
//! Schemes: the whole file; content-defined chunks at three sizes (a gear
//! rolling hash with FastCDC's two-mask normalisation); the children of the
//! syntax tree's root; the syntax tree split until pieces fit a budget;
//! "syntax cdc", which cuts only at node ends but picks which ends by content,
//! so small siblings share a chunk; and single lines, the floor a text diff
//! would reach.
//!
//! Costs: `new` is chunk bytes the store had not seen. `zstd` is those bytes
//! packed 256 KB at a time and compressed, as segments would be. `list` is
//! what the file versions pay to name their chunks, with the lists themselves
//! cut into shared groups of about 64 ids (a two-level tree).
//!
//! Reads only what the clone holds: in a partial clone the blobs it lacks are
//! skipped, never fetched.
//!
//! It also counts how many directories a commit touches, which is what a
//! checkout of one subdirectory has to live with.

use astscan::lang::Registry;
use astscan::tree::FlatTree;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use xxhash_rust::xxh3::xxh3_128;

/// Bytes a file version pays per chunk for listing it: a 32-byte id.
const LIST_ENTRY: usize = 32;

/// New chunk bytes are compressed in packs of this size.
const PACK: usize = 256 << 10;

#[derive(Default)]
struct Scheme {
  name: String,
  seen: HashSet<u128>,
  seen_groups: HashSet<u128>,
  new_bytes: usize,
  listed_chunks: usize,
  /// Bytes of chunk lists: new groups of ids, plus one id per group per version.
  list_bytes: usize,
  pack: Vec<u8>,
  packed_bytes: usize,
  ids: Vec<u8>,
}

impl Scheme {
  fn new(name: &str) -> Self {
    Scheme { name: name.to_string(), ..Scheme::default() }
  }

  fn seal(&mut self) {
    if !self.pack.is_empty() {
      self.packed_bytes += zstd::bulk::compress(&self.pack, 3).expect("zstd compresses").len();
      self.pack.clear();
    }
  }

  /// Store the chunks of one blob. With `count`, charge the new ones.
  fn store(&mut self, blob: &[u8], cuts: &[usize], count: bool) {
    let mut from = 0;
    self.ids.clear();
    let mut group_from = 0;
    for (i, &to) in cuts.iter().enumerate() {
      let chunk = &blob[from..to];
      from = to;
      let id = xxh3_128(chunk);
      let fresh = self.seen.insert(id);
      if count {
        self.listed_chunks += 1;
        if fresh {
          self.new_bytes += chunk.len();
          self.pack.extend_from_slice(chunk);
          if self.pack.len() >= PACK {
            self.seal();
          }
        }
      }
      // The list of ids is cut where an id says so: about one in 64.
      self.ids.extend_from_slice(&id.to_le_bytes());
      if cuts.len() > 1 && ((id as u64).is_multiple_of(64) || i + 1 == cuts.len()) {
        let fresh_group = self.seen_groups.insert(xxh3_128(&self.ids[group_from..]));
        if count {
          let entries = (self.ids.len() - group_from) / 16;
          self.list_bytes += LIST_ENTRY + if fresh_group { entries * LIST_ENTRY } else { 0 };
        }
        group_from = self.ids.len();
      }
    }
  }
}

/// A gear table from a fixed seed, so runs agree.
fn gear_table() -> [u64; 256] {
  let mut state = 0x9E37_79B9_7F4A_7C15u64;
  std::array::from_fn(|_| {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
  })
}

/// Content-defined cut points: end offsets of the chunks, the last being `len`.
fn cdc(blob: &[u8], average: usize, gear: &[u64; 256], cuts: &mut Vec<usize>) {
  cuts.clear();
  let (min, max) = (average / 4, average * 4);
  let bits = average.trailing_zeros();
  // Harder to cut before the average, easier after: fewer tiny and huge chunks.
  let (hard, easy) = ((1u64 << (bits + 2)) - 1, (1u64 << (bits - 2)) - 1);
  let mut start = 0;
  while start < blob.len() {
    let end = (start + max).min(blob.len());
    let mut hash = 0u64;
    let mut cut = end;
    for i in (start + min).min(end)..end {
      hash = (hash << 1).wrapping_add(gear[blob[i] as usize]);
      let mask = if i - start < average { hard } else { easy };
      if hash & mask == 0 {
        cut = i + 1;
        break;
      }
    }
    cuts.push(cut);
    start = cut;
  }
}

fn lines(blob: &[u8], cuts: &mut Vec<usize>) {
  cuts.clear();
  cuts.extend(memchr::memchr_iter(b'\n', blob).map(|i| i + 1));
  if cuts.last() != Some(&blob.len()) {
    cuts.push(blob.len());
  }
}

/// Cut only at node ends, but let content pick which: after a node whose own
/// bytes hash under a threshold proportional to its length, so that chunks
/// come out around `average` whatever the nodes' sizes, and an edit moves at
/// most the cuts next to it. A node too large to share a chunk is descended into.
#[allow(clippy::too_many_arguments)]
fn syntax_cdc(t: &FlatTree, blob: &[u8], node: u32, lo: usize, hi: usize, average: usize, open: &mut usize, cuts: &mut Vec<usize>) {
  let mut cursor = lo;
  let mut kids = t.children(node);
  while let Some(child) = kids.next(t) {
    let end = (t.end[child as usize] as usize).min(hi);
    if end <= cursor {
      continue;
    }
    if end - cursor > average * 4 && !t.is_leaf(child) {
      syntax_cdc(t, blob, child, cursor, end, average, open, cuts);
    } else {
      let piece = &blob[cursor..end];
      let size = end - *open;
      // P(cut) = len / average, and never before a quarter of the average.
      let wanted = (piece.len() as u128 * u64::MAX as u128 / average as u128).min(u64::MAX as u128) as u64;
      if size >= average * 4 || (size >= average / 4 && (xxh3_128(piece) as u64) < wanted) {
        cuts.push(end);
        *open = end;
      }
    }
    cursor = end;
  }
  if node == 0 && blob.len() > *open {
    cuts.push(blob.len());
    *open = blob.len();
  }
}

/// Cut at the ends of `node`'s children, descending into a child that is
/// larger than `budget`. What lies between two children goes with the later
/// one, and what follows the last child is a chunk of its own.
fn syntax(t: &FlatTree, node: u32, lo: usize, hi: usize, budget: usize, cuts: &mut Vec<usize>) {
  if hi - lo <= budget || t.is_leaf(node) {
    if hi > lo {
      cuts.push(hi);
    }
    return;
  }
  let mut cursor = lo;
  let mut kids = t.children(node);
  while let Some(child) = kids.next(t) {
    let end = (t.end[child as usize] as usize).min(hi);
    if end <= cursor {
      continue;
    }
    syntax(t, child, cursor, end, budget, cuts);
    cursor = end;
  }
  if hi > cursor {
    cuts.push(hi);
  }
}

struct Change {
  old: Option<String>,
  new: String,
  path: String,
}

fn main() {
  let mut args = std::env::args().skip(1);
  let repo = args.next().expect("usage: chunk_probe <repo> [commits]");
  let limit = args.next().unwrap_or_else(|| "2000".to_string());

  // The history, oldest first.
  let log = Command::new("git")
    .env("GIT_NO_LAZY_FETCH", "1")
    .args(["-C", &repo, "log", "--no-merges", "-n", &limit, "--raw", "--no-renames", "--abbrev=40", "--format=C %H"])
    .output()
    .expect("git runs");
  let mut commits: Vec<Vec<Change>> = Vec::new();
  for line in String::from_utf8_lossy(&log.stdout).lines() {
    if line.starts_with("C ") {
      commits.push(Vec::new());
    } else if let Some(rest) = line.strip_prefix(':') {
      let (meta, path) = rest.split_once('\t').expect("a raw diff line has a path");
      let fields: Vec<&str> = meta.split(' ').collect();
      let old = match fields[4] {
        "M" => Some(fields[2].to_string()),
        "A" => None,
        _ => continue,
      };
      commits.last_mut().expect("a commit line comes first").push(Change { old, new: fields[3].to_string(), path: path.to_string() });
    }
  }
  commits.reverse();

  // Lives as long as this program: it ends when its stdin closes at exit.
  #[allow(clippy::zombie_processes)]
  let mut cat = Command::new("git")
    .env("GIT_NO_LAZY_FETCH", "1")
    .args(["-C", &repo, "cat-file", "--batch"])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .spawn()
    .expect("git runs");
  let mut to_git = cat.stdin.take().expect("piped");
  let mut from_git = BufReader::new(cat.stdout.take().expect("piped"));
  let mut blob = |oid: &str| -> Option<Vec<u8>> {
    writeln!(to_git, "{oid}").ok()?;
    let mut header = String::new();
    from_git.read_line(&mut header).ok()?;
    let size: usize = header.split_whitespace().nth(2)?.parse().ok()?;
    let mut bytes = vec![0; size + 1];
    from_git.read_exact(&mut bytes).ok()?;
    bytes.pop();
    Some(bytes)
  };

  let gear = gear_table();
  let mut registry = Registry::default();
  let mut parsers: HashMap<String, tree_sitter::Parser> = HashMap::new();
  let mut tree = FlatTree::default();
  let mut schemes: Vec<Scheme> = [
    "file",
    "cdc 4096",
    "cdc 1024",
    "cdc 256",
    "syntax top-level",
    "syntax <= 1024",
    "syntax cdc 4096",
    "syntax cdc 1024",
    "syntax cdc 256",
    "line",
  ]
  .map(Scheme::new)
  .into();
  let only: Option<HashSet<String>> = args.next().map(|l| l.split(',').map(str::to_string).collect());
  let mut stored: HashSet<String> = HashSet::new();
  let mut cuts = Vec::new();
  let (mut versions, mut version_bytes, mut skipped) = (0usize, 0usize, 0usize);
  // The baseline Git's packs approximate: each version compressed against the one it replaces.
  let (mut delta_bytes, mut delta_base_fetches) = (0usize, 0usize);
  let mut dirs_touched: BTreeMap<usize, usize> = BTreeMap::new();
  let mut by_language: BTreeMap<String, usize> = BTreeMap::new();

  for commit in &commits {
    // Which second-level directories does this commit reach into?
    let dirs: HashSet<String> = commit.iter().map(|c| c.path.split('/').take(2).collect::<Vec<_>>().join("/")).collect();
    if !dirs.is_empty() {
      *dirs_touched.entry(dirs.len().min(8)).or_default() += 1;
    }
    for change in commit {
      let Some(name) = registry.name_for_path(Path::new(&change.path)).map(str::to_string) else { continue };
      if only.as_ref().is_some_and(|only| !only.contains(&name)) {
        continue;
      }
      let Some(lang) = registry.resolve(&name) else { continue };
      let parser = parsers.entry(name.clone()).or_insert_with(|| {
        let mut p = tree_sitter::Parser::new();
        p.set_language(&lang.ts).expect("a built-in grammar");
        p
      });
      let mut put = |oid: &str, count: bool, schemes: &mut Vec<Scheme>| -> Option<usize> {
        let bytes = blob(oid)?;
        if bytes.is_empty() || bytes.len() > 3_000_000 || std::str::from_utf8(&bytes).is_err() {
          return None;
        }
        tree.build(parser.parse(&bytes, None)?);
        for scheme in schemes.iter_mut() {
          match scheme.name.as_str() {
            "file" => {
              cuts.clear();
              cuts.push(bytes.len());
            }
            "line" => lines(&bytes, &mut cuts),
            name if name.starts_with("cdc ") => cdc(&bytes, name[4..].parse().expect("a size"), &gear, &mut cuts),
            name if name.starts_with("syntax cdc ") => {
              cuts.clear();
              let mut open = 0;
              syntax_cdc(&tree, &bytes, 0, 0, bytes.len(), name[11..].parse().expect("a size"), &mut open, &mut cuts);
            }
            name => {
              let budget = name.strip_prefix("syntax <= ").map_or(usize::MAX, |n| n.parse().expect("a size"));
              cuts.clear();
              if name == "syntax top-level" {
                // One level only: the root's children, whatever their size.
                let mut cursor = 0;
                let mut kids = tree.children(0);
                while let Some(child) = kids.next(&tree) {
                  let end = tree.end[child as usize] as usize;
                  if end > cursor {
                    cuts.push(end);
                    cursor = end;
                  }
                }
                if bytes.len() > cursor {
                  cuts.push(bytes.len());
                }
              } else {
                syntax(&tree, 0, 0, bytes.len(), budget, &mut cuts);
              }
            }
          }
          debug_assert_eq!(cuts.last(), Some(&bytes.len()));
          scheme.store(&bytes, &cuts, count);
        }
        Some(bytes.len())
      };
      // The version being replaced is in the store already; put it there if this run has not.
      if let Some(old) = &change.old
        && stored.insert(old.clone())
      {
        put(old, false, &mut schemes);
      }
      if !stored.insert(change.new.clone()) {
        continue;
      }
      match put(&change.new, true, &mut schemes) {
        Some(len) => {
          versions += 1;
          version_bytes += len;
          *by_language.entry(name).or_default() += 1;
        }
        None => {
          skipped += 1;
          continue;
        }
      }
      let new = blob(&change.new).expect("read a moment ago");
      let base = change.old.as_ref().and_then(|old| blob(old)).unwrap_or_default();
      delta_base_fetches += !base.is_empty() as usize;
      // The old version as a raw dictionary, with a window wide enough to reach all of it.
      let mut compressor = zstd::bulk::Compressor::with_dictionary(3, &base).expect("a raw dictionary");
      let window = (base.len() + new.len()).max(1 << 10).next_power_of_two().trailing_zeros().clamp(10, 27);
      compressor.set_parameter(zstd::zstd_safe::CParameter::WindowLog(window)).expect("a valid window");
      delta_bytes += compressor.compress(&new).expect("zstd compresses").len();
    }
  }

  println!(
    "{repo}: {} commits, {versions} new file versions with a grammar ({skipped} skipped), {:.1} MB",
    commits.len(),
    version_bytes as f64 / 1e6
  );
  let top: Vec<String> = {
    let mut v: Vec<_> = by_language.iter().collect();
    v.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    v.iter().take(5).map(|(l, n)| format!("{l} {n}")).collect()
  };
  println!("  languages: {}", top.join(", "));
  println!(
    "  {:<18} {:>9} {:>8} {:>10} {:>9} {:>9} {:>10} {:>8}",
    "scheme", "new MB", "reused", "avg chunk", "zstd MB", "list MB", "stored MB", "vs file"
  );
  let mut file_stored = 0.0;
  for s in &mut schemes {
    s.seal();
    let stored = (s.packed_bytes + s.list_bytes) as f64;
    if s.name == "file" {
      file_stored = stored;
    }
    println!(
      "  {:<18} {:>9.2} {:>7.1}% {:>10.0} {:>9.2} {:>9.2} {:>10.2} {:>7.1}%",
      s.name,
      s.new_bytes as f64 / 1e6,
      100.0 * (1.0 - s.new_bytes as f64 / version_bytes as f64),
      version_bytes as f64 / s.listed_chunks.max(1) as f64,
      s.packed_bytes as f64 / 1e6,
      s.list_bytes as f64 / 1e6,
      stored / 1e6,
      100.0 * stored / file_stored,
    );
  }
  println!(
    "  {:<18} {:>9} {:>8} {:>10} {:>9.2} {:>9} {:>10.2} {:>7.1}%   ({delta_base_fetches} of {versions} versions had a base)",
    "delta vs previous",
    "",
    "",
    "",
    delta_bytes as f64 / 1e6,
    "",
    delta_bytes as f64 / 1e6,
    100.0 * delta_bytes as f64 / file_stored,
  );
  let all: usize = dirs_touched.values().sum();
  let shares: Vec<String> =
    dirs_touched.iter().map(|(n, c)| format!("{}{n}: {:.0}%", if *n == 8 { ">=" } else { "" }, 100.0 * *c as f64 / all as f64)).collect();
  println!("  second-level directories touched per commit: {}", shares.join(", "));
}
