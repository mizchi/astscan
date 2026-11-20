//! One file: parse, flatten, run every rule of its grammar in a single pass,
//! and serialise the matches.
//!
//! A `Worker` belongs to one thread and owns everything that is reused from
//! file to file: the parsers, the flat tree's arrays, the environment and the
//! output buffer. Scanning a file allocates only when a buffer has to grow.

use crate::cache::{Cache, Codec, file_key};
use crate::config::{Group, RuleSet, Unit};
use crate::eval::{Env, Matcher};
use crate::fastparse::Parser as FastParser;
use crate::lang::{HTML, HtmlInjections, INJECTABLE, Lang, Registry};
use crate::output::{LineIndex, Mode, write_json_str, write_lines, write_range};
use crate::record::{Found, Item, Reader, Span, write_found, write_unit};
use crate::tree::{FlatTree, NONE};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use tree_sitter::Parser;

const MAX_FILE_SIZE: usize = 3_000_000;
const MAX_LINE_COUNT: usize = 200_000;
const IGNORE_TEXT: &str = "ast-grep-ignore";
/// Output is handed to stdout once a worker holds this much.
const FLUSH_AT: usize = 1 << 18;
/// Cache records are handed to the cache file once a worker holds this much.
const STORE_AT: usize = 1 << 22;

pub struct Shared<'a> {
  /// The grammar of each unit; known before the rules are compiled.
  pub langs: &'a [Arc<Lang>],
  /// The compiled rules, set by whoever compiles them. A file answered
  /// from the cache never waits for this.
  pub rules: &'a OnceLock<Result<RuleSet, String>>,
  pub mode: Mode,
  /// Results of earlier runs, by file content.
  pub cache: Option<&'a Cache>,
  pub error_matches: AtomicUsize,
  pub match_count: AtomicUsize,
  pub file_count: AtomicUsize,
  pub byte_count: AtomicUsize,
  /// Parse with `fastparse` and fall back to tree-sitter where it gives up.
  pub fast_parser: bool,
  /// Files `fastparse` handed back, and files it parsed.
  pub fast_parsed: [AtomicUsize; 2],
  /// Compare the direct flattening with the cursor one on every file.
  pub check_flatten: bool,
  /// Nanoseconds spent parsing, flattening, and matching plus writing.
  pub phase_nanos: [AtomicUsize; 3],
  /// Slowest files as `(micros, path)`, kept only under `--stats`.
  pub slow: Option<std::sync::Mutex<Vec<(u128, String)>>>,
}

impl<'a> Shared<'a> {
  pub fn unit_for(&self, lang_name: &str) -> Option<usize> {
    self.langs.iter().position(|l| l.name == lang_name)
  }

  /// The compiled rules, waiting for them if need be. `None` when they did not compile.
  pub fn compiled(&self) -> Option<&'a RuleSet> {
    self.rules.wait().as_ref().ok()
  }
}

/// `// ast-grep-ignore` comments of one file. `None` as a rule set means every rule.
#[derive(Default)]
struct Suppressions {
  file: Option<Option<HashSet<String>>>,
  lines: HashMap<usize, Option<HashSet<String>>>,
}

impl Suppressions {
  fn covers(set: &Option<HashSet<String>>, id: &str) -> bool {
    set.as_ref().is_none_or(|s| s.contains(id))
  }
  fn suppressed(&self, line: usize, id: &str) -> bool {
    self.file.as_ref().is_some_and(|s| Self::covers(s, id)) || self.lines.get(&line).is_some_and(|s| Self::covers(s, id))
  }
}

