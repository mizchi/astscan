//! Results kept between runs: what a file matched, keyed by what it contained.
//!
//! In CI most files are the same from one run to the next, and for those the
//! answer is already known. One cache file belongs to one rule set (its name
//! is the rule set's hash), and holds a record per scanned file: the hash of
//! the file's path and bytes, the match counts, and a payload the caller
//! defines. The scanner stores the file's matches there (`record.rs`) and on
//! a hit writes its output from them, skipping parsing and matching.
//!
//! A payload of 128 bytes or more is stored zstd-compressed, each record on
//! its own.
//!
//! The file is an append-only log, mapped read-only. A run appends the
//! records of the files it had to scan. Nothing in it is trusted further
//! than it checks out: a record that runs past the end of the file is cut
//! off, and a payload whose checksum fails is a miss.
//!
//! The cache is best-effort. Any I/O failure turns writing off for the run
//! and never changes what is reported.

use memmap2::Mmap;
use std::collections::HashMap;
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use xxhash_rust::xxh3::{Xxh3, xxh3_64, xxh3_128_with_seed};

const MAGIC: &[u8; 8] = b"astscan\0";
const FORMAT: u32 = 3;
/// Magic, format, four reserved bytes, the rule set's key.
const HEADER: usize = 32;
/// Key, matches, error matches, stored length, output length, checksum of
/// the stored bytes, flags.
const RECORD: usize = 40;
/// Flag: the stored bytes are a zstd frame.
const ZSTD: u32 = 1;
const LEVEL: i32 = 3;
/// Below this a payload is stored as it is.
const COMPRESS_FROM: usize = 128;
/// Dead records are left alone until they weigh this much.
const COMPACT_ABOVE: u64 = 8 << 20;
/// A cache file of another rule set is deleted once it is this old.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// Keys are already hashes; hashing them again buys nothing.
#[derive(Default)]
struct KeyHasher(u64);

impl Hasher for KeyHasher {
  fn finish(&self) -> u64 {
    self.0
  }
  fn write(&mut self, bytes: &[u8]) {
    for chunk in bytes.chunks(8) {
      let mut word = [0u8; 8];
      word[..chunk.len()].copy_from_slice(chunk);
      self.0 ^= u64::from_le_bytes(word);
    }
  }
  fn write_u128(&mut self, key: u128) {
    self.0 = key as u64;
  }
}

struct Slot {
  /// Where the payload starts in the file.
  offset: usize,
  len: u32,
  raw_len: u32,
  matches: u32,
  errors: u32,
  checksum: u32,
  flags: u32,
  /// Served during this run: what a compaction keeps.
  used: AtomicBool,
}

pub struct Hit<'a> {
  /// As stored: read it with `Codec::read`.
  stored: &'a [u8],
  raw_len: u32,
  flags: u32,
  pub matches: u32,
  pub errors: u32,
}

/// A worker's compressor and decompressor, kept from file to file.
pub struct Codec {
  compressor: zstd::bulk::Compressor<'static>,
  decompressor: zstd::bulk::Decompressor<'static>,
  scratch: Vec<u8>,
}

impl Default for Codec {
  fn default() -> Self {
    Codec {
      compressor: zstd::bulk::Compressor::new(LEVEL).expect("a level zstd has"),
      decompressor: zstd::bulk::Decompressor::new().expect("no options to reject"),
      scratch: Vec::new(),
    }
  }
}

