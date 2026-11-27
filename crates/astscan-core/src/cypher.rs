//! Read-only Cypher patterns over a single flattened AST.

use crate::lang::Lang;
use crate::simd::find_u16;
use crate::tree::FlatTree;

const MAX_NESTING: usize = 128;

pub(crate) struct Step {
  pub kind: u16,
  predicates: Vec<Predicate>,
}

enum Edge {
  Child(Option<u16>),
  Descendant,
}

struct Predicate {
  edge: Edge,
  negated: bool,
  /// Index of an earlier step in the plan.
  target: usize,
}

impl Step {
  pub fn matches(&self, node: u32, tree: &FlatTree, hits: &[Vec<u32>]) -> bool {
    self.predicates.iter().all(|p| p.exists(node, tree, &hits[p.target]) != p.negated)
  }
}

impl Predicate {
  fn exists(&self, node: u32, tree: &FlatTree, targets: &[u32]) -> bool {
    // Every target is in pre-order. If the first one after this node is
    // past its subtree, there is no witness anywhere inside it.
    let start = targets.partition_point(|&target| target <= node);
    if targets.get(start).is_none_or(|&target| target >= tree.sub_end[node as usize]) {
      return false;
    }
    match self.edge {
      Edge::Descendant => true,
      Edge::Child(field) => {
        let mut children = tree.children(node);
        while let Some(child) = children.next(tree) {
          if field.is_none_or(|field| tree.field[child as usize] == field) && targets[start..].binary_search(&child).is_ok() {
            return true;
          }
        }
        false
      }
    }
  }
}

/// Resolve inner predicates once per file; outer candidates use range lookups
/// against these sorted witnesses, even when their subtrees overlap.
pub(crate) fn evaluate<'a>(steps: &[Step], tree: &FlatTree, hits: &'a mut Vec<Vec<u32>>) -> &'a [Vec<u32>] {
  if hits.len() < steps.len() {
    hits.resize_with(steps.len(), Vec::new);
  }
  for (index, step) in steps.iter().enumerate() {
    let (prior, rest) = hits.split_at_mut(index);
    let current = &mut rest[0];
    current.clear();
    let mut from = 0;
    while let Some(offset) = find_u16(&tree.kind[from..], step.kind) {
      let node = from + offset;
      if step.matches(node as u32, tree, prior) {
        current.push(node as u32);
      }
      from = node + 1;
    }
  }
  &hits[..steps.len()]
}

pub(crate) fn compile(text: &str, lang: &Lang, steps: &mut Vec<Step>) -> Result<(Step, Option<String>), String> {
  let mut parser = Parser { rest: text, len: text.len(), lang, steps, scope: Vec::new() };
  parser.expect_keyword("MATCH")?;
  let (name, kind) = parser.node()?;
  let name = name.ok_or_else(|| parser.error("the outer MATCH needs a variable"))?;
  let mut has_filter = parser.keyword("WHERE");
  let pattern = if has_filter && parser.keyword("PATTERN") {
    parser.expect_symbol("(")?;
    let pattern = parser.string()?;
    parser.expect_symbol(")")?;
    has_filter = parser.keyword("AND");
    Some(pattern)
  } else {
    None
  };
  let root = parser.step(Some(name), kind, has_filter)?;
  parser.expect_keyword("RETURN")?;
  if parser.name() != Some(name) {
    return Err(parser.error(&format!("RETURN must use `{name}`")));
  }
  parser.symbol(";");
  if !parser.rest.trim().is_empty() {
    return Err(parser.error("expected end of query"));
  }
  Ok((root, pattern))
}

struct Parser<'a, 'p> {
  rest: &'a str,
  len: usize,
  lang: &'p Lang,
  steps: &'p mut Vec<Step>,
  scope: Vec<&'a str>,
}

