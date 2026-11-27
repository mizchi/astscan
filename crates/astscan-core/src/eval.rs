//! Rule evaluation over a `FlatTree`.
//!
//! Semantics follow ast-grep's matcher: the same pattern algorithm, the same
//! strictness table, the same order of sub-rule evaluation. What differs is
//! the mechanics. The environment is three append-only stacks, so "try this
//! and forget it if it fails" is a mark and a truncate rather than a clone of
//! a hash map, and every evaluation leaves the environment untouched when it
//! fails.

use crate::rule::{ERROR_KIND, MetaVar, NthChild, PNode, Pattern, Relation, Rule, RuleCore, StopBy, Strictness};
use crate::simd::find_u16;
use crate::tree::{FlatTree, Kids, NONE};

#[derive(Default)]
pub struct Env {
  /// `(var, node)`; a later entry for a var shadows an earlier one.
  pub single: Vec<(u16, u32)>,
  /// `(var, start, len)` into `pool`.
  pub multi: Vec<(u16, u32, u32)>,
  pub pool: Vec<u32>,
  /// Nodes matched by relational sub-rules: ast-grep's "secondary" labels.
  pub labels: Vec<u32>,
}

#[derive(Clone, Copy)]
pub struct Mark(usize, usize, usize, usize);

impl Env {
  #[inline]
  pub fn mark(&self) -> Mark {
    Mark(self.single.len(), self.multi.len(), self.pool.len(), self.labels.len())
  }
  #[inline]
  pub fn truncate(&mut self, m: Mark) {
    self.single.truncate(m.0);
    self.multi.truncate(m.1);
    self.pool.truncate(m.2);
    self.labels.truncate(m.3);
  }
  pub fn clear(&mut self) {
    self.single.clear();
    self.multi.clear();
    self.pool.clear();
    self.labels.clear();
  }
  #[inline]
  fn get_single(&self, var: u16) -> Option<u32> {
    self.single.iter().rev().find(|(v, _)| *v == var).map(|(_, n)| *n)
  }
  fn get_multi(&self, var: u16) -> Option<(u32, u32)> {
    self.multi.iter().rev().find(|(v, _, _)| *v == var).map(|(_, s, l)| (*s, *l))
  }
}

enum One {
  MatchedBoth,
  SkipBoth,
  SkipGoal,
  SkipCandidate,
  NoMatch,
}

enum Flow {
  Continue,
  Fallthrough,
  Return,
}

pub struct Matcher<'a> {
  pub t: &'a FlatTree,
  pub src: &'a str,
  pub utils: &'a [Rule],
}

