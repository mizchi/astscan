//! Rules: the compiled form of an ast-grep rule object, and its compiler.
//!
//! The YAML surface is ast-grep's (`pattern`, `kind`, `regex`, `nthChild`,
//! `inside`/`has`/`precedes`/`follows`, `all`/`any`/`not`/`matches`, with
//! `constraints` and local `utils`). Nothing here evaluates anything; see
//! `eval.rs`.

use crate::lang::Lang;
use serde_yaml::Value;
use std::collections::HashMap;
use tree_sitter::{Node, Parser};

pub const ERROR_KIND: u16 = 65535;

// ---------------------------------------------------------------- kind sets

#[derive(Clone, Default, PartialEq, Debug)]
pub struct KindSet(Vec<u64>);

impl KindSet {
  pub fn one(k: u16) -> Self {
    let mut s = KindSet::default();
    s.insert(k);
    s
  }
  pub fn insert(&mut self, k: u16) {
    let w = (k >> 6) as usize;
    if self.0.len() <= w {
      self.0.resize(w + 1, 0);
    }
    self.0[w] |= 1 << (k & 63);
  }
  #[inline]
  pub fn contains(&self, k: u16) -> bool {
    self.0.get((k >> 6) as usize).is_some_and(|w| w & (1 << (k & 63)) != 0)
  }
  pub fn union_with(&mut self, o: &KindSet) {
    if self.0.len() < o.0.len() {
      self.0.resize(o.0.len(), 0);
    }
    for (a, b) in self.0.iter_mut().zip(&o.0) {
      *a |= b;
    }
  }
  pub fn intersect_with(&mut self, o: &KindSet) {
    for (i, a) in self.0.iter_mut().enumerate() {
      *a &= o.0.get(i).copied().unwrap_or(0);
    }
  }
  pub fn iter(&self) -> impl Iterator<Item = u16> + '_ {
    self.0.iter().enumerate().flat_map(|(w, bits)| (0..64).filter(move |b| bits & (1u64 << b) != 0).map(move |b| (w * 64 + b) as u16))
  }
  /// The only member, when there is exactly one.
  pub fn single(&self) -> Option<u16> {
    let mut it = self.iter();
    let first = it.next()?;
    it.next().is_none().then_some(first)
  }
}

// ----------------------------------------------------------------- patterns

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Strictness {
  Cst,
  Smart,
  Ast,
  Relaxed,
  Signature,
  Template,
}

#[derive(Clone, PartialEq, Debug)]
pub enum MetaVar {
  /// `$A` (named nodes only) or `$$A` (any node).
  Capture(u16, bool),
  /// `$_` / `$$_`.
  Dropped(bool),
  /// `$$$`.
  Multiple,
  /// `$$$ARGS`.
  MultiCapture(u16),
}

#[derive(Debug)]
pub enum PNode {
  MetaVar(MetaVar),
  Terminal { text: String, named: bool, kind: u16 },
  Internal { kind: u16, children: Vec<PNode> },
}

impl PNode {
  /// An anonymous token: skipped in the goal after an ellipsis.
  pub fn is_trivial(&self) -> bool {
    matches!(self, PNode::Terminal { named: false, .. })
  }
  pub fn ellipsis(&self) -> Option<Option<u16>> {
    match self {
      PNode::MetaVar(MetaVar::Multiple) => Some(None),
      PNode::MetaVar(MetaVar::MultiCapture(v)) => Some(Some(*v)),
      _ => None,
    }
  }
}

#[derive(Debug)]
pub struct Pattern {
  pub node: PNode,
  pub root_kind: Option<u16>,
  pub strictness: Strictness,
}

impl Pattern {
  fn potential_kinds(&self) -> Option<KindSet> {
    if self.strictness == Strictness::Template {
      return None;
    }
    let kind = match &self.node {
      PNode::Terminal { kind, .. } => *kind,
      PNode::MetaVar(_) => self.root_kind?,
      PNode::Internal { kind, .. } => {
        if *kind == ERROR_KIND {
          return None;
        }
        *kind
      }
    };
    Some(KindSet::one(kind))
  }
}

fn is_valid_first_char(c: char) -> bool {
  matches!(c, 'A'..='Z' | '_')
}
fn is_valid_meta_var_char(c: char) -> bool {
  is_valid_first_char(c) || c.is_ascii_digit()
}