impl<'a> Parser<'a, '_> {
  fn name(&mut self) -> Option<&'a str> {
    let rest = self.rest.trim_start();
    if !rest.as_bytes().first().is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_') {
      return None;
    }
    let len = rest.bytes().take_while(|c| c.is_ascii_alphanumeric() || *c == b'_').count();
    let (name, rest) = rest.split_at(len);
    self.rest = rest;
    Some(name)
  }

  fn keyword(&mut self, keyword: &str) -> bool {
    let saved = self.rest;
    if self.name().is_some_and(|name| name.eq_ignore_ascii_case(keyword)) {
      true
    } else {
      self.rest = saved;
      false
    }
  }

  fn symbol(&mut self, symbol: &str) -> bool {
    let Some(rest) = self.rest.trim_start().strip_prefix(symbol) else { return false };
    self.rest = rest;
    true
  }

  fn expect_keyword(&mut self, keyword: &str) -> Result<(), String> {
    self.keyword(keyword).then_some(()).ok_or_else(|| self.error(&format!("expected `{keyword}`")))
  }

  fn expect_symbol(&mut self, symbol: &str) -> Result<(), String> {
    self.symbol(symbol).then_some(()).ok_or_else(|| self.error(&format!("expected `{symbol}`")))
  }

  fn string(&mut self) -> Result<String, String> {
    let rest = self.rest.trim_start();
    let body = rest.strip_prefix('"').ok_or_else(|| self.error("expected a double-quoted string"))?;
    let mut escaped = false;
    for (index, ch) in body.char_indices() {
      if escaped {
        escaped = false;
      } else if ch == '\\' {
        escaped = true;
      } else if ch == '"' {
        let (value, tail) = rest.split_at(index + 2);
        let value = serde_json::from_str(value).map_err(|e| self.error(&format!("invalid string: {e}")))?;
        self.rest = tail;
        return Ok(value);
      }
    }
    Err(self.error("unterminated string"))
  }

  fn error(&self, message: &str) -> String {
    format!("cypher at byte {}: {message}", self.len - self.rest.trim_start().len())
  }

  fn node(&mut self) -> Result<(Option<&'a str>, u16), String> {
    self.expect_symbol("(")?;
    let variable = self.name();
    if variable.is_some_and(|name| self.scope.contains(&name)) {
      return Err(self.error("target variable is already bound"));
    }
    self.expect_symbol(":")?;
    let kind = self.name().ok_or_else(|| self.error("expected an AST kind"))?;
    let id = self.lang.kind_to_id(kind);
    if id == 0 || id as usize >= self.lang.nkinds {
      return Err(self.error(&format!("unknown AST kind `{kind}` for {}", self.lang.name)));
    }
    self.expect_symbol(")")?;
    Ok((variable, id))
  }

  fn step(&mut self, variable: Option<&'a str>, kind: u16, has_filter: bool) -> Result<Step, String> {
    let mut step = Step { kind, predicates: Vec::new() };
    if has_filter {
      let name = variable.ok_or_else(|| self.error("a node with WHERE needs a variable"))?;
      if self.scope.len() == MAX_NESTING {
        return Err(self.error(&format!("query nesting exceeds {MAX_NESTING} WHERE clauses")));
      }
      self.scope.push(name);
      loop {
        step.predicates.push(self.predicate(name)?);
        if !self.keyword("AND") {
          break;
        }
      }
      self.scope.pop();
    }
    Ok(step)
  }

  fn predicate(&mut self, anchor: &str) -> Result<Predicate, String> {
    let negated = self.keyword("NOT");
    self.expect_keyword("EXISTS")?;
    self.expect_symbol("{")?;
    self.expect_keyword("MATCH")?;
    self.expect_symbol("(")?;
    if self.name() != Some(anchor) {
      return Err(self.error(&format!("MATCH must start from the enclosing variable `{anchor}`")));
    }
    self.expect_symbol(")")?;
    self.expect_symbol("-")?;
    self.expect_symbol("[")?;
    self.expect_symbol(":")?;
    let edge = match self.name() {
      Some("CHILD") if self.symbol("{") => {
        self.expect_keyword("field")?;
        self.expect_symbol(":")?;
        let name = self.string()?;
        let field = self.lang.field_to_id(&name).ok_or_else(|| self.error(&format!("unknown AST field `{name}`")))?;
        self.expect_symbol("}")?;
        Edge::Child(Some(field))
      }
      Some("CHILD") => Edge::Child(None),
      Some("DESCENDANT") => Edge::Descendant,
      _ => return Err(self.error("expected CHILD or DESCENDANT")),
    };
    self.expect_symbol("]")?;
    self.expect_symbol("->")?;
    let (name, kind) = self.node()?;
    let has_filter = self.keyword("WHERE");
    let step = self.step(name, kind, has_filter)?;
    self.expect_symbol("}")?;
    let target = self.steps.len();
    self.steps.push(step);
    Ok(Predicate { edge, negated, target })
  }
}