impl Codec {
  /// Append one record to `out`.
  pub fn write_record(&mut self, out: &mut Vec<u8>, key: u128, matches: u32, errors: u32, payload: &[u8]) {
    let mut stored = payload;
    let mut flags = 0;
    if payload.len() >= COMPRESS_FROM {
      self.scratch.clear();
      self.scratch.reserve(zstd::zstd_safe::compress_bound(payload.len()));
      if self.compressor.compress_to_buffer(payload, &mut self.scratch).is_ok() && self.scratch.len() < payload.len() {
        stored = &self.scratch;
        flags = ZSTD;
      }
    }
    out.extend_from_slice(&key.to_le_bytes());
    out.extend_from_slice(&matches.to_le_bytes());
    out.extend_from_slice(&errors.to_le_bytes());
    out.extend_from_slice(&(stored.len() as u32).to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&(xxh3_64(stored) as u32).to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(stored);
  }

  /// Append the output a hit stands for to `out`. `false`, with `out` as it
  /// was, when the stored bytes do not decode to what the record promised.
  pub fn read(&mut self, hit: &Hit, out: &mut Vec<u8>) -> bool {
    if hit.flags & ZSTD == 0 {
      out.extend_from_slice(hit.stored);
      return hit.stored.len() == hit.raw_len as usize;
    }
    let at = out.len();
    out.resize(at + hit.raw_len as usize, 0);
    let ok = self.decompressor.decompress_to_buffer(hit.stored, &mut out[at..]).is_ok_and(|n| n == hit.raw_len as usize);
    if !ok {
      out.truncate(at);
    }
    ok
  }
}

enum Writer {
  Unopened,
  Open(File),
  Off,
}

pub struct Cache {
  path: PathBuf,
  ruleset: u128,
  map: Option<Mmap>,
  index: HashMap<u128, Slot, BuildHasherDefault<KeyHasher>>,
  /// Length of the file when it was mapped, and of its prefix of whole records.
  file_len: u64,
  valid_len: u64,
  writer: Mutex<Writer>,
  pub hits: AtomicUsize,
  pub misses: AtomicUsize,
}

/// The key of a rule set: everything that decides what a file's output is,
/// apart from the file. Each part is length-prefixed so parts cannot run together.
pub fn ruleset_key(parts: &[&[u8]]) -> u128 {
  let mut hasher = Xxh3::new();
  for part in parts {
    hasher.update(&(part.len() as u64).to_le_bytes());
    hasher.update(part);
  }
  hasher.digest128()
}

/// The key of one file under this rule set: its bytes, and what it is
/// scanned as, since the same bytes under another extension meet other rules.
/// Not its path: a file that moved, or a second copy of it, is the same file.
#[inline]
pub fn file_key(scanned_as: u64, content: &[u8]) -> u128 {
  xxh3_128_with_seed(content, scanned_as)
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
  u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn header(ruleset: u128) -> [u8; HEADER] {
  let mut h = [0u8; HEADER];
  h[..8].copy_from_slice(MAGIC);
  h[8..12].copy_from_slice(&FORMAT.to_le_bytes());
  h[16..].copy_from_slice(&ruleset.to_le_bytes());
  h
}

impl Cache {
  /// Open the cache of `ruleset` under `dir`. A missing or unreadable file is an empty cache.
  pub fn open(dir: &Path, ruleset: u128) -> Cache {
    let mut cache = Cache {
      path: dir.join(format!("{ruleset:032x}.cache")),
      ruleset,
      map: None,
      index: HashMap::default(),
      file_len: 0,
      valid_len: 0,
      writer: Mutex::new(Writer::Unopened),
      hits: AtomicUsize::new(0),
      misses: AtomicUsize::new(0),
    };
    let Ok(file) = File::open(&cache.path) else { return cache };
    cache.file_len = file.metadata().map_or(0, |m| m.len());
    // SAFETY: the mapping is read-only, and writers only ever append to this
    // file or replace it by rename; the one truncation they make cuts bytes
    // past `valid_len`, which no reader indexes.
    let Ok(map) = (unsafe { Mmap::map(&file) }) else { return cache };
    if map.len() < HEADER || map[..HEADER] != header(ruleset) {
      return cache;
    }
    let mut at = HEADER;
    while at + RECORD <= map.len() {
      let key = u128::from_le_bytes(map[at..at + 16].try_into().expect("sixteen bytes"));
      let len = u32_at(&map, at + 24);
      let end = at + RECORD + len as usize;
      if end > map.len() {
        break;
      }
      // A file scanned again after a failed checksum has two records; the later one stands.
      cache.index.insert(
        key,
        Slot {
          offset: at + RECORD,
          len,
          raw_len: u32_at(&map, at + 28),
          matches: u32_at(&map, at + 16),
          errors: u32_at(&map, at + 20),
          checksum: u32_at(&map, at + 32),
          flags: u32_at(&map, at + 36),
          used: AtomicBool::new(false),
        },
      );
      at = end;
    }
    cache.valid_len = at as u64;
    cache.map = Some(map);
    cache
  }

  /// What the file with this key printed last time, if that is known and
  /// intact. The caller counts it as a hit once it has read it.
  pub fn get(&self, key: u128) -> Option<Hit<'_>> {
    let slot = self.index.get(&key)?;
    let map = self.map.as_ref()?;
    let stored = &map[slot.offset..slot.offset + slot.len as usize];
    if xxh3_64(stored) as u32 != slot.checksum {
      return None;
    }
    slot.used.store(true, Ordering::Relaxed);
    Some(Hit { stored, raw_len: slot.raw_len, flags: slot.flags, matches: slot.matches, errors: slot.errors })
  }

  /// Open the file for appending, repairing or starting it as needed.
  /// `None` when the cache cannot be written this run.
  fn open_writer(&self) -> Option<File> {
    let dir = self.path.parent()?;
    std::fs::create_dir_all(dir).ok()?;
    let fresh = !self.path.exists();
    let mut file = File::options().read(true).write(true).create(true).truncate(false).open(&self.path).ok()?;
    // One writer at a time; a second run going on at once just does not save.
    file.try_lock().ok()?;
    // Somebody wrote since the file was mapped: what it holds now is unknown.
    if file.metadata().ok()?.len() != self.file_len {
      return None;
    }
    if self.valid_len == 0 {
      file.set_len(0).ok()?;
      file.write_all(&header(self.ruleset)).ok()?;
    } else if self.valid_len < self.file_len {
      file.set_len(self.valid_len).ok()?;
    }
    file.seek(SeekFrom::End(0)).ok()?;
    if fresh {
      self.remove_stale_siblings(dir);
    }
    Some(file)
  }

  /// A rule set that changed leaves its old cache behind. Clear out the ones
  /// nobody has written to for a while, at the moment a new one is started.
  fn remove_stale_siblings(&self, dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
      let path = entry.path();
      if path == self.path || path.extension().is_none_or(|e| e != "cache") {
        continue;
      }
      let age = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
      if age.is_some_and(|age| age > STALE_AFTER) {
        let _ = std::fs::remove_file(path);
      }
    }
  }