fn extract_meta_var(src: &str, meta_char: char, vars: &mut Interner) -> Option<MetaVar> {
  let ellipsis: String = std::iter::repeat_n(meta_char, 3).collect();
  if src == ellipsis {
    return Some(MetaVar::Multiple);
  }
  if let Some(trimmed) = src.strip_prefix(&ellipsis) {
    if !trimmed.chars().all(is_valid_meta_var_char) {
      return None;
    }
    return Some(if trimmed.starts_with('_') { MetaVar::Multiple } else { MetaVar::MultiCapture(vars.intern(trimmed)) });
  }
  let trimmed = src.strip_prefix(meta_char)?;
  let (trimmed, named) = match trimmed.strip_prefix(meta_char) {
    Some(t) => (t, false),
    None => (trimmed, true),
  };
  if !trimmed.starts_with(is_valid_first_char) || !trimmed.chars().all(is_valid_meta_var_char) {
    return None;
  }
  Some(if trimmed.starts_with('_') { MetaVar::Dropped(named) } else { MetaVar::Capture(vars.intern(trimmed), named) })
}

fn node_to_pattern(node: Node, src: &str, lang: &Lang, vars: &mut Interner) -> PNode {
  let text = &src[node.start_byte()..node.end_byte()];
  if let Some(mv) = extract_meta_var(text, lang.expando, vars) {
    return PNode::MetaVar(mv);
  }
  if node.child_count() == 0 {
    return PNode::Terminal { text: text.to_string(), named: node.is_named(), kind: node.kind_id() };
  }
  let mut children = Vec::with_capacity(node.child_count() as usize);
  for i in 0..node.child_count() {
    let c = node.child(i).expect("child index is in range");
    if !c.is_missing() {
      children.push(node_to_pattern(c, src, lang, vars));
    }
  }
  PNode::Internal { kind: node.kind_id(), children }
}

fn is_single_node(n: &Node) -> bool {
  match n.child_count() {
    1 => true,
    2 => {
      let c = n.child(1).expect("second child must exist");
      c.is_missing() || c.kind().is_empty()
    }
    _ => false,
  }
}

fn find_kind<'t>(node: Node<'t>, kind: u16) -> Option<Node<'t>> {
  if node.kind_id() == kind {
    return Some(node);
  }
  for i in 0..node.child_count() {
    if let Some(found) = find_kind(node.child(i).expect("child index is in range"), kind) {
      return Some(found);
    }
  }
  None
}

pub fn compile_pattern(src: &str, selector: Option<&str>, lang: &Lang, vars: &mut Interner) -> Result<Pattern, String> {
  let processed = lang.pre_process_pattern(src);
  let mut parser = Parser::new();
  parser.set_language(&lang.ts).map_err(|e| e.to_string())?;
  let tree = parser.parse(processed.as_bytes(), None).ok_or_else(|| format!("Fails to parse the pattern query: `{src}`"))?;
  let root = tree.root_node();
  let pattern = if let Some(selector) = selector {
    let kind = lang.kind_to_id(selector);
    if kind == 0 {
      return Err(format!("Kind `{selector}` is invalid."));
    }
    let node = find_kind(root, kind)
      .ok_or_else(|| format!("Fails to create Contextual pattern: selector `{selector}` matches no node in the context `{src}`."))?;
    Pattern { root_kind: Some(node.kind_id()), node: node_to_pattern(node, &processed, lang, vars), strictness: Strictness::Smart }
  } else {
    if root.child_count() == 0 {
      return Err(format!("No AST root is detected. Please check the pattern source `{src}`."));
    }
    if !is_single_node(&root) {
      return Err(format!("Multiple AST nodes are detected. Please check the pattern source `{src}`."));
    }
    let mut inner = root;
    while is_single_node(&inner) {
      inner = inner.child(0).expect("a single node has a first child");
    }
    Pattern { root_kind: None, node: node_to_pattern(inner, &processed, lang, vars), strictness: Strictness::Smart }
  };
  if matches!(pattern.node, PNode::MetaVar(MetaVar::Multiple | MetaVar::MultiCapture(_))) {
    return Err(format!("Standalone multi meta variable `{src}` is invalid. Use `$VAR` or wrap `$$$VAR` in a larger pattern."));
  }
  Ok(pattern)
}

// -------------------------------------------------------------------- rules

pub enum StopBy {
  Neighbor,
  End,
  Rule(Rule),
}

