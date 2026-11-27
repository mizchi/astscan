//! File I/O, cache replay and JSON output for CLI workers.

use crate::cache::{Cache, Codec, file_key};
use crate::output::{LineIndex, Mode, write_json_str, write_lines, write_range};
use crate::record::{Found, Item, Reader, Span, write_found, write_unit};
use astscan_core::internals::{ScanOptions, Target};
use astscan_core::{RuleSet, ScanError, Scanner, Severity};
use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

const MAX_FILE_SIZE: usize = 3_000_000;
const MAX_LINE_COUNT: usize = 200_000;
/// Output is handed to stdout once a worker holds this much.
const FLUSH_AT: usize = 1 << 18;
/// Cache records are handed to the cache file once a worker holds this much.
const STORE_AT: usize = 1 << 22;

pub struct Compiled {
  pub rules: RuleSet,
  output: Vec<OutputUnit>,
}

struct OutputUnit {
  language_json: Vec<u8>,
  groups: Vec<Vec<JsonRule>>,
  var_keys: Vec<Vec<u8>>,
}

struct JsonRule {
  id_json: Vec<u8>,
  tail: Vec<u8>,
  is_error: bool,
}

impl Compiled {
  pub fn new(rules: RuleSet) -> Self {
    let output = rules
      .units()
      .iter()
      .map(|unit| {
        let groups = unit
          .groups()
          .map(|metas| {
            metas
              .iter()
              .map(|meta| {
                let mut id_json = Vec::new();
                write_json_str(&mut id_json, meta.id.as_bytes());
                let mut tail = b",\"ruleId\":".to_vec();
                tail.extend_from_slice(&id_json);
                tail.extend_from_slice(format!(",\"severity\":\"{}\",\"note\":", meta.severity.as_str()).as_bytes());
                match &meta.note {
                  Some(note) => write_json_str(&mut tail, note.as_bytes()),
                  None => tail.extend_from_slice(b"null"),
                }
                tail.extend_from_slice(b",\"message\":");
                write_json_str(&mut tail, meta.message.as_bytes());
                JsonRule { id_json, tail, is_error: meta.severity == Severity::Error }
              })
              .collect()
          })
          .collect();
        let var_keys = unit
          .variables()
          .iter()
          .map(|name| {
            let mut key = Vec::new();
            write_json_str(&mut key, name.as_bytes());
            key.push(b':');
            key
          })
          .collect();
        let mut language_json = Vec::new();
        write_json_str(&mut language_json, unit.language().as_bytes());
        OutputUnit { language_json, groups, var_keys }
      })
      .collect();
    Self { rules, output }
  }

  pub fn group_count(&self) -> usize {
    self.output.iter().map(|unit| unit.groups.len()).sum()
  }
}

pub struct Shared<'a> {
  /// Set by the background rule compiler.
  pub rules: &'a OnceLock<Result<Compiled, String>>,
  pub mode: Mode,
  pub cache: Option<&'a Cache>,
  pub error_matches: AtomicUsize,
  pub match_count: AtomicUsize,
  pub file_count: AtomicUsize,
  pub byte_count: AtomicUsize,
  pub options: ScanOptions,
  pub fast_parsed: [AtomicUsize; 2],
  pub phase_nanos: [AtomicUsize; 3],
  pub skipped_parses: AtomicUsize,
  pub skipped_bytes: AtomicUsize,
  pub rejected_groups: AtomicUsize,
  pub prefilter_nanos: AtomicUsize,
  pub slow: Option<std::sync::Mutex<Vec<(u128, String)>>>,
}

impl<'a> Shared<'a> {
  /// The compiled rules, waiting for them if need be. `None` if compilation failed.
  fn compiled(&self) -> Option<&'a Compiled> {
    self.rules.wait().as_ref().ok()
  }
}

fn flush(out: &mut Vec<u8>) {
  if out.is_empty() {
    return;
  }
  let mut stdout = std::io::stdout().lock();
  if stdout.write_all(out).is_err() {
    // The reader went away; there is nobody left to report to.
    std::process::exit(0);
  }
  out.clear();
}

