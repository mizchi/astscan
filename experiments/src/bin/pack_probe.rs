//! How should versions of source files be compressed in a store that wants
//! both small size and cheap access to any one version?
//!
//! A delta against the previous version is the smallest, but reading a
//! version then means reading the chain behind it. This replays the last N
//! commits of a Git repository and stores every new file version under
//! several policies, reporting the bytes stored and the bytes a reader has to
//! fetch to get one version back.
//!
//!   cargo run --release -p astscan-experiments --bin pack_probe -- <repo> [commits=2000]
//!
//! Policies:
//! - alone: each version compressed by itself.
//! - dictionary: each version compressed with one shared dictionary, trained
//!   on the tree as it was before the first replayed commit. No chains.
//! - chain: a delta against the previous version of the same path, with a
//!   full copy every K versions (K = unbounded is what a Git pack tends to).
//! - keyframe: a delta against the path's last full copy, renewed every K
//!   versions. Every read is two fetches at most.
//!
//! Reads only what the clone holds: in a partial clone the blobs it lacks are
//! skipped, never fetched.
//!
//! A full copy is compressed with the dictionary. A delta is zstd with the
//! base as a raw dictionary, which is what `zstd --patch-from` does.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

const LEVEL: i32 = 3;
const DICTIONARY_SIZE: usize = 112 << 10;
const MAX_FILE: usize = 3_000_000;

fn delta(base: &[u8], new: &[u8]) -> usize {
  let mut c = zstd::bulk::Compressor::with_dictionary(LEVEL, base).expect("a raw dictionary");
  // A window wide enough to reach all of the base.
  let window = (base.len() + new.len()).max(1 << 10).next_power_of_two().trailing_zeros().clamp(10, 27);
  c.set_parameter(zstd::zstd_safe::CParameter::WindowLog(window)).expect("a valid window");
  c.compress(new).expect("zstd compresses").len()
}

#[derive(Default)]
struct Policy {
  name: String,
  /// Versions between full copies; 0 for never.
  every: usize,
  /// Deltas are taken against the last full copy, not the previous version.
  keyframe: bool,
  stored: usize,
  /// Sum over versions of the bytes a reader fetches for that version.
  fetched: usize,
  longest_chain: usize,
  paths: HashMap<String, PathState>,
}

#[derive(Default)]
struct PathState {
  /// What a delta is taken against.
  base: Vec<u8>,
  /// Versions stored since the last full copy.
  since_full: usize,
  /// Bytes a reader fetches for the current base: the chain so far.
  chain_bytes: usize,
  chain_len: usize,
}