pub struct Relation {
  pub rule: Rule,
  pub field: Option<u16>,
  pub stop: StopBy,
  /// Kinds the inner rule can match at all: lets a descendant search skip
  /// nodes by reading one array instead of evaluating the rule.
  pub kinds: Option<KindSet>,
  pub one_kind: Option<u16>,
}

pub struct NthChild {
  pub step: i32,
  pub offset: i32,
  pub of_rule: Option<Rule>,
  pub reverse: bool,
}

impl NthChild {
  pub fn is_matched(&self, index: usize) -> bool {
    let index = (index + 1) as i32;
    if self.step == 0 {
      index == self.offset
    } else {
      let n = index - self.offset;
      n / self.step >= 0 && n % self.step == 0
    }
  }
}

pub enum Rule {
  Pattern(Pattern),
  Kind(u16),
  Regex(regex::Regex),
  NthChild(Box<NthChild>),
  Inside(Box<Relation>),
  Has(Box<Relation>),
  Precedes(Box<Relation>),
  Follows(Box<Relation>),
  All(Vec<Rule>, Option<KindSet>),
  Any(Vec<Rule>, Option<KindSet>),
  Not(Box<Rule>),
  /// Index into the owning core's `utils`.
  Matches(u32),
}

pub struct RuleCore {
  pub rule: Rule,
  pub kinds: Option<KindSet>,
  pub constraints: Vec<(u16, Rule)>,
  pub utils: Vec<Rule>,
}

#[derive(Default)]
pub struct Interner {
  pub names: Vec<String>,
  map: HashMap<String, u16>,
}

impl Interner {
  pub fn intern(&mut self, name: &str) -> u16 {
    if let Some(&i) = self.map.get(name) {
      return i;
    }
    let i = self.names.len() as u16;
    self.names.push(name.to_string());
    self.map.insert(name.to_string(), i);
    i
  }
}

struct Compiler<'a> {
  lang: &'a Lang,
  vars: &'a mut Interner,
  util_ids: HashMap<String, u32>,
}

const ATOMIC: &[&str] = &["pattern", "kind", "regex", "nthChild", "range"];
const COMPOSITE: &[&str] = &["all", "any", "not", "matches"];
const RELATIONAL: &[&str] = &["inside", "has", "precedes", "follows"];

fn as_str<'v>(v: &'v Value, what: &str) -> Result<&'v str, String> {
  v.as_str().ok_or_else(|| format!("`{what}` must be a string"))
}

fn parse_an_b(input: &str) -> Result<(i32, i32), String> {
  #[derive(Clone, Copy)]
  enum S {
    Initial,
    N,
    Sign(bool),
    Num(bool),
  }
  let bad = || "Rule contains invalid nthChild: Invalid syntax".to_string();
  let (mut step, mut sign, mut num, mut state) = (0i32, 1i32, 0i32, S::Initial);
  for c in input.chars().filter(|c| !c.is_whitespace()) {
    state = match (state, c) {
      (S::Initial, '+' | '-') => {
        sign = if c == '+' { 1 } else { -1 };
        S::Sign(false)
      }
      (S::Initial, '0'..='9') => {
        num = (c as u8 - b'0') as i32;
        S::Num(false)
      }
      (S::Initial, 'n' | 'N') => {
        step = sign;
        S::N
      }
      (S::Sign(has_n), '0'..='9') => {
        num = (c as u8 - b'0') as i32;
        S::Num(has_n)
      }
      (S::Sign(false), 'n' | 'N') => {
        step = sign;
        S::N
      }
      (S::Num(has_n), '0'..='9') => {
        num = num * 10 + (c as u8 - b'0') as i32;
        S::Num(has_n)
      }
      (S::Num(false), 'n' | 'N') => {
        step = sign * num;
        num = 0;
        S::N
      }
      (S::N, '+' | '-') => {
        sign = if c == '+' { 1 } else { -1 };
        num = 0;
        S::Sign(true)
      }
      _ => return Err(bad()),
    };
  }
  if matches!(state, S::Sign(_) | S::Initial) { Err(bad()) } else { Ok((step, num * sign)) }
}

fn parse_position(v: &Value) -> Result<(i32, i32), String> {
  match v {
    Value::Number(n) => n.as_u64().map(|n| (0, n as i32)).ok_or_else(|| "Rule contains invalid nthChild.".to_string()),
    Value::String(s) => parse_an_b(s),
    _ => Err("Rule contains invalid nthChild.".to_string()),
  }
}