pub struct Worker<'a> {
  shared: &'a Shared<'a>,
  scanner: Option<Scanner<'a>>,
  lines: LineIndex,
  out: Vec<u8>,
  head: Vec<u8>,
  text: Vec<u8>,
  path_json: Vec<u8>,
  src: Vec<u8>,
  /// Matches and error-severity matches of the file being scanned.
  matched: usize,
  errors: usize,
  /// Reusable match storage for cache replay.
  found: Found,
  /// Encoded matches of the file being scanned.
  entry: Vec<u8>,
  /// A cached entry, decompressed.
  payload: Vec<u8>,
  records: Vec<u8>,
  codec: Codec,
}

impl<'a> Worker<'a> {
  pub fn new(shared: &'a Shared<'a>) -> Self {
    Worker {
      shared,
      scanner: None,
      lines: LineIndex::default(),
      out: Vec::with_capacity(FLUSH_AT * 2),
      head: Vec::new(),
      text: Vec::new(),
      path_json: Vec::new(),
      src: Vec::new(),
      matched: 0,
      errors: 0,
      found: Found::default(),
      entry: Vec::new(),
      payload: Vec::new(),
      records: Vec::new(),
      codec: Codec::default(),
    }
  }

  pub fn flush(&mut self) {
    flush(&mut self.out);
  }

  pub fn scan_file(&mut self, path: &Path, target: Target) {
    let mut src = std::mem::take(&mut self.src);
    src.clear();
    let read = std::fs::File::open(path).and_then(|mut f| std::io::Read::read_to_end(&mut f, &mut src));
    if read.is_ok() && !src.is_empty() {
      self.scan_bytes(&src, path, target);
    }
    self.src = src;
    if self.out.len() >= FLUSH_AT {
      self.flush();
    }
  }

  /// Answer from the cache when the file is known, scan it and remember the answer when not.
  fn scan_bytes(&mut self, src: &[u8], path: &Path, target: Target) {
    let shared = self.shared;
    let display = path.to_string_lossy();
    // `ignore` yields `./a/b` for the root `.`; ast-grep reports `a/b`.
    let display = display.strip_prefix("./").unwrap_or(&display);
    let scanned_as = match target {
      Target::Unit(unit) => unit as u64,
      Target::Html => u64::MAX,
    };
    let key = shared.cache.map(|cache| (cache, file_key(scanned_as, src)));
    (self.matched, self.errors) = (0, 0);
    if let Some((cache, key)) = key
      && let Some(hit) = cache.get(key)
    {
      let mut payload = std::mem::take(&mut self.payload);
      payload.clear();
      let replayed = self.codec.read(&hit, &mut payload) && self.replay(&payload, src, display);
      self.payload = payload;
      if replayed {
        cache.hits.fetch_add(1, Ordering::Relaxed);
        shared.file_count.fetch_add(1, Ordering::Relaxed);
        shared.byte_count.fetch_add(src.len(), Ordering::Relaxed);
        shared.match_count.fetch_add(self.matched, Ordering::Relaxed);
        shared.error_matches.fetch_add(self.errors, Ordering::Relaxed);
        return;
      }
    }
    // Rules that did not compile have no answer to give, and none to remember.
    let Some(compiled) = shared.compiled() else { return };
    if let Some((cache, _)) = key {
      cache.misses.fetch_add(1, Ordering::Relaxed);
      self.entry.clear();
    }
    let complete = self.scan_text(src, display, target, compiled, key.is_some());
    if self.matched > 0 {
      shared.match_count.fetch_add(self.matched, Ordering::Relaxed);
    }
    if self.errors > 0 {
      shared.error_matches.fetch_add(self.errors, Ordering::Relaxed);
    }
    if complete && let Some((cache, key)) = key {
      self.codec.write_record(&mut self.records, key, self.matched as u32, self.errors as u32, &self.entry);
      if self.records.len() >= STORE_AT {
        cache.append(&mut self.records);
      }
    }
  }

