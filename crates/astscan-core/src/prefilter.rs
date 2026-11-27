//! Conservative source checks derived from the rules that can emit matches.

use crate::config::Group;
use crate::rule::{PNode, Rule, Strictness};
use memchr::memmem::Finder;
use regex_syntax::hir::{Hir, HirKind};
use std::collections::HashMap;

/// A successful match requires at least one of these byte strings in its source.
type Literals = Vec<Vec<u8>>;

const MIN_LITERAL_BYTES: usize = 3;
const MAX_ALTERNATIVES: usize = 16;

fn literal(bytes: &[u8]) -> Option<Literals> {
  (bytes.len() >= MIN_LITERAL_BYTES).then(|| vec![bytes.to_vec()])
}

fn alternatives(sets: impl IntoIterator<Item = Option<Literals>>) -> Option<Literals> {
  let mut literals = Vec::new();
  for set in sets {
    for bytes in set? {
      if !literals.contains(&bytes) {
        literals.push(bytes);
        if literals.len() > MAX_ALTERNATIVES {
          return None;
        }
      }
    }
  }
  (!literals.is_empty()).then_some(literals)
}

fn strongest(sets: impl IntoIterator<Item = Literals>) -> Option<Literals> {
  sets.into_iter().max_by_key(|set| (set.iter().map(Vec::len).min().unwrap_or(0), std::cmp::Reverse(set.len())))
}

fn pattern_literal(node: &PNode) -> Option<&[u8]> {
  match node {
    PNode::Terminal { text, named: true, .. } => Some(text.as_bytes()),
    PNode::Internal { children, .. } => children.iter().filter_map(pattern_literal).max_by_key(|text| text.len()),
    _ => None,
  }
}

fn regex_literals(hir: &Hir) -> Option<Literals> {
  match hir.kind() {
    HirKind::Literal(text) => literal(&text.0),
    HirKind::Capture(capture) => regex_literals(&capture.sub),
    HirKind::Repetition(repeat) if repeat.min > 0 => regex_literals(&repeat.sub),
    HirKind::Concat(parts) => strongest(parts.iter().filter_map(regex_literals)),
    HirKind::Alternation(branches) => alternatives(branches.iter().map(regex_literals)),
    _ => None,
  }
}

fn rule_literals(rule: &Rule, regexes: &[regex::Regex]) -> Option<Literals> {
  match rule {
    // In Smart mode a named terminal cannot be skipped, even after an ellipsis.
    Rule::Pattern(pattern) if pattern.strictness == Strictness::Smart => literal(pattern_literal(&pattern.node)?),
    Rule::Regex(index) => regex_literals(&regex_syntax::parse(regexes[*index].as_str()).ok()?),
    Rule::All(rules, _) => strongest(rules.iter().filter_map(|rule| rule_literals(rule, regexes))),
    Rule::Any(rules, _) => alternatives(rules.iter().map(|rule| rule_literals(rule, regexes))),
    Rule::Has(rel) | Rule::Inside(rel) | Rule::Precedes(rel) | Rule::Follows(rel) => rule_literals(&rel.rule, regexes),
    // Negative predicates, optional constraints and unresolved utils yield no required text.
    _ => None,
  }
}

pub(crate) struct Prefilter {
  needles: Vec<Finder<'static>>,
  /// Group order is unchanged. An empty list always passes.
  groups: Vec<Vec<usize>>,
  covers_all_groups: bool,
}

impl Prefilter {
  pub(crate) fn compile(groups: &[Group], regexes: &[regex::Regex]) -> Option<Self> {
    let mut needles = Vec::new();
    let mut ids = HashMap::new();
    let groups: Vec<Vec<usize>> = groups
      .iter()
      .map(|group| {
        rule_literals(&group.core.rule, regexes)
          .into_iter()
          .flatten()
          .map(|bytes| {
            *ids.entry(bytes).or_insert_with_key(|bytes| {
              let id = needles.len();
              needles.push(Finder::new(bytes).into_owned());
              id
            })
          })
          .collect()
      })
      .collect();
    let covers_all_groups = groups.iter().all(|group| !group.is_empty());
    (!needles.is_empty()).then_some(Self { needles, groups, covers_all_groups })
  }
  pub(crate) fn covers_all_groups(&self) -> bool {
    self.covers_all_groups
  }
}

#[derive(Default)]
pub(crate) struct PrefilterScratch {
  present: Vec<Option<bool>>,
  rejected: Vec<bool>,
}

impl PrefilterScratch {
  /// Populate the rejection mask or stop at the first group that needs parsing.
  pub(crate) fn check(&mut self, plan: &Prefilter, src: &[u8], files_only: bool) -> usize {
    self.present.clear();
    self.present.resize(plan.needles.len(), None);
    self.rejected.clear();
    let mut rejected = 0;
    for group in &plan.groups {
      let reject =
        !group.is_empty() && group.iter().all(|&id| !*self.present[id].get_or_insert_with(|| plan.needles[id].find(src).is_some()));
      if files_only && !reject {
        return 0;
      }
      self.rejected.push(reject);
      rejected += usize::from(reject);
    }
    rejected
  }

  pub(crate) fn rejected(&self) -> &[bool] {
    &self.rejected
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{Registry, RuleSet};

  #[test]
  fn rejects_only_groups_with_a_proven_missing_literal() {
    let rules = RuleSet::compile(
      r#"
id: either-call
language: JavaScript
rule:
  any:
    - pattern: logValue($X)
    - kind: call_expression
      regex: '^warnUser(?:Now)?\('
---
id: any-number
language: JavaScript
rule:
  any:
    - pattern: logValue($X)
    - kind: number
---
id: positive-relation
language: JavaScript
rule:
  kind: call_expression
  has:
    field: function
    regex: '^(écho|hello)$'
---
id: negative
language: JavaScript
rule:
  kind: identifier
  not:
    regex: 'hello'
"#,
      &mut Registry::default(),
    )
    .unwrap();
    let plan = rules.units[0].prefilter.as_ref().unwrap();
    assert!(!plan.covers_all_groups());
    let mut scratch = PrefilterScratch::default();
    assert_eq!(scratch.check(plan, b"1", false), 2);
    assert_eq!(scratch.rejected(), [true, false, true, false]);
    assert_eq!(scratch.check(plan, "warnUser(écho)".as_bytes(), false), 0);
    assert_eq!(scratch.rejected(), [false, false, false, false]);
    assert_eq!(scratch.check(plan, b"logValue(42)", false), 1);
    assert_eq!(scratch.rejected(), [false, false, true, false]);
  }
}