impl Compiler<'_> {
  fn rule(&mut self, v: &Value) -> Result<Rule, String> {
    self.rule_with(v, &[])
  }

  /// `extra` are keys the caller consumes itself (`stopBy`, `field`).
  fn rule_with(&mut self, v: &Value, extra: &[&str]) -> Result<Rule, String> {
    let map = v.as_mapping().ok_or("a rule must be an object")?;
    for key in map.keys() {
      let key = as_str(key, "rule key")?;
      if !ATOMIC.contains(&key) && !COMPOSITE.contains(&key) && !RELATIONAL.contains(&key) && !extra.contains(&key) {
        return Err(format!("unknown field `{key}` in rule"));
      }
    }
    let get = |k: &str| map.get(k).filter(|v| !v.is_null());
    let mut rules = Vec::with_capacity(1);

    if let Some(p) = get("pattern") {
      rules.push(Rule::Pattern(self.pattern(p)?));
    }
    if let Some(k) = get("kind") {
      let name = as_str(k, "kind")?;
      if name.contains(|c: char| c.is_whitespace() || matches!(c, '>' | '+' | '~' | ',' | ':' | '.' | '#' | '[')) {
        return Err(format!("Rule contains invalid kind matcher: selector syntax in `kind: {name}` is not supported by astscan"));
      }
      let id = self.lang.kind_to_id(name);
      if id == 0 {
        return Err(format!("Rule contains invalid kind matcher: Kind `{name}` is invalid."));
      }
      rules.push(Rule::Kind(id));
    }
    if let Some(r) = get("regex") {
      let re = regex::Regex::new(as_str(r, "regex")?).map_err(|e| format!("Rule contains invalid regex matcher: {e}"))?;
      rules.push(Rule::Regex(re));
    }
    if let Some(n) = get("nthChild") {
      rules.push(Rule::NthChild(Box::new(self.nth_child(n)?)));
    }
    if get("range").is_some() {
      return Err("`range` is not supported by astscan".to_string());
    }

    if let Some(all) = get("all") {
      let inner = self.rule_list(all, "all")?;
      rules.push(Rule::All(inner, None));
    }
    if let Some(any) = get("any") {
      let inner = self.rule_list(any, "any")?;
      rules.push(Rule::Any(inner, None));
    }
    if let Some(not) = get("not") {
      rules.push(Rule::Not(Box::new(self.rule(not)?)));
    }
    if let Some(m) = get("matches") {
      let id = m.as_str().ok_or("parameterized `matches` is not supported by astscan")?;
      let idx = *self.util_ids.get(id).ok_or_else(|| format!("Rule contains invalid matches reference: Rule `{id}` is not defined."))?;
      rules.push(Rule::Matches(idx));
    }

    if let Some(r) = get("inside") {
      rules.push(Rule::Inside(Box::new(self.relation(r, true)?)));
    }
    if let Some(r) = get("has") {
      rules.push(Rule::Has(Box::new(self.relation(r, true)?)));
    }
    if let Some(r) = get("precedes") {
      rules.push(Rule::Precedes(Box::new(self.relation(r, false)?)));
    }
    if let Some(r) = get("follows") {
      rules.push(Rule::Follows(Box::new(self.relation(r, false)?)));
    }

    match rules.len() {
      0 => Err("Rule must have one positive matcher.".to_string()),
      1 => Ok(rules.pop().expect("one rule")),
      _ => Ok(Rule::All(rules, None)),
    }
  }

  fn rule_list(&mut self, v: &Value, what: &str) -> Result<Vec<Rule>, String> {
    let seq = v.as_sequence().ok_or_else(|| format!("`{what}` must be a list of rules"))?;
    seq.iter().map(|r| self.rule(r)).collect()
  }

  fn pattern(&mut self, v: &Value) -> Result<Pattern, String> {
    let wrap = |e: String| format!("Rule contains invalid pattern matcher: {e}");
    match v {
      Value::String(s) => compile_pattern(s, None, self.lang, self.vars).map_err(wrap),
      Value::Mapping(m) => {
        let context = as_str(m.get("context").ok_or("contextual pattern needs `context`")?, "context")?;
        let selector = m.get("selector").filter(|v| !v.is_null()).map(|s| as_str(s, "selector")).transpose()?;
        let mut p = compile_pattern(context, selector, self.lang, self.vars).map_err(wrap)?;
        if let Some(s) = m.get("strictness").filter(|v| !v.is_null()) {
          p.strictness = match as_str(s, "strictness")? {
            "cst" => Strictness::Cst,
            "smart" => Strictness::Smart,
            "ast" => Strictness::Ast,
            "relaxed" => Strictness::Relaxed,
            "signature" => Strictness::Signature,
            "template" => Strictness::Template,
            other => return Err(format!("invalid strictness `{other}`")),
          };
        }
        Ok(p)
      }
      _ => Err("`pattern` must be a string or an object".to_string()),
    }
  }

  fn nth_child(&mut self, v: &Value) -> Result<NthChild, String> {
    if let Value::Mapping(m) = v {
      let (step, offset) = parse_position(m.get("position").ok_or("nthChild needs `position`")?)?;
      let of_rule = m.get("ofRule").filter(|v| !v.is_null()).map(|r| self.rule(r)).transpose()?;
      let reverse = m.get("reverse").and_then(|v| v.as_bool()).unwrap_or(false);
      return Ok(NthChild { step, offset, of_rule, reverse });
    }
    let (step, offset) = parse_position(v)?;
    Ok(NthChild { step, offset, of_rule: None, reverse: false })
  }

  fn relation(&mut self, v: &Value, allow_field: bool) -> Result<Relation, String> {
    let map = v.as_mapping().ok_or("a relational rule must be an object")?;
    let stop = match map.get("stopBy") {
      None | Some(Value::Null) => StopBy::Neighbor,
      Some(Value::String(s)) if s == "neighbor" => StopBy::Neighbor,
      Some(Value::String(s)) if s == "end" => StopBy::End,
      Some(Value::String(s)) => return Err(format!("unknown variant `{s}`, expected `neighbor`, `end` or a rule object")),
      Some(r) => StopBy::Rule(self.rule(r)?),
    };
    let field = match map.get("field").filter(|v| !v.is_null()) {
      None => None,
      Some(_) if !allow_field => return Err("field is only supported in has/inside.".to_string()),
      Some(f) => {
        let name = as_str(f, "field")?;
        Some(self.lang.field_to_id(name).ok_or_else(|| format!("Relational rule contains invalid field {name}."))?)
      }
    };
    let rule = self.rule_with(v, &["stopBy", "field"])?;
    Ok(Relation { rule, field, stop, kinds: None, one_kind: None })
  }
}