  /// Write the matches a cached entry holds, as the scan that stored them
  /// wrote them. `false`, with nothing written, when the entry does not fit
  /// these rules and this file: the file is then scanned as if never seen.
  fn replay(&mut self, payload: &[u8], src: &[u8], path: &str) -> bool {
    let shared = self.shared;
    let Some(compiled) = shared.compiled() else { return false };
    if payload.is_empty() {
      return true;
    }
    // Once over everything before a byte is written: output cannot be taken back.
    let mut reader = Reader::new(payload);
    let mut unit: Option<&OutputUnit> = None;
    loop {
      match reader.next(&mut self.found) {
        Err(_) => return false,
        Ok(None) => break,
        Ok(Some(Item::Unit(u))) => unit = compiled.output.get(u as usize),
        Ok(Some(Item::Found)) => {
          let f = &self.found;
          let Some(group) = unit.and_then(|u| u.groups.get(f.group as usize)) else { return false };
          let vars = f.single.iter().map(|(v, _)| v).chain(f.multi.iter().map(|(v, _, _)| v));
          if f.extent().is_none_or(|end| end as usize > src.len())
            || f.dead.iter().any(|&i| i as usize >= group.len())
            || vars.into_iter().any(|&v| v as usize >= unit.map_or(0, |u| u.var_keys.len()))
          {
            return false;
          }
        }
      }
      if unit.is_none() {
        return false;
      }
    }

    self.lines.build(src);
    self.path_json.clear();
    write_json_str(&mut self.path_json, path.as_bytes());
    let mut reader = Reader::new(payload);
    let mut unit = &compiled.output[0];
    while let Ok(Some(item)) = reader.next(&mut self.found) {
      match item {
        Item::Unit(u) => unit = &compiled.output[u as usize],
        Item::Found => {
          let mut e = Emit {
            out: &mut self.out,
            head: &mut self.head,
            text: &mut self.text,
            lines: &self.lines,
            src,
            path_json: &self.path_json,
            mode: shared.mode,
            unit,
            keys: &unit.var_keys,
          };
          let (count, errs) = emit(&mut e, &unit.groups[self.found.group as usize], &self.found);
          self.matched += count;
          self.errors += errs;
          if self.out.len() >= FLUSH_AT {
            flush(&mut self.out);
          }
        }
      }
    }
    true
  }