  /// Append whole records, and empty `records`.
  pub fn append(&self, records: &mut Vec<u8>) {
    if records.is_empty() {
      return;
    }
    let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
    if matches!(*writer, Writer::Unopened) {
      *writer = self.open_writer().map_or(Writer::Off, Writer::Open);
    }
    if let Writer::Open(file) = &mut *writer
      && file.write_all(records).is_err()
    {
      *writer = Writer::Off;
    }
    records.clear();
  }

  /// End of the run. When the records this run did not use outweigh the
  /// ones it did, write the file anew without them.
  pub fn finish(&self) {
    let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
    let Writer::Open(file) = &mut *writer else { return };
    let Some(map) = &self.map else { return };
    let appended_from = self.valid_len.max(HEADER as u64);
    let Ok(now) = file.metadata().map(|m| m.len()) else { return };
    let appended = now.saturating_sub(appended_from);
    let live: u64 = self.index.values().filter(|s| s.used.load(Ordering::Relaxed)).map(|s| RECORD as u64 + s.len as u64).sum();
    let dead = self.valid_len.saturating_sub(HEADER as u64 + live);
    if dead < COMPACT_ABOVE || dead < live + appended {
      return;
    }
    let temp = self.path.with_extension(format!("tmp{}", std::process::id()));
    let mut rewrite = || -> std::io::Result<()> {
      let mut out = std::io::BufWriter::new(File::create(&temp)?);
      out.write_all(&header(self.ruleset))?;
      for slot in self.index.values().filter(|s| s.used.load(Ordering::Relaxed)) {
        out.write_all(&map[slot.offset - RECORD..slot.offset + slot.len as usize])?;
      }
      file.seek(SeekFrom::Start(appended_from))?;
      std::io::copy(file, &mut out)?;
      out.into_inner().map_err(|e| e.into_error())?.sync_data()?;
      std::fs::rename(&temp, &self.path)
    };
    if rewrite().is_err() {
      let _ = std::fs::remove_file(&temp);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("astscan-cache-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
  }

  fn store(cache: &Cache, entries: &[(u128, &[u8])]) {
    let (mut codec, mut records) = (Codec::default(), Vec::new());
    for (key, payload) in entries {
      codec.write_record(&mut records, *key, 1, 0, payload);
    }
    cache.append(&mut records);
  }

  fn output(cache: &Cache, key: u128) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    Codec::default().read(&cache.get(key)?, &mut out).then_some(out)
  }

  /// Bytes zstd cannot shrink, so a record of them is as large as they are.
  fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    (0..len)
      .map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u8
      })
      .collect()
  }

  #[test]
  fn a_repetitive_payload_is_stored_smaller_and_read_back_whole() {
    let dir = dir("zstd");
    let line = br#"{"file":"src/a.ts","language":"TypeScript","range":{"byteOffset":{"start":0,"end":14}},"ruleIds":["log"]}"#;
    let payload: Vec<u8> = (0..1000).flat_map(|_| line.iter().copied().chain(*b"\n")).collect();
    let cache = Cache::open(&dir, 1);
    store(&cache, &[(1, &payload), (2, b"short\n"), (3, &noise(4096))]);
    drop(cache);
    let size = std::fs::metadata(dir.join(format!("{:032x}.cache", 1))).unwrap().len() as usize;
    assert!(size < payload.len() / 10 + 4096 + 3 * RECORD + HEADER + 16, "{size} bytes for {} of repeats", payload.len());
    let cache = Cache::open(&dir, 1);
    assert_eq!(output(&cache, 1).expect("stored"), payload);
    assert_eq!(output(&cache, 2).expect("stored"), b"short\n");
    assert_eq!(output(&cache, 3).expect("stored"), noise(4096));
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn keys_tell_path_and_content_apart() {
    assert_ne!(file_key(0, b"x"), file_key(1, b"x"));
    assert_ne!(file_key(0, b"x"), file_key(0, b"y"));
    assert_eq!(file_key(0, b"x"), file_key(0, b"x"));
    assert_ne!(ruleset_key(&[b"ab", b"c"]), ruleset_key(&[b"a", b"bc"]));
  }

  #[test]
  fn a_record_written_is_a_record_read() {
    let dir = dir("roundtrip");
    let cache = Cache::open(&dir, 7);
    assert!(cache.get(1).is_none());
    store(&cache, &[(1, b"one\n"), (2, b"")]);
    drop(cache);
    let cache = Cache::open(&dir, 7);
    let hit = cache.get(1).expect("stored");
    assert_eq!((hit.matches, hit.errors), (1, 0));
    assert_eq!(output(&cache, 1).expect("stored"), b"one\n");
    assert_eq!(output(&cache, 2).expect("stored"), b"");
    assert!(cache.get(3).is_none());
    // Another rule set has another file.
    assert!(Cache::open(&dir, 8).get(1).is_none());
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn compaction_drops_what_a_run_did_not_use() {
    let dir = dir("compact");
    let big = noise(COMPACT_ABOVE as usize);
    let cache = Cache::open(&dir, 1);
    store(&cache, &[(1, b"keep\n"), (2, &big)]);
    drop(cache);
    let before = std::fs::metadata(dir.join(format!("{:032x}.cache", 1))).unwrap().len();

    let cache = Cache::open(&dir, 1);
    assert!(cache.get(1).is_some());
    store(&cache, &[(3, b"new\n")]);
    cache.finish();
    drop(cache);
    let after = std::fs::metadata(dir.join(format!("{:032x}.cache", 1))).unwrap().len();
    assert!(after < before / 2, "{after} of {before} bytes left");

    let cache = Cache::open(&dir, 1);
    assert_eq!(output(&cache, 1).expect("used, so kept"), b"keep\n");
    assert_eq!(output(&cache, 3).expect("new, so kept"), b"new\n");
    assert!(cache.get(2).is_none());
    let _ = std::fs::remove_dir_all(&dir);
  }
}