// ---------------------------------------------- potential kinds, post-compile

/// The util references a rule reaches without crossing a relational rule:
/// the edges a cycle is illegal along, and the ones kinds are derived through.
fn direct_refs(rule: &Rule, out: &mut Vec<u32>) {
  match rule {
    Rule::All(rs, _) | Rule::Any(rs, _) => rs.iter().for_each(|r| direct_refs(r, out)),
    Rule::Not(r) => direct_refs(r, out),
    Rule::Matches(i) => out.push(*i),
    _ => {}
  }
}

fn potential_kinds(rule: &Rule, util_kinds: &[Option<KindSet>]) -> Option<KindSet> {
  match rule {
    Rule::Pattern(p) => p.potential_kinds(),
    Rule::Kind(k) => Some(KindSet::one(*k)),
    Rule::Regex(_) | Rule::Not(_) | Rule::Inside(_) | Rule::Has(_) | Rule::Precedes(_) | Rule::Follows(_) => None,
    Rule::NthChild(n) => potential_kinds(n.of_rule.as_ref()?, util_kinds),
    Rule::All(rs, _) => {
      let mut set: Option<KindSet> = None;
      for r in rs {
        let Some(k) = potential_kinds(r, util_kinds) else { continue };
        match set.as_mut() {
          Some(s) => s.intersect_with(&k),
          None => set = Some(k),
        }
      }
      set
    }
    Rule::Any(rs, _) => {
      let mut set = KindSet::default();
      for r in rs {
        set.union_with(&potential_kinds(r, util_kinds)?);
      }
      Some(set)
    }
    Rule::Matches(i) => util_kinds[*i as usize].clone(),
  }
}