  fn scan_text(&mut self, src: &[u8], display: &str, target: Target, compiled: &'a Compiled, recording: bool) -> bool {
    // A file that is not UTF-8 is skipped, as is one both huge and long.
    let Ok(text) = simdutf8::basic::from_utf8(src) else { return true };
    if text.len() > MAX_FILE_SIZE && text.lines().count() > MAX_LINE_COUNT {
      return true;
    }

    let shared = self.shared;
    let scanner = self.scanner.get_or_insert_with(|| Scanner::with_options(&compiled.rules, shared.options));
    let started = shared.slow.as_ref().map(|_| std::time::Instant::now());
    let mut lines_built = false;
    let mut recorded_unit = None;
    let stats = scanner.scan_target(text, target, |idx, found| {
      if !lines_built {
        self.lines.build(src);
        self.path_json.clear();
        write_json_str(&mut self.path_json, display.as_bytes());
        lines_built = true;
      }
      if recording {
        if recorded_unit != Some(idx) {
          recorded_unit = Some(idx);
          write_unit(&mut self.entry, idx as u32);
        }
        write_found(&mut self.entry, found);
      }
      let unit = &compiled.output[idx];
      let mut e = Emit {
        out: &mut self.out,
        head: &mut self.head,
        text: &mut self.text,
        lines: &self.lines,
        src,
        path_json: &self.path_json,
        mode: shared.mode,
        unit,
        keys: &unit.var_keys,
      };
      let (count, errors) = emit(&mut e, &unit.groups[found.group as usize], found);
      self.matched += count;
      self.errors += errors;
      if self.out.len() >= FLUSH_AT {
        flush(&mut self.out);
      }
    });
    match stats {
      Ok(stats) => {
        shared.file_count.fetch_add(stats.files, Ordering::Relaxed);
        shared.byte_count.fetch_add(stats.bytes, Ordering::Relaxed);
        if shared.options.timed {
          let prefilter = stats.prefilter;
          if prefilter.nanos > 0 || prefilter.rejected_groups > 0 {
            shared.skipped_parses.fetch_add(prefilter.skipped_parses, Ordering::Relaxed);
            shared.skipped_bytes.fetch_add(prefilter.skipped_bytes, Ordering::Relaxed);
            shared.rejected_groups.fetch_add(prefilter.rejected_groups, Ordering::Relaxed);
            shared.prefilter_nanos.fetch_add(prefilter.nanos, Ordering::Relaxed);
          }
          for (total, elapsed) in shared.phase_nanos.iter().zip(stats.phase_nanos) {
            total.fetch_add(elapsed, Ordering::Relaxed);
          }
        }
        if shared.options.parser == astscan_core::ParserKind::Fast {
          for (total, count) in shared.fast_parsed.iter().zip(stats.fast_parsed) {
            total.fetch_add(count, Ordering::Relaxed);
          }
        }
      }
      Err(ScanError::FastParserMismatch(diff)) => {
        eprintln!("astscan: fast parser mismatch in {display}: {diff}");
        std::process::exit(3);
      }
      Err(ScanError::FlattenMismatch(diff)) => {
        eprintln!("astscan: flatten mismatch in {display}: {diff}");
        std::process::exit(3);
      }
      Err(ScanError::ParseFailed | ScanError::SourceTooLarge) => {
        // A scan that did not finish must not be served by the cache.
        return false;
      }
    }
    if let (Some(slow), Some(started)) = (&shared.slow, started) {
      let mut slow = slow.lock().expect("no panic while holding the lock");
      slow.push((started.elapsed().as_micros(), display.to_string()));
      if slow.len() > 64 {
        slow.sort_unstable_by_key(|e| std::cmp::Reverse(e.0));
        slow.truncate(8);
      }
    }
    true
  }
}

impl Drop for Worker<'_> {
  fn drop(&mut self) {
    if let Some(cache) = self.shared.cache {
      cache.append(&mut self.records);
    }
    self.flush();
  }
}

struct Emit<'a, 'b> {
  out: &'b mut Vec<u8>,
  head: &'b mut Vec<u8>,
  text: &'b mut Vec<u8>,
  lines: &'b LineIndex,
  src: &'b [u8],
  path_json: &'b [u8],
  mode: Mode,
  unit: &'a OutputUnit,
  /// Metavariable names as JSON object keys.
  keys: &'a [Vec<u8>],
}

impl Emit<'_, '_> {
  fn node(&self, out: &mut Vec<u8>, (s, e): Span, with_text: bool) {
    out.push(b'{');
    if with_text {
      out.extend_from_slice(b"\"text\":");
      write_json_str(out, &self.src[s as usize..e as usize]);
      out.push(b',');
    }
    out.extend_from_slice(b"\"range\":");
    write_range(out, self.lines.range(self.src, s, e));
    out.push(b'}');
  }

  /// `,"metaVariables":{…}`, or nothing when the match captured nothing.
  fn meta_variables(&self, out: &mut Vec<u8>, f: &Found, with_text: bool) {
    if f.single.is_empty() && f.multi.is_empty() && f.labels.is_empty() {
      return;
    }
    let keys = self.keys;
    out.extend_from_slice(b",\"metaVariables\":{\"single\":{");
    let mut first = true;
    for (i, &(var, span)) in f.single.iter().enumerate() {
      // A later binding of the same variable shadows this one.
      if f.single[i + 1..].iter().any(|(v, _)| *v == var) {
        continue;
      }
      if !std::mem::take(&mut first) {
        out.push(b',');
      }
      out.extend_from_slice(&keys[var as usize]);
      self.node(out, span, with_text);
    }
    out.extend_from_slice(b"},\"multi\":{");
    first = true;
    for (i, &(var, start, len)) in f.multi.iter().enumerate() {
      if f.multi[i + 1..].iter().any(|(v, _, _)| *v == var) {
        continue;
      }
      if !std::mem::take(&mut first) {
        out.push(b',');
      }
      out.extend_from_slice(&keys[var as usize]);
      self.node_list(out, &f.pool[start as usize..(start + len) as usize], with_text);
    }
    if !f.labels.is_empty() {
      if !first {
        out.push(b',');
      }
      out.extend_from_slice(b"\"secondary\":");
      self.node_list(out, &f.labels, with_text);
    }
    out.extend_from_slice(b"},\"transformed\":{}}");
  }