fn parse_suppression_set(text: &str) -> Option<HashSet<String>> {
  let (_, after) = text.trim().split_once(IGNORE_TEXT)?;
  let after = after.trim();
  if after.is_empty() {
    return None;
  }
  let (_, rules) = after.split_once(':')?;
  Some(rules.split(',').filter_map(|r| r.trim_start().split(' ').next()).map(str::to_string).collect())
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

/// What a listed file is scanned as.
#[derive(Clone, Copy)]
pub enum Target {
  /// A file of the grammar of `RuleSet::units[i]`.
  Unit(usize),
  /// An HTML document: scanned as HTML when HTML rules exist, and once per
  /// embedded script or style for the rules of that language.
  Html,
}

pub struct Worker<'a> {
  shared: &'a Shared<'a>,
  html: Option<HtmlInjections>,
  parsers: Vec<Option<Parser>>,
  /// Outer `None`: not tried yet. Inner `None`: a grammar the driver cannot read.
  fast_parsers: Vec<Option<Option<FastParser>>>,
  tree: FlatTree,
  env: Env,
  lines: LineIndex,
  out: Vec<u8>,
  head: Vec<u8>,
  text: Vec<u8>,
  src: Vec<u8>,
  /// Matches and error-severity matches of the file being scanned.
  matched: usize,
  errors: usize,
  /// The match being written, as byte ranges.
  found: Found,
  /// Whether the file being scanned goes into the cache, and its matches so far, encoded.
  recording: bool,
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
      html: None,
      parsers: shared.langs.iter().map(|_| None).collect(),
      fast_parsers: shared.langs.iter().map(|_| None).collect(),
      tree: FlatTree::default(),
      env: Env::default(),
      lines: LineIndex::default(),
      out: Vec::with_capacity(FLUSH_AT * 2),
      head: Vec::new(),
      text: Vec::new(),
      src: Vec::new(),
      matched: 0,
      errors: 0,
      found: Found::default(),
      recording: false,
      entry: Vec::new(),
      payload: Vec::new(),
      records: Vec::new(),
      codec: Codec::default(),
    }
  }

  pub fn flush(&mut self) {
    flush(&mut self.out);
  }

  pub fn scan_file(&mut self, path: &std::path::Path, target: Target) {
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
  fn scan_bytes(&mut self, src: &[u8], path: &std::path::Path, target: Target) {
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
    if shared.compiled().is_none() {
      return;
    }
    if let Some((cache, _)) = key {
      cache.misses.fetch_add(1, Ordering::Relaxed);
      self.recording = true;
      self.entry.clear();
    }
    self.scan_text(src, display, target);
    if self.matched > 0 {
      shared.match_count.fetch_add(self.matched, Ordering::Relaxed);
    }
    if self.errors > 0 {
      shared.error_matches.fetch_add(self.errors, Ordering::Relaxed);
    }
    if let Some((cache, key)) = key {
      self.recording = false;
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
    let Some(rules) = shared.compiled() else { return false };
    if payload.is_empty() {
      return true;
    }
    // Once over everything before a byte is written: output cannot be taken back.
    let mut reader = Reader::new(payload);
    let mut unit: Option<&Unit> = None;
    loop {
      match reader.next(&mut self.found) {
        Err(_) => return false,
        Ok(None) => break,
        Ok(Some(Item::Unit(u))) => unit = rules.units.get(u as usize),
        Ok(Some(Item::Found)) => {
          let f = &self.found;
          let Some(group) = unit.and_then(|u| u.groups.get(f.group as usize)) else { return false };
          let vars = f.single.iter().map(|(v, _)| v).chain(f.multi.iter().map(|(v, _, _)| v));
          if f.extent().is_none_or(|end| end as usize > src.len())
            || f.dead.iter().any(|&i| i as usize >= group.metas.len())
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
    let mut reader = Reader::new(payload);
    let mut unit = &rules.units[0];
    while let Ok(Some(item)) = reader.next(&mut self.found) {
      match item {
        Item::Unit(u) => unit = &rules.units[u as usize],
        Item::Found => {
          let mut e = Emit {
            out: &mut self.out,
            head: &mut self.head,
            text: &mut self.text,
            lines: &self.lines,
            src,
            path,
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

  fn scan_text(&mut self, src: &[u8], display: &str, target: Target) {
    // A file that is not UTF-8 is skipped, as is one both huge and long.
    let Ok(text) = simdutf8::basic::from_utf8(src) else { return };
    if text.len() > MAX_FILE_SIZE && text.lines().count() > MAX_LINE_COUNT {
      return;
    }
    let started = self.shared.slow.as_ref().map(|_| std::time::Instant::now());
    match target {
      Target::Unit(unit_idx) => self.scan_source(text, display, unit_idx, None),
      Target::Html => {
        if let Some(unit_idx) = self.shared.unit_for(HTML) {
          self.scan_source(text, display, unit_idx, None);
        }
        let injections = self.html.get_or_insert_with(HtmlInjections::default).extract(text);
        for (lang, range) in injections {
          let unit = INJECTABLE
            .contains(&lang.as_str())
            .then(|| Registry::builtin_name(&lang))
            .flatten()
            .and_then(|name| self.shared.unit_for(name));
          if let Some(unit_idx) = unit {
            self.scan_source(text, display, unit_idx, Some(range));
          }
        }
      }
    }
    if let (Some(slow), Some(started)) = (&self.shared.slow, started) {
      let mut slow = slow.lock().expect("no panic while holding the lock");
      slow.push((started.elapsed().as_micros(), display.to_string()));
      if slow.len() > 64 {
        slow.sort_unstable_by_key(|e| std::cmp::Reverse(e.0));
        slow.truncate(8);
      }
    }
  }

  /// `within` restricts parsing to one range of `src`: a script inside HTML.
  pub fn scan_source(&mut self, src: &str, path: &str, unit_idx: usize, within: Option<tree_sitter::Range>) {
    let shared = self.shared;
    let Some(rules) = shared.compiled() else { return };
    let unit: &Unit = &rules.units[unit_idx];
    let timed = shared.slow.is_some();
    let clock = |on: bool| on.then(std::time::Instant::now);
    let lap = |phase: usize, since: Option<std::time::Instant>| {
      if let Some(t) = since {
        shared.phase_nanos[phase].fetch_add(t.elapsed().as_nanos() as usize, Ordering::Relaxed);
      }
    };
    let t0 = clock(timed);
    // The driver of our own first; whatever it hands back goes to tree-sitter.
    let mut parsed_fast = false;
    if shared.fast_parser && within.is_none() {
      let fast = self.fast_parsers[unit_idx].get_or_insert_with(|| FastParser::new(&unit.lang.ts).ok());
      if let Some(fast) = fast {
        parsed_fast = fast.parse(src, &mut self.tree).is_ok();
      }
      shared.fast_parsed[parsed_fast as usize].fetch_add(1, Ordering::Relaxed);
    }
    if parsed_fast && !shared.check_flatten {
      lap(0, t0);
    }
    let t1 = clock(timed);
    if !parsed_fast || shared.check_flatten {
      let parser = self.parsers[unit_idx].get_or_insert_with(|| {
        let mut p = Parser::new();
        p.set_language(&unit.lang.ts).expect("the grammar loaded, so its version is compatible");
        p
      });
      let parsed = match within {
        None => parser.parse(src.as_bytes(), None),
        Some(range) => {
          if parser.set_included_ranges(&[range]).is_err() {
            return;
          }
          let tree = parser.parse(src.as_bytes(), None);
          parser.set_included_ranges(&[]).expect("an empty range list is always valid");
          tree
        }
      };
      let Some(ts_tree) = parsed else { return };
      if shared.check_flatten {
        // Three routes to one tree: the cursor is the reference for both others.
        let mut reference = FlatTree::default();
        reference.build_with_cursor(&ts_tree);
        if parsed_fast && let Some(diff) = crate::tree::first_difference(&self.tree, &reference) {
          eprintln!("astscan: fast parser mismatch in {path}: {diff}");
          std::process::exit(3);
        }
        self.tree.build(ts_tree);
        if let Some(diff) = crate::tree::first_difference(&self.tree, &reference) {
          eprintln!("astscan: flatten mismatch in {path}: {diff}");
          std::process::exit(3);
        }
      } else {
        lap(0, t0);
        self.tree.build(ts_tree);
      }
    }
    lap(1, t1);
    let t2 = clock(timed);
    shared.file_count.fetch_add(1, Ordering::Relaxed);
    shared.byte_count.fetch_add(src.len(), Ordering::Relaxed);

    let bytes = src.as_bytes();
    let mut lines_built = false;
    let mut unit_written = false;
    let suppressions = if memchr::memmem::find(bytes, IGNORE_TEXT.as_bytes()).is_some() {
      self.lines.build(bytes);
      lines_built = true;
      match self.collect_suppressions(src, unit) {
        Some(s) => Some(s),
        // A bare file-level `ast-grep-ignore` silences the whole file.
        None => return,
      }
    } else {
      None
    };

    let t = &self.tree;
    let mut matched = 0usize;
    let mut errors = 0usize;
    let span = |n: u32| -> Span { (t.start[n as usize], t.end[n as usize]) };
    for n in 0..t.len() as u32 {
      let Some(groups) = unit.kind_map.get(t.kind[n as usize] as usize) else { continue };
      for &g in groups {
        let group: &Group = &unit.groups[g as usize];
        self.env.clear();
        let m = Matcher { t, src, utils: &group.core.utils };
        let Some(node) = m.match_core(&group.core, n, &mut self.env) else { continue };
        if !lines_built {
          self.lines.build(bytes);
          lines_built = true;
        }
        // The match as byte ranges: all that writing it, now or from the cache, needs.
        let (env, f) = (&self.env, &mut self.found);
        f.clear();
        f.group = g;
        (f.start, f.end) = span(node);
        if let Some(suppressions) = &suppressions {
          let line = self.lines.line(f.start);
          f.dead.extend((0..group.metas.len() as u32).filter(|&i| suppressions.suppressed(line, &group.metas[i as usize].id)));
          if f.dead.len() == group.metas.len() {
            continue;
          }
        }
        f.single.extend(env.single.iter().map(|&(var, n)| (var as u32, span(n))));
        for &(var, first, count) in &env.multi {
          f.multi.push((var as u32, f.pool.len() as u32, count));
          f.pool.extend(env.pool[first as usize..(first + count) as usize].iter().map(|&n| span(n)));
        }
        f.labels.extend(env.labels.iter().map(|&n| span(n)));
        if self.recording {
          if !std::mem::replace(&mut unit_written, true) {
            write_unit(&mut self.entry, unit_idx as u32);
          }
          write_found(&mut self.entry, f);
        }
        let mut e = Emit {
          out: &mut self.out,
          head: &mut self.head,
          text: &mut self.text,
          lines: &self.lines,
          src: bytes,
          path,
          mode: shared.mode,
          unit,
          keys: &unit.var_keys,
        };
        let (count, errs) = emit(&mut e, group, f);
        matched += count;
        errors += errs;
      }
      // Every line in the buffer is complete, so it can leave at any node.
      if self.out.len() >= FLUSH_AT {
        flush(&mut self.out);
      }
    }
    lap(2, t2);
    self.matched += matched;
    self.errors += errors;
  }

  /// `None` when the whole file is suppressed.
  fn collect_suppressions(&self, src: &str, unit: &Unit) -> Option<Suppressions> {
    let t = &self.tree;
    let lines = &self.lines;
    let mut sup = Suppressions::default();
    for n in 0..t.len() as u32 {
      if !unit.lang.comment_kind.get(t.kind[n as usize] as usize).copied().unwrap_or(false) {
        continue;
      }
      let text = &src[t.start[n as usize] as usize..t.end[n as usize] as usize];
      let Some(start) = text.find(|c: char| c.is_alphabetic()) else { continue };
      if !text[start..].starts_with(IGNORE_TEXT) {
        continue;
      }
      let line = lines.line(t.start[n as usize]);
      let prev = t.prev[n as usize];
      let prev_same_line = (prev != NONE && lines.line(t.end[prev as usize]) == line).then_some(prev);
      let next = t.ts_next_sibling(n);
      let set = parse_suppression_set(text);
      if line == 0 && prev_same_line.is_none() && (next == NONE || lines.line(t.start[next as usize]) >= 2) {
        set.as_ref()?;
        sup.file = Some(set);
        continue;
      }
      let key = prev_same_line.map_or(line + 1, |p| lines.line(t.start[p as usize]));
      sup.lines.insert(key, set);
    }
    Some(sup)
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
  path: &'b str,
  mode: Mode,
  unit: &'a Unit,
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
    write_range(out, self.src, self.lines, s, e);
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
fn emit(e: &mut Emit, group: &Group, f: &Found) -> (usize, usize) {
  let (start, end) = (f.start, f.end);
  let live = |i: usize| !f.dead.contains(&(i as u32));
  let metas = || group.metas.iter().enumerate().filter(|(i, _)| live(*i)).map(|(_, m)| m);
  let mut count = 0;
  let mut errors = 0;

  let mut head = std::mem::take(e.head);
  head.clear();
  match e.mode {
    Mode::Slim => {
      head.extend_from_slice(b"{\"file\":");
      write_json_str(&mut head, e.path.as_bytes());
      head.extend_from_slice(b",\"language\":");
      write_json_str(&mut head, e.unit.lang.name.as_bytes());
      head.extend_from_slice(b",\"range\":");
      write_range(&mut head, e.src, e.lines, start, end);
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
      write_range(&mut head, e.src, e.lines, start, end);
      let range_end = head.len();
      head.extend_from_slice(b",\"file\":");
      write_json_str(&mut head, e.path.as_bytes());
      write_lines(&mut head, e.src, start, end);
      head.extend_from_slice(b",\"language\":");
      write_json_str(&mut head, e.unit.lang.name.as_bytes());
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
          write_range(e.out, e.src, e.lines, s, en);
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