/// Store each `all`/`any`/relation's kind set on the node, so evaluation
/// rejects a wrong-kind node before running any sub-rule.
fn fill_kinds(rule: &mut Rule, util_kinds: &[Option<KindSet>]) {
  match rule {
    Rule::All(rs, _) | Rule::Any(rs, _) => {
      rs.iter_mut().for_each(|r| fill_kinds(r, util_kinds));
    }
    _ => {}
  }
  match rule {
    Rule::All(..) | Rule::Any(..) => {
      let k = potential_kinds(rule, util_kinds);
      if let Rule::All(_, kinds) | Rule::Any(_, kinds) = rule {
        *kinds = k;
      }
    }
    Rule::Not(r) => fill_kinds(r, util_kinds),
    Rule::NthChild(n) => {
      if let Some(r) = n.of_rule.as_mut() {
        fill_kinds(r, util_kinds)
      }
    }
    Rule::Inside(rel) | Rule::Has(rel) | Rule::Precedes(rel) | Rule::Follows(rel) => {
      fill_kinds(&mut rel.rule, util_kinds);
      if let StopBy::Rule(s) = &mut rel.stop {
        fill_kinds(s, util_kinds);
      }
      rel.kinds = potential_kinds(&rel.rule, util_kinds);
      rel.one_kind = rel.kinds.as_ref().and_then(|k| k.single());
    }
    Rule::Pattern(_) | Rule::Kind(_) | Rule::Regex(_) | Rule::Matches(_) => {}
  }
}

/// Compile one rule document's `rule` + `constraints` + `utils` for a language.
pub fn compile_core(
  rule: &Value,
  constraints: Option<&Value>,
  utils: Option<&Value>,
  lang: &Lang,
  vars: &mut Interner,
) -> Result<RuleCore, String> {
  let mut util_names: Vec<(&str, &Value)> = Vec::new();
  if let Some(utils) = utils.filter(|v| !v.is_null()) {
    let map = utils.as_mapping().ok_or("`utils` must be an object")?;
    for (k, v) in map {
      util_names.push((as_str(k, "utils key")?, v));
    }
  }
  let util_ids = util_names.iter().enumerate().map(|(i, (n, _))| (n.to_string(), i as u32)).collect();
  let mut c = Compiler { lang, vars, util_ids };

  let mut compiled_utils = Vec::with_capacity(util_names.len());
  for (name, v) in &util_names {
    compiled_utils.push(c.rule(v).map_err(|e| format!("`utils` is not configured correctly ({name}): {e}"))?);
  }
  let mut main = c.rule(rule).map_err(|e| format!("`rule` is not configured correctly: {e}"))?;
  let mut compiled_constraints = Vec::new();
  if let Some(cons) = constraints.filter(|v| !v.is_null()) {
    let map = cons.as_mapping().ok_or("`constraints` must be an object")?;
    for (k, v) in map {
      let var = c.vars.intern(as_str(k, "constraints key")?);
      compiled_constraints.push((var, c.rule(v).map_err(|e| format!("`constraints` is not configured correctly: {e}"))?));
    }
  }

  // Kinds of utils, in dependency order; a cycle along direct references is an error.
  let n = compiled_utils.len();
  let mut util_kinds: Vec<Option<KindSet>> = vec![None; n];
  let mut state = vec![0u8; n]; // 0 new, 1 visiting, 2 done
  fn visit(i: usize, utils: &[Rule], names: &[(&str, &Value)], state: &mut [u8], kinds: &mut [Option<KindSet>]) -> Result<(), String> {
    match state[i] {
      2 => return Ok(()),
      1 => return Err(format!("Rule `{}` has a cyclic dependency in its `matches` sub-rule.", names[i].0)),
      _ => {}
    }
    state[i] = 1;
    let mut refs = Vec::new();
    direct_refs(&utils[i], &mut refs);
    for r in refs {
      visit(r as usize, utils, names, state, kinds)?;
    }
    kinds[i] = potential_kinds(&utils[i], kinds);
    state[i] = 2;
    Ok(())
  }
  for i in 0..n {
    visit(i, &compiled_utils, &util_names, &mut state, &mut util_kinds)?;
  }
  for u in compiled_utils.iter_mut() {
    fill_kinds(u, &util_kinds);
  }
  fill_kinds(&mut main, &util_kinds);
  for (_, r) in compiled_constraints.iter_mut() {
    fill_kinds(r, &util_kinds);
  }
  let kinds = potential_kinds(&main, &util_kinds);
  Ok(RuleCore { rule: main, kinds, constraints: compiled_constraints, utils: compiled_utils })
}