  fn node_list(&self, out: &mut Vec<u8>, spans: &[Span], with_text: bool) {
    out.push(b'[');
    for (i, &span) in spans.iter().enumerate() {
      if i > 0 {
        out.push(b',');
      }
      self.node(out, span, with_text);
    }
    out.push(b']');
  }
}

/// Write one match under every rule of its group that is live here. Returns
/// `(lines, error-severity lines)`.
fn emit(e: &mut Emit, rules: &[JsonRule], f: &Found) -> (usize, usize) {
  let (start, end) = (f.start, f.end);
  let range = e.lines.range(e.src, start, end);
  let live = |i: usize| !f.dead.contains(&(i as u32));
  let metas = || rules.iter().enumerate().filter(|(i, _)| live(*i)).map(|(_, m)| m);
  let mut count = 0;
  let mut errors = 0;

  let mut head = std::mem::take(e.head);
  head.clear();
  match e.mode {
    Mode::Slim => {
      head.extend_from_slice(b"{\"file\":");
      head.extend_from_slice(e.path_json);
      head.extend_from_slice(b",\"language\":");
      head.extend_from_slice(&e.unit.language_json);
      head.extend_from_slice(b",\"range\":");
      write_range(&mut head, range);
      e.meta_variables(&mut head, f, false);
      head.extend_from_slice(b",\"ruleIds\":[");
      for meta in metas() {
        if count > 0 {
          head.push(b',');
        }
        head.extend_from_slice(&meta.id_json);
        count += 1;
        errors += meta.is_error as usize;
      }
      head.extend_from_slice(b"]}\n");
      // Every rule of the group silenced: there is no line to write.
      if count > 0 {
        e.out.extend_from_slice(&head);
      }
    }
    Mode::Stream => {
      // The matched text is escaped once and reused for the primary label.
      let mut text = std::mem::take(e.text);
      text.clear();
      write_json_str(&mut text, &e.src[start as usize..end as usize]);
      head.extend_from_slice(b"{\"text\":");
      head.extend_from_slice(&text);
      head.extend_from_slice(b",\"range\":");
      let range_at = head.len();
      write_range(&mut head, range);
      let range_end = head.len();
      head.extend_from_slice(b",\"file\":");
      head.extend_from_slice(e.path_json);
      write_lines(&mut head, e.src, e.lines, range, &text[1..text.len() - 1]);
      head.extend_from_slice(b",\"language\":");
      head.extend_from_slice(&e.unit.language_json);
      e.meta_variables(&mut head, f, true);
      for meta in metas() {
        e.out.extend_from_slice(&head);
        e.out.extend_from_slice(&meta.tail);
        e.out.extend_from_slice(b",\"labels\":[{\"text\":");
        e.out.extend_from_slice(&text);
        e.out.extend_from_slice(b",\"range\":");
        e.out.extend_from_slice(&head[range_at..range_end]);
        e.out.extend_from_slice(b",\"style\":\"primary\"}");
        for &(s, en) in &f.labels {
          e.out.extend_from_slice(b",{\"text\":");
          write_json_str(e.out, &e.src[s as usize..en as usize]);
          e.out.extend_from_slice(b",\"range\":");
          write_range(e.out, e.lines.range(e.src, s, en));
          e.out.extend_from_slice(b",\"style\":\"secondary\"}");
        }
        e.out.extend_from_slice(b"]}\n");
        count += 1;
        errors += meta.is_error as usize;
      }
      *e.text = text;
    }
  }
  *e.head = head;
  (count, errors)
}