fn main() {
  let mut args = std::env::args().skip(1);
  let repo = args.next().expect("usage: pack_probe <repo> [commits]");
  let limit = args.next().unwrap_or_else(|| "2000".to_string());
  let git = |a: &[&str]| -> String {
    let out = Command::new("git").env("GIT_NO_LAZY_FETCH", "1").arg("-C").arg(&repo).args(a).output().expect("git runs");
    String::from_utf8_lossy(&out.stdout).into_owned()
  };

  // `(path, old, new)` per change, oldest commit first.
  let mut commits: Vec<Vec<(String, Option<String>, String)>> = Vec::new();
  let mut oldest = String::new();
  for line in git(&["log", "--no-merges", "-n", &limit, "--raw", "--no-renames", "--abbrev=40", "--format=C %H"]).lines() {
    if let Some(hash) = line.strip_prefix("C ") {
      commits.push(Vec::new());
      oldest = hash.to_string();
    } else if let Some(rest) = line.strip_prefix(':') {
      let (meta, path) = rest.split_once('\t').expect("a raw diff line has a path");
      let f: Vec<&str> = meta.split(' ').collect();
      let old = match f[4] {
        "M" => Some(f[2].to_string()),
        "A" => None,
        _ => continue,
      };
      commits.last_mut().expect("a commit line comes first").push((path.to_string(), old, f[3].to_string()));
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
    (!bytes.is_empty() && bytes.len() <= MAX_FILE && std::str::from_utf8(&bytes).is_ok()).then_some(bytes)
  };

  // Train on the tree the replay starts from: text files, a spread of them.
  let listing = git(&["ls-tree", "-r", &format!("{oldest}^")]);
  let entries: Vec<&str> = listing.lines().filter_map(|l| l.split_whitespace().nth(2)).collect();
  let step = (entries.len() / 4000).max(1);
  let samples: Vec<Vec<u8>> = entries.iter().step_by(step).filter_map(|oid| blob(oid)).filter(|b| b.len() <= 64 << 10).collect();
  let dictionary = zstd::dict::from_samples(&samples, DICTIONARY_SIZE).expect("enough samples to train on");
  let mut with_dictionary = zstd::bulk::Compressor::with_dictionary(LEVEL, &dictionary).expect("a trained dictionary");
  let mut alone = zstd::bulk::Compressor::new(LEVEL).expect("a level zstd has");

  let policy = |name: &str, every: usize, keyframe: bool| Policy { name: name.to_string(), every, keyframe, ..Policy::default() };
  let mut policies = vec![
    policy("chain, unbounded", 0, false),
    policy("chain, full every 64", 64, false),
    policy("chain, full every 16", 16, false),
    policy("chain, full every 4", 4, false),
    policy("keyframe every 64", 64, true),
    policy("keyframe every 16", 16, true),
    policy("keyframe every 4", 4, true),
  ];
  let (mut versions, mut raw, mut alone_bytes, mut dictionary_bytes) = (0usize, 0usize, 0usize, 0usize);

  for commit in &commits {
    for (path, old, new) in commit {
      let Some(new_bytes) = blob(new) else { continue };
      versions += 1;
      raw += new_bytes.len();
      alone_bytes += alone.compress(&new_bytes).expect("zstd compresses").len();
      let full = with_dictionary.compress(&new_bytes).expect("zstd compresses").len();
      dictionary_bytes += full;
      for p in &mut policies {
        let known = p.paths.contains_key(path);
        let state = p.paths.entry(path.clone()).or_default();
        // A path first seen as a modification: its old version is in the store, as a full copy.
        if !known && let Some(old_bytes) = old.as_ref().and_then(|o| blob(o)) {
          state.chain_bytes = with_dictionary.compress(&old_bytes).expect("zstd compresses").len();
          state.base = old_bytes;
          state.since_full = 1;
        }
        let renew = state.base.is_empty() || (p.every > 0 && state.since_full >= p.every);
        if renew {
          p.stored += full;
          p.fetched += full;
          *state = PathState { base: new_bytes.clone(), since_full: 1, chain_bytes: full, chain_len: 0 };
          continue;
        }
        let size = delta(&state.base, &new_bytes);
        p.stored += size;
        p.fetched += state.chain_bytes + size;
        state.since_full += 1;
        if p.keyframe {
          p.longest_chain = p.longest_chain.max(1);
        } else {
          // The next delta is against this version, which sits one link further down.
          state.base = new_bytes.clone();
          state.chain_bytes += size;
          state.chain_len += 1;
          p.longest_chain = p.longest_chain.max(state.chain_len);
        }
      }
    }
  }

  let mb = |n: usize| n as f64 / 1e6;
  println!(
    "{repo}: {} commits, {versions} text file versions, {:.1} MB; dictionary {} KB from {} files",
    commits.len(),
    mb(raw),
    dictionary.len() >> 10,
    samples.len()
  );
  println!("  {:<22} {:>10} {:>8} {:>16} {:>14}", "policy", "stored MB", "of raw", "fetch per read", "longest chain");
  let row = |name: &str, stored: usize, fetched: usize, chain: usize| {
    println!(
      "  {:<22} {:>10.2} {:>7.2}% {:>13.0} B {:>14}",
      name,
      mb(stored),
      100.0 * stored as f64 / raw as f64,
      fetched as f64 / versions as f64,
      chain
    );
  };
  row("alone", alone_bytes, alone_bytes, 0);
  row("dictionary", dictionary_bytes, dictionary_bytes, 0);
  for p in &policies {
    row(&p.name, p.stored, p.fetched, p.longest_chain);
  }
}
