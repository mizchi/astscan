//! Parse, flatten, and run all applicable rule groups over one source.

use crate::config::{RuleInfo, RuleSet, Unit};
use crate::eval::{Env, Matcher};
use crate::fastparse::Parser as FastParser;
use crate::found::{Found, Span};
use crate::lang::{HTML, HtmlInjections, INJECTABLE, Registry};
use crate::prefilter::PrefilterScratch;
use crate::tree::{FlatTree, NONE, first_difference};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::Instant;
use tree_sitter::Parser;

const IGNORE_TEXT: &str = "ast-grep-ignore";

/// A match shared by all equivalent rules that are active at this location.
/// All offsets are UTF-8 byte offsets in the original source.
pub struct Match<'a> {
  unit: &'a Unit,
  found: &'a Found,
}

impl<'a> Match<'a> {
  /// The name of the grammar that found the match, including inside HTML.
  pub fn language(&self) -> &'a str {
    &self.unit.lang.name
  }

  pub fn range(&self) -> Range<u32> {
    self.found.start..self.found.end
  }

  pub fn rules(&self) -> impl Iterator<Item = &'a RuleInfo> {
    let dead = &self.found.dead;
    self.unit.groups[self.found.group as usize]
      .metas
      .iter()
      .enumerate()
      .filter_map(move |(i, rule)| (!dead.contains(&(i as u32))).then_some(rule))
  }

  pub fn single_captures(&self) -> impl Iterator<Item = (&'a str, Range<u32>)> {
    let captures = &self.found.single;
    captures.iter().enumerate().filter_map(move |(i, &(var, (start, end)))| {
      if captures[i + 1..].iter().any(|&(v, _)| v == var) {
        return None;
      }
      Some((self.unit.var_names[var as usize].as_str(), start..end))
    })
  }

  pub fn multi_captures(&self) -> impl Iterator<Item = (&'a str, impl Iterator<Item = Range<u32>> + 'a)> {
    let captures = &self.found.multi;
    captures.iter().enumerate().filter_map(move |(i, &(var, start, len))| {
      if captures[i + 1..].iter().any(|&(v, _, _)| v == var) {
        return None;
      }
      let items = self.found.pool[start as usize..(start + len) as usize].iter().map(|&(s, e)| s..e);
      Some((self.unit.var_names[var as usize].as_str(), items))
    })
  }

  /// Secondary matches from relational rules.
  pub fn labels(&self) -> impl Iterator<Item = Range<u32>> {
    self.found.labels.iter().map(|&(s, e)| s..e)
  }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ParserKind {
  #[default]
  TreeSitter,
  /// Experimental parser with tree-sitter fallback.
  Fast,
}

#[derive(Debug)]
pub enum ScanError {
  SourceTooLarge,
  ParseFailed,
  FastParserMismatch(String),
  FlattenMismatch(String),
}

impl std::fmt::Display for ScanError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::SourceTooLarge => f.write_str("source exceeds the 32-bit tree-sitter offset limit"),
      Self::ParseFailed => f.write_str("tree-sitter could not parse the source"),
      Self::FastParserMismatch(diff) => write!(f, "fast parser mismatch: {diff}"),
      Self::FlattenMismatch(diff) => write!(f, "flatten mismatch: {diff}"),
    }
  }
}

impl std::error::Error for ScanError {}

#[derive(Clone, Copy)]
pub enum Target {
  Unit(usize),
  Html,
}

/// How to use necessary literals before structural matching.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LiteralPrefilter {
  Off,
  /// Check only when every group can be rejected, so parsing may be skipped.
  #[default]
  Files,
  /// Also check mixed rule sets and bypass individual rejected groups.
  FilesAndGroups,
}

/// Independent optimizations for in-memory scans.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScanOptimizations {
  /// Reject rule groups whose required bytes are absent from the source.
  pub literal_prefilter: LiteralPrefilter,
  /// Give each scanner separate regex scratch space, sharing compiled regexes.
  pub worker_local_regex: bool,
}

#[derive(Clone, Copy, Default)]
pub struct ScanOptions {
  pub parser: ParserKind,
  pub check_flatten: bool,
  pub timed: bool,
  pub optimizations: ScanOptimizations,
}

#[derive(Default)]
pub struct ScanStats {
  pub files: usize,
  pub bytes: usize,
  pub fast_parsed: [usize; 2],
  pub phase_nanos: [usize; 3],
  pub prefilter: PrefilterStats,
}

#[derive(Default)]
pub struct PrefilterStats {
  pub skipped_parses: usize,
  pub skipped_bytes: usize,
  pub rejected_groups: usize,
  pub nanos: usize,
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

/// Parser and matching buffers reused across sources. Use one scanner per
/// worker and share the compiled rules between workers.
pub struct Scanner<'a> {
  rules: &'a RuleSet,
  options: ScanOptions,
  html: Option<HtmlInjections>,
  parsers: Vec<Option<Parser>>,
  /// Outer None means uninitialised. Inner None means an unsupported grammar.
  fast_parsers: Vec<Option<Option<FastParser>>>,
  tree: FlatTree,
  env: Env,
  local_regexes: Vec<Option<Vec<regex::Regex>>>,
  prefilter: PrefilterScratch,
  newlines: Vec<u32>,
  found: Found,
}