impl<'a> Matcher<'a> {
  #[inline]
  fn text(&self, n: u32) -> &'a str {
    &self.src[self.t.start[n as usize] as usize..self.t.end[n as usize] as usize]
  }
  #[inline]
  fn kind(&self, n: u32) -> u16 {
    self.t.kind[n as usize]
  }

  /// A whole rule document against one node: the rule, then its constraints.
  pub fn match_core(&self, core: &RuleCore, n: u32, env: &mut Env) -> Option<u32> {
    let mark = env.mark();
    let ret = self.eval(&core.rule, n, env)?;
    if !core.constraints.is_empty() {
      // Constraints see the captures the rule made, not ones other constraints add.
      let bound = env.single.len();
      for (var, rule) in &core.constraints {
        let Some(cand) = env.single[..bound].iter().rev().find(|(v, _)| v == var).map(|(_, n)| *n) else { continue };
        if self.eval(rule, cand, env).is_none() {
          env.truncate(mark);
          return None;
        }
      }
    }
    Some(ret)
  }

  /// Does the rule match, in an environment of its own?
  fn matches_fresh(&self, rule: &Rule, n: u32) -> bool {
    let mut env = Env::default();
    self.eval(rule, n, &mut env).is_some()
  }

  pub fn eval(&self, rule: &Rule, n: u32, env: &mut Env) -> Option<u32> {
    match rule {
      Rule::Kind(k) => (self.kind(n) == *k).then_some(n),
      Rule::Pattern(p) => self.match_pattern(p, n, env).then_some(n),
      Rule::Regex(re) => re.is_match(self.text(n)).then_some(n),
      Rule::NthChild(nc) => self.nth_child(nc, n, env),
      Rule::All(rules, kinds) => {
        if let Some(k) = kinds
          && !k.contains(self.kind(n))
        {
          return None;
        }
        let mark = env.mark();
        for r in rules {
          if self.eval(r, n, env).is_none() {
            env.truncate(mark);
            return None;
          }
        }
        Some(n)
      }
      Rule::Any(rules, kinds) => {
        if let Some(k) = kinds
          && !k.contains(self.kind(n))
        {
          return None;
        }
        rules.iter().any(|r| self.eval(r, n, env).is_some()).then_some(n)
      }
      Rule::Not(inner) => {
        let mark = env.mark();
        let matched = self.eval(inner, n, env).is_some();
        env.truncate(mark);
        (!matched).then_some(n)
      }
      Rule::Matches(i) => self.eval(&self.utils[*i as usize], n, env),
      Rule::Inside(rel) => self.labelled(self.inside(rel, n, env), env),
      Rule::Has(rel) => self.labelled(self.has(rel, n, env), env),
      Rule::Precedes(rel) => self.labelled(self.sibling(rel, n, env, true), env),
      Rule::Follows(rel) => self.labelled(self.sibling(rel, n, env, false), env),
    }
  }

  #[inline]
  fn labelled(&self, matched: Option<u32>, env: &mut Env) -> Option<u32> {
    let m = matched?;
    env.labels.push(m);
    Some(m)
  }

  fn inside(&self, rel: &Relation, n: u32, env: &mut Env) -> Option<u32> {
    let t = self.t;
    let mut last = n;
    let mut cur = t.parent[n as usize];
    let mut stopped = false;
    while cur != NONE {
      if let StopBy::Rule(stop) = &rel.stop {
        if stopped {
          break;
        }
        stopped = self.matches_fresh(stop, cur);
      }
      let expect = last;
      last = cur;
      let on_field = match rel.field {
        Some(f) => t.child_by_field(cur, f) == Some(expect),
        None => true,
      };
      if on_field && let Some(m) = self.eval(&rel.rule, cur, env) {
        return Some(m);
      }
      if matches!(rel.stop, StopBy::Neighbor) {
        return None;
      }
      cur = t.parent[cur as usize];
    }
    None
  }

  /// First node in pre-order range `[from, to)` the relation's rule matches.
  fn find_in_range(&self, rel: &Relation, from: u32, to: u32, env: &mut Env) -> Option<u32> {
    let (from, to) = (from as usize, to as usize);
    if let Some(k) = rel.one_kind {
      // One candidate kind: jump between its occurrences in the kind array.
      let kinds = &self.t.kind[..to];
      let mut i = from;
      while let Some(off) = find_u16(&kinds[i..], k) {
        let d = (i + off) as u32;
        if let Some(m) = self.eval(&rel.rule, d, env) {
          return Some(m);
        }
        i += off + 1;
      }
      return None;
    }
    if let Some(set) = &rel.kinds {
      for d in from..to {
        if set.contains(self.t.kind[d])
          && let Some(m) = self.eval(&rel.rule, d as u32, env)
        {
          return Some(m);
        }
      }
      return None;
    }
    (from..to).find_map(|d| self.eval(&rel.rule, d as u32, env))
  }

  fn find_child(&self, rule: &Rule, n: u32, env: &mut Env) -> Option<u32> {
    let mut kids = self.t.children(n);
    while let Some(c) = kids.next(self.t) {
      if let Some(m) = self.eval(rule, c, env) {
        return Some(m);
      }
    }
    None
  }

  fn has(&self, rel: &Relation, n: u32, env: &mut Env) -> Option<u32> {
    let t = self.t;
    if let Some(f) = rel.field {
      let nd = t.child_by_field(n, f)?;
      return match &rel.stop {
        StopBy::Neighbor => self.eval(&rel.rule, nd, env),
        StopBy::End => self.find_in_range(rel, nd, t.sub_end[nd as usize], env),
        StopBy::Rule(stop) => {
          self.eval(&rel.rule, nd, env).or_else(|| if self.matches_fresh(stop, nd) { None } else { self.find_child(&rel.rule, nd, env) })
        }
      };
    }
    match &rel.stop {
      StopBy::Neighbor => self.find_child(&rel.rule, n, env),
      StopBy::End => self.find_in_range(rel, n + 1, t.sub_end[n as usize], env),
      StopBy::Rule(stop) => self.has_until(rel, stop, n, env),
    }
  }

  fn has_until(&self, rel: &Relation, stop: &Rule, n: u32, env: &mut Env) -> Option<u32> {
    let t = self.t;
    let mut child = n + 1;
    while child < t.sub_end[n as usize] {
      if let Some(m) = self.eval(&rel.rule, child, env) {
        return Some(m);
      }
      // Pre-order traversal. A stop node was tested above, and its descendants are skipped.
      child = if self.matches_fresh(stop, child) { t.sub_end[child as usize] } else { child + 1 };
    }
    None
  }

  /// `precedes` / `follows`. The sibling walk reproduces tree-sitter's, quirks
  /// included: the immediate neighbour comes from `ts_node_next_sibling`, a
  /// longer walk from a cursor placed by byte offset.
  fn sibling(&self, rel: &Relation, n: u32, env: &mut Env, forward: bool) -> Option<u32> {
    let t = self.t;
    if matches!(rel.stop, StopBy::Neighbor) {
      let c = if forward { t.ts_next_sibling(n) } else { t.prev[n as usize] };
      return if c == NONE { None } else { self.eval(&rel.rule, c, env) };
    }
    let anchor = t.sibling_anchor(n);
    if anchor == NONE {
      return None;
    }
    let step = |x: u32| if forward { t.next_sibling(x) } else { t.prev[x as usize] };
    let mut cur = step(anchor);
    let mut stopped = false;
    while cur != NONE {
      if let StopBy::Rule(stop) = &rel.stop {
        if stopped {
          break;
        }
        stopped = self.matches_fresh(stop, cur);
      }
      if let Some(m) = self.eval(&rel.rule, cur, env) {
        return Some(m);
      }
      cur = step(cur);
    }
    None
  }

  fn nth_child(&self, nc: &NthChild, n: u32, env: &mut Env) -> Option<u32> {
    let t = self.t;
    let parent = t.parent[n as usize];
    if parent == NONE {
      return None;
    }
    let index = if let Some(rule) = &nc.of_rule {
      let mut matched = Vec::new();
      let mut kids = t.children(parent);
      while let Some(c) = kids.next(t) {
        if !t.is_named(c) {
          continue;
        }
        let mark = env.mark();
        if let Some(m) = self.eval(rule, c, env) {
          matched.push(m);
        }
        env.truncate(mark);
      }
      if nc.reverse {
        matched.reverse();
      }
      let index = matched.iter().position(|&c| c == n)?;
      self.eval(rule, n, env);
      index
    } else {
      // Position among named siblings, counted without collecting them.
      if !t.is_named(n) {
        return None;
      }
      let (mut before, mut after, mut seen) = (0usize, 0usize, false);
      let mut kids = t.children(parent);
      while let Some(c) = kids.next(t) {
        if c == n {
          seen = true;
          if !nc.reverse {
            break;
          }
        } else if t.is_named(c) {
          if seen { after += 1 } else { before += 1 }
        }
      }
      if nc.reverse { after } else { before }
    };
    nc.is_matched(index).then_some(n)
  }

  // --------------------------------------------------------------- patterns

  pub fn match_pattern(&self, p: &Pattern, n: u32, env: &mut Env) -> bool {
    if let Some(k) = p.root_kind
      && self.kind(n) != k
    {
      return false;
    }
    let mark = env.mark();
    let ok = if let (PNode::MetaVar(mv), Strictness::Smart) = (&p.node, p.strictness) {
      self.match_meta_var(mv, n, env)
    } else {
      matches!(self.match_node(&p.node, n, env, p.strictness), One::MatchedBoth)
    };
    if !ok {
      env.truncate(mark);
    }
    ok
  }

  fn match_node(&self, goal: &PNode, cand: u32, env: &mut Env, s: Strictness) -> One {
    match goal {
      PNode::Terminal { text, kind, named } => self.match_terminal(s, *named, text, *kind, cand),
      PNode::MetaVar(mv) => {
        if should_skip_comment(s) && self.t.is_extra(cand) {
          return One::SkipCandidate;
        }
        if self.match_meta_var(mv, cand, env) { One::MatchedBoth } else { One::NoMatch }
      }
      PNode::Internal { kind, children } => {
        let kind_matched = s == Strictness::Template || kinds_matching(*kind, self.kind(cand));
        if !kind_matched {
          return One::NoMatch;
        }
        if self.match_nodes(children, self.t.children(cand), env, s) { One::MatchedBoth } else { One::NoMatch }
      }
    }
  }

  fn match_terminal(&self, s: Strictness, is_named: bool, text: &str, goal_kind: u16, cand: u32) -> One {
    use Strictness as M;
    let cand_named = self.t.is_named(cand);
    let is_kind_matched = kinds_matching(goal_kind, self.kind(cand));
    if is_kind_matched && (!is_named || text == self.text(cand)) {
      return One::MatchedBoth;
    }
    if should_skip_comment(s) && self.t.is_extra(cand) {
      return One::SkipCandidate;
    }
    let (skip_goal, skip_candidate) = match s {
      M::Cst => (false, false),
      M::Smart => (false, !cand_named),
      M::Ast | M::Relaxed => (!is_named, !cand_named),
      M::Signature => {
        if is_kind_matched {
          return One::MatchedBoth;
        }
        (!is_named, !cand_named)
      }
      M::Template => {
        if text == self.text(cand) {
          return One::MatchedBoth;
        }
        (false, !cand_named)
      }
    };
    match (skip_goal, skip_candidate) {
      (true, true) => One::SkipBoth,
      (true, false) => One::SkipGoal,
      (false, true) => One::SkipCandidate,
      (false, false) => One::NoMatch,
    }
  }

  fn match_meta_var(&self, mv: &MetaVar, cand: u32, env: &mut Env) -> bool {
    match mv {
      MetaVar::Capture(var, named) => {
        if *named && !self.t.is_named(cand) {
          return false;
        }
        self.insert_single(env, *var, cand)
      }
      MetaVar::Dropped(named) => !(*named && !self.t.is_named(cand)),
      MetaVar::Multiple => true,
      MetaVar::MultiCapture(var) => self.insert_multi(env, *var, &[cand]),
    }
  }

  fn insert_single(&self, env: &mut Env, var: u16, cand: u32) -> bool {
    if let Some(prev) = env.get_single(var)
      && !self.exact(prev, cand)
    {
      return false;
    }
    env.single.push((var, cand));
    true
  }

  fn insert_multi(&self, env: &mut Env, var: u16, cands: &[u32]) -> bool {
    if let Some((start, len)) = env.get_multi(var) {
      let t = self.t;
      let mut prev = env.pool[start as usize..(start + len) as usize].iter().copied().filter(|&n| t.is_named(n));
      let mut next = cands.iter().copied().filter(|&n| t.is_named(n));
      loop {
        match (prev.next(), next.next()) {
          (Some(a), Some(b)) if self.exact(a, b) => continue,
          (None, None) => break,
          _ => return false,
        }
      }
    }
    let start = env.pool.len() as u32;
    env.pool.extend_from_slice(cands);
    env.multi.push((var, start, cands.len() as u32));
    true
  }

  /// Are two nodes the same code? What a repeated metavariable requires.
  fn exact(&self, goal: u32, cand: u32) -> bool {
    let t = self.t;
    let (mut g, mut c) = (goal, cand);
    loop {
      if g != c {
        if t.is_named_leaf(g) || t.is_named_leaf(c) {
          if self.text(g) != self.text(c) {
            return false;
          }
        } else {
          if self.kind(g) != self.kind(c) {
            return false;
          }
          match (t.children(g).peek(), t.children(c).peek()) {
            (Some(a), Some(b)) => {
              (g, c) = (a, b);
              continue;
            }
            (None, None) => {}
            _ => return false,
          }
        }
      }
      // Ascend both trees together until we can compare their next siblings.
      loop {
        if g == goal || c == cand {
          return g == goal && c == cand;
        }
        match (t.next_sibling(g), t.next_sibling(c)) {
          (NONE, NONE) => (g, c) = (t.parent[g as usize], t.parent[c as usize]),
          (NONE, _) | (_, NONE) => return false,
          (a, b) => {
            (g, c) = (a, b);
            break;
          }
        }
      }
    }
  }

  fn match_nodes(&self, goals: &[PNode], mut kids: Kids, env: &mut Env, s: Strictness) -> bool {
    let t = self.t;
    let mut gi = 0usize;
    if kids.peek().is_none() {
      return false;
    }
    loop {
      match self.may_match_ellipsis(goals, &mut gi, &mut kids, env, s) {
        None => return false,
        Some(Flow::Return) => return true,
        Some(Flow::Continue) => continue,
        Some(Flow::Fallthrough) => {}
      }
      match self.match_single_skip_trivial(goals, &mut gi, &mut kids, env, s) {
        None => return false,
        Some(Flow::Return) => return true,
        Some(Flow::Continue) => continue,
        Some(Flow::Fallthrough) => {}
      }
      if gi < goals.len() {
        gi += 1;
        kids.next(t);
      }
      if gi >= goals.len() {
        while let Some(c) = kids.next(t) {
          if !self.should_skip_trailing(s, c) {
            return false;
          }
        }
        return true;
      }
      if kids.peek().is_none() {
        return false;
      }
    }
  }

  fn should_skip_trailing(&self, s: Strictness, cand: u32) -> bool {
    use Strictness as M;
    let t = self.t;
    match s {
      M::Cst => false,
      M::Smart => true,
      M::Ast => !t.is_named(cand),
      M::Relaxed | M::Signature => !t.is_named(cand) || t.is_extra(cand),
      M::Template => t.is_extra(cand),
    }
  }

  fn bind_ellipsis(
    &self,
    env: &mut Env,
    name: Option<u16>,
    matched: &mut Vec<u32>,
    rest: Option<&mut Kids>,
    skipped_anonymous: usize,
  ) -> Option<()> {
    if let Some(kids) = rest {
      while let Some(c) = kids.next(self.t) {
        matched.push(c);
      }
    }
    if let Some(var) = name {
      let keep = matched.len().saturating_sub(skipped_anonymous);
      matched.truncate(keep);
      if !self.insert_multi(env, var, matched) {
        return None;
      }
    }
    Some(())
  }

  fn may_match_ellipsis(&self, goals: &[PNode], gi: &mut usize, kids: &mut Kids, env: &mut Env, s: Strictness) -> Option<Flow> {
    let t = self.t;
    let Some(curr) = goals.get(*gi) else { return Some(Flow::Return) };
    let Some(name) = curr.ellipsis() else { return Some(Flow::Fallthrough) };
    let mut matched = Vec::new();
    *gi += 1;
    if *gi >= goals.len() {
      self.bind_ellipsis(env, name, &mut matched, Some(kids), 0)?;
      return Some(Flow::Return);
    }
    let mut skipped_anonymous = 0;
    while goals[*gi].is_trivial() {
      *gi += 1;
      skipped_anonymous += 1;
      if *gi >= goals.len() {
        self.bind_ellipsis(env, name, &mut matched, Some(kids), skipped_anonymous)?;
        return Some(Flow::Return);
      }
    }
    if goals[*gi].ellipsis().is_some() {
      matched.push(kids.next(t).expect("a candidate was peeked"));
      kids.peek()?;
      self.bind_ellipsis(env, name, &mut matched, None, skipped_anonymous)?;
      return Some(Flow::Continue);
    }
    loop {
      let mark = env.mark();
      let hit = matches!(self.match_node(&goals[*gi], kids.peek().expect("a candidate was peeked"), env, s), One::MatchedBoth);
      env.truncate(mark);
      if hit {
        self.bind_ellipsis(env, name, &mut matched, None, skipped_anonymous)?;
        return Some(Flow::Fallthrough);
      }
      matched.push(kids.next(t).expect("a candidate was peeked"));
      kids.peek()?;
    }
  }

  fn match_single_skip_trivial(&self, goals: &[PNode], gi: &mut usize, kids: &mut Kids, env: &mut Env, s: Strictness) -> Option<Flow> {
    loop {
      let Some(cand) = kids.peek() else {
        return should_skip_goal(s, goals, gi).then_some(Flow::Fallthrough);
      };
      match self.match_node(&goals[*gi], cand, env, s) {
        One::MatchedBoth => return Some(Flow::Fallthrough),
        One::SkipGoal => {
          *gi += 1;
          if *gi >= goals.len() {
            return Some(Flow::Fallthrough);
          }
        }
        One::SkipBoth => {
          kids.next(self.t);
          *gi += 1;
          if *gi >= goals.len() {
            return Some(Flow::Fallthrough);
          }
        }
        One::SkipCandidate => {
          kids.next(self.t);
        }
        One::NoMatch => return None,
      }
    }
  }
}

#[inline]
fn kinds_matching(goal: u16, cand: u16) -> bool {
  goal == cand || goal == ERROR_KIND
}

fn should_skip_comment(s: Strictness) -> bool {
  !matches!(s, Strictness::Cst | Strictness::Ast)
}

/// With the candidates used up, can the goals that remain be dropped?
fn should_skip_goal(s: Strictness, goals: &[PNode], gi: &mut usize) -> bool {
  use Strictness as M;
  while let Some(pattern) = goals.get(*gi) {
    let skipped = match s {
      M::Cst => false,
      M::Smart | M::Template => matches!(pattern, PNode::MetaVar(MetaVar::Multiple | MetaVar::MultiCapture(_))),
      M::Ast | M::Relaxed | M::Signature => match pattern {
        PNode::MetaVar(MetaVar::Multiple | MetaVar::MultiCapture(_)) => true,
        PNode::MetaVar(MetaVar::Dropped(named) | MetaVar::Capture(_, named)) => !named,
        PNode::Terminal { named, .. } => !named,
        PNode::Internal { .. } => false,
      },
    };
    if !skipped {
      return false;
    }
    *gi += 1;
  }
  true
}