impl<'a> Scanner<'a> {
  pub fn new(rules: &'a RuleSet) -> Self {
    Self::with_parser(rules, ParserKind::default())
  }

  pub fn with_parser(rules: &'a RuleSet, parser: ParserKind) -> Self {
    Self::configured(rules, ScanOptions { parser, ..ScanOptions::default() })
  }

  /// Choose which optimizations to use with this scanner and parser.
  pub fn with_optimizations(mut self, optimizations: ScanOptimizations) -> Self {
    self.options.optimizations = optimizations;
    self
  }

  #[cfg(feature = "internals")]
  #[doc(hidden)]
  pub fn with_options(rules: &'a RuleSet, options: ScanOptions) -> Self {
    Self::configured(rules, options)
  }

  fn configured(rules: &'a RuleSet, options: ScanOptions) -> Self {
    Self {
      rules,
      options,
      html: None,
      parsers: rules.units.iter().map(|_| None).collect(),
      fast_parsers: rules.units.iter().map(|_| None).collect(),
      tree: FlatTree::default(),
      env: Env::default(),
      local_regexes: rules.units.iter().map(|_| None).collect(),
      prefilter: PrefilterScratch::default(),
      newlines: Vec::new(),
      found: Found::default(),
    }
  }

  /// Scan a UTF-8 source using a built-in name/alias or a custom grammar name.
  /// A language without rules yields no matches. HTML also scans scripts and
  /// styles with rules for their declared language.
  pub fn scan(&mut self, language: &str, src: &str, mut visit: impl FnMut(Match<'_>)) -> Result<(), ScanError> {
    let unit = self.rules.unit_for(language);
    let builtin = Registry::builtin_name(language);
    let target = if builtin == Some(HTML) {
      Target::Html
    } else if let Some(index) = unit.or_else(|| builtin.and_then(|name| self.rules.unit_for(name))) {
      Target::Unit(index)
    } else {
      return Ok(());
    };
    let rules = self.rules;
    self.scan_target_impl(src, target, |idx, found| visit(Match { unit: &rules.units[idx], found }))?;
    Ok(())
  }

  #[cfg(feature = "internals")]
  #[doc(hidden)]
  pub fn scan_target(&mut self, src: &str, target: Target, visit: impl FnMut(usize, &Found)) -> Result<ScanStats, ScanError> {
    self.scan_target_impl(src, target, visit)
  }

  fn scan_target_impl(&mut self, src: &str, target: Target, mut visit: impl FnMut(usize, &Found)) -> Result<ScanStats, ScanError> {
    if src.len() > u32::MAX as usize {
      return Err(ScanError::SourceTooLarge);
    }
    let mut stats = ScanStats::default();
    match target {
      Target::Unit(unit_idx) => self.scan_source(src, unit_idx, None, &mut stats, &mut visit)?,
      Target::Html => {
        if let Some(unit_idx) = self.rules.unit_for(HTML) {
          self.scan_source(src, unit_idx, None, &mut stats, &mut visit)?;
        }
        let injections = self.html.get_or_insert_with(HtmlInjections::default).extract(src);
        for (lang, range) in injections {
          let unit =
            INJECTABLE.contains(&lang.as_str()).then(|| Registry::builtin_name(&lang)).flatten().and_then(|name| self.rules.unit_for(name));
          if let Some(unit_idx) = unit {
            self.scan_source(src, unit_idx, Some(range), &mut stats, &mut visit)?;
          }
        }
      }
    }
    Ok(stats)
  }

  fn scan_source(
    &mut self,
    src: &str,
    unit_idx: usize,
    within: Option<tree_sitter::Range>,
    stats: &mut ScanStats,
    visit: &mut impl FnMut(usize, &Found),
  ) -> Result<(), ScanError> {
    let Some(unit) = self.rules.units.get(unit_idx) else { return Ok(()) };
    let clock = || self.options.timed.then(Instant::now);
    let lap = |phase: &mut usize, since: Option<Instant>| {
      if let Some(t) = since {
        *phase += t.elapsed().as_nanos() as usize;
      }
    };
    stats.files += 1;
    stats.bytes += src.len();
    let mode = self.options.optimizations.literal_prefilter;
    let rejected_groups = if !self.options.check_flatten
      && mode != LiteralPrefilter::Off
      && let Some(plan) = &unit.prefilter
      && (mode == LiteralPrefilter::FilesAndGroups || plan.covers_all_groups())
    {
      let started = clock();
      // Checking the whole source also covers any included range in an HTML injection.
      let rejected = self.prefilter.check(plan, src.as_bytes(), mode == LiteralPrefilter::Files);
      lap(&mut stats.prefilter.nanos, started);
      if rejected == unit.groups.len() {
        stats.prefilter.skipped_parses += 1;
        stats.prefilter.skipped_bytes += within.map_or(src.len(), |range| range.end_byte - range.start_byte);
        stats.prefilter.rejected_groups += rejected;
        return Ok(());
      }
      if mode == LiteralPrefilter::FilesAndGroups {
        stats.prefilter.rejected_groups += rejected;
        rejected
      } else {
        0
      }
    } else {
      0
    };
    let t0 = clock();
    let mut parsed_fast = false;
    if self.options.parser == ParserKind::Fast && within.is_none() {
      let fast = self.fast_parsers[unit_idx].get_or_insert_with(|| FastParser::new(unit.lang.clone()).ok());
      if let Some(fast) = fast {
        parsed_fast = fast.parse(src, &mut self.tree).is_ok();
      }
      stats.fast_parsed[parsed_fast as usize] += 1;
    }
    let t1;
    if parsed_fast && !self.options.check_flatten {
      lap(&mut stats.phase_nanos[0], t0);
      t1 = clock();
    } else {
      let parser = self.parsers[unit_idx].get_or_insert_with(|| {
        let mut p = Parser::new();
        p.set_language(&unit.lang.ts).expect("the grammar loaded, so its version is compatible");
        p
      });
      let parsed = match within {
        None => parser.parse(src.as_bytes(), None),
        Some(range) => {
          parser.set_included_ranges(&[range]).map_err(|_| ScanError::ParseFailed)?;
          let tree = parser.parse(src.as_bytes(), None);
          parser.set_included_ranges(&[]).expect("an empty range list is always valid");
          tree
        }
      };
      let ts_tree = parsed.ok_or(ScanError::ParseFailed)?;
      lap(&mut stats.phase_nanos[0], t0);
      t1 = clock();
      if self.options.check_flatten {
        let mut reference = FlatTree::default();
        reference.build_with_cursor(&ts_tree);
        if parsed_fast && let Some(diff) = first_difference(&self.tree, &reference) {
          return Err(ScanError::FastParserMismatch(diff));
        }
        self.tree.build(ts_tree);
        if let Some(diff) = first_difference(&self.tree, &reference) {
          return Err(ScanError::FlattenMismatch(diff));
        }
      } else {
        self.tree.build(ts_tree);
      }
    }
    lap(&mut stats.phase_nanos[1], t1);
    let t2 = clock();
    let suppressions = if memchr::memmem::find(src.as_bytes(), IGNORE_TEXT.as_bytes()).is_some() {
      self.newlines.clear();
      self.newlines.extend(memchr::memchr_iter(b'\n', src.as_bytes()).map(|i| i as u32));
      match self.collect_suppressions(src, unit) {
        Some(s) => Some(s),
        // A bare file-level ast-grep-ignore silences the whole file.
        None => return Ok(()),
      }
    } else {
      None
    };

    let t = &self.tree;
    let regexes = if self.options.optimizations.worker_local_regex && !unit.regexes.is_empty() {
      self.local_regexes[unit_idx].get_or_insert_with(|| unit.regexes.clone()).as_slice()
    } else {
      &unit.regexes
    };
    let rejected = if rejected_groups == 0 { None } else { Some(self.prefilter.rejected()) };
    let span = |n: u32| -> Span { (t.start[n as usize], t.end[n as usize]) };
    for n in 0..t.len() as u32 {
      let Some(groups) = unit.kind_map.get(t.kind[n as usize] as usize) else { continue };
      for &g in groups {
        if rejected.is_some_and(|mask| mask[g as usize]) {
          continue;
        }
        let group = &unit.groups[g as usize];
        self.env.clear();
        let m = Matcher { t, src, utils: &group.core.utils, regexes };
        let Some(node) = m.match_core(&group.core, n, &mut self.env) else { continue };
        let (env, f) = (&self.env, &mut self.found);
        f.clear();
        f.group = g;
        (f.start, f.end) = span(node);
        if let Some(suppressions) = &suppressions {
          let line = self.newlines.partition_point(|&offset| offset < f.start);
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
        visit(unit_idx, f);
      }
    }
    lap(&mut stats.phase_nanos[2], t2);
    Ok(())
  }

  /// `None` when the whole file is suppressed.
  fn collect_suppressions(&self, src: &str, unit: &Unit) -> Option<Suppressions> {
    let t = &self.tree;
    let line_for = |at| self.newlines.partition_point(|&offset| offset < at);
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
      let line = line_for(t.start[n as usize]);
      let prev = t.prev[n as usize];
      let prev_same_line = (prev != NONE && line_for(t.end[prev as usize]) == line).then_some(prev);
      let next = t.ts_next_sibling(n);
      let set = parse_suppression_set(text);
      if line == 0 && prev_same_line.is_none() && (next == NONE || line_for(t.start[next as usize]) >= 2) {
        set.as_ref()?;
        sup.file = Some(set);
        continue;
      }
      let key = prev_same_line.map_or(line + 1, |p| line_for(t.start[p as usize]));
      sup.lines.insert(key, set);
    }
    Some(sup)
  }
}
