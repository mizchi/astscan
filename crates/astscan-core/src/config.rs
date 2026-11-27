//! Loading: an ast-grep rule file (multi-document YAML) and an optional
//! `sgconfig.yml` for `customLanguages`.
//!
//! Loading is two steps. `plan_rules` reads the file far enough to know the
//! grammars involved; `Plan::compile` builds the matchers. The split lets the
//! caller walk the tree while the rules compile.
//!
//! Rules that compile to the same matcher for the same grammar are merged
//! into one `Group`: the matcher runs once per node and the match is reported
//! under every id in the group.

use crate::lang::{CustomDecl, Lang, Registry};
use crate::prefilter::Prefilter;
use crate::rule::{Interner, RuleCore, compile_core};
use serde_yaml::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
  Hint,
  Info,
  Warning,
  Error,
}

impl Severity {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Hint => "hint",
      Self::Info => "info",
      Self::Warning => "warning",
      Self::Error => "error",
    }
  }
}

#[derive(Debug)]
pub struct RuleInfo {
  pub id: String,
  pub severity: Severity,
  pub message: String,
  pub note: Option<String>,
}

pub(crate) struct Group {
  pub core: RuleCore,
  pub metas: Vec<RuleInfo>,
}

/// Everything that runs against files of one grammar.
pub struct Unit {
  pub(crate) lang: Arc<Lang>,
  pub(crate) groups: Vec<Group>,
  /// Kind id to the groups whose matcher can match that kind.
  pub(crate) kind_map: Vec<Vec<u32>>,
  pub(crate) var_names: Vec<String>,
  pub(crate) regexes: Vec<regex::Regex>,
  pub(crate) prefilter: Option<Prefilter>,
}

#[cfg(feature = "internals")]
impl Unit {
  pub fn language(&self) -> &str {
    &self.lang.name
  }

  pub fn groups(&self) -> impl ExactSizeIterator<Item = &[RuleInfo]> {
    self.groups.iter().map(|group| group.metas.as_slice())
  }

  pub fn variables(&self) -> &[String] {
    &self.var_names
  }
}

/// Compiled rules, reusable by any number of scanners.
pub struct RuleSet {
  pub(crate) units: Vec<Unit>,
  rule_count: usize,
  warnings: Vec<String>,
}

impl RuleSet {
  /// Compile ast-grep rules from a multi-document YAML string.
  pub fn compile(yaml: &str, registry: &mut Registry) -> Result<Self, String> {
    plan_yaml(yaml, "YAML", registry)?.compile()
  }

  pub fn rule_count(&self) -> usize {
    self.rule_count
  }

  /// Rules with no eligible node kind are skipped and reported here.
  pub fn warnings(&self) -> &[String] {
    &self.warnings
  }

  pub(crate) fn unit_for(&self, lang_name: &str) -> Option<usize> {
    self.units.iter().position(|u| u.lang.name == lang_name)
  }

  #[cfg(feature = "internals")]
  pub fn units(&self) -> &[Unit] {
    &self.units
  }
}

fn current_target() -> String {
  let arch = std::env::consts::ARCH;
  match std::env::consts::OS {
    "macos" => format!("{arch}-apple-darwin"),
    "linux" => format!("{arch}-unknown-linux-{}", if cfg!(target_env = "musl") { "musl" } else { "gnu" }),
    "windows" => format!("{arch}-pc-windows-msvc"),
    os => format!("{arch}-unknown-{os}"),
  }
}

/// Load custom grammars from the given ast-grep config file.
///
/// # Safety
///
/// Every declared symbol must be a compatible tree-sitter grammar constructor
/// in a trusted native library.
pub unsafe fn load_sgconfig(path: &Path, registry: &mut Registry) -> Result<(), String> {
  let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
  let doc: Value = serde_yaml::from_str(&text).map_err(|e| format!("cannot parse config {}: {e}", path.display()))?;
  let base = path.parent().unwrap_or(Path::new("."));
  let Some(langs) = doc.get("customLanguages").and_then(|v| v.as_mapping()) else { return Ok(()) };
  for (name, decl) in langs {
    let name = name.as_str().ok_or("customLanguages key must be a string")?;
    let lib = match decl.get("libraryPath") {
      Some(Value::String(s)) => s.clone(),
      Some(Value::Mapping(m)) => {
        let target = current_target();
        m.get(target.as_str())
          .and_then(|v| v.as_str())
          .ok_or_else(|| format!("Target dynamic lib `{target}` is not configure"))?
          .to_string()
      }
      _ => return Err(format!("customLanguages.{name}: `libraryPath` is required")),
    };
    let one_char = |key: &str| decl.get(key).and_then(|v| v.as_str()).and_then(|s| s.chars().next());
    let extensions = decl
      .get("extensions")
      .and_then(|v| v.as_sequence())
      .map(|s| s.iter().filter_map(|e| e.as_str().map(str::to_string)).collect())
      .unwrap_or_default();
    // SAFETY: the caller guarantees the origin and signature of each symbol.
    unsafe {
      registry.register_custom(CustomDecl {
        name: name.to_string(),
        library_path: base.join(lib),
        symbol: decl.get("languageSymbol").and_then(|v| v.as_str()).map(str::to_string).unwrap_or_else(|| format!("tree_sitter_{name}")),
        meta_var_char: one_char("metaVarChar"),
        expando_char: one_char("expandoChar"),
        extensions,
      })
    }?;
  }
  Ok(())
}

const IGNORED_FIELDS: &[&str] =
  &["id", "language", "rule", "constraints", "utils", "message", "severity", "note", "url", "metadata", "labels", "fix"];
const UNSUPPORTED_FIELDS: &[&str] = &["transform", "rewriters", "files", "ignores"];

/// One rule, read and checked but not compiled.
struct Pending {
  doc: Value,
  meta: RuleInfo,
  /// Index of its grammar in `Plan::langs`, and of its `Unit` once compiled.
  unit: usize,
}

/// A rule file read as far as knowing which grammars it has rules for. That
/// is all it takes to list the files to scan; compiling the matchers is the
/// slow part, and `compile` can run while the files are being listed.
pub struct Plan {
  /// One per grammar with rules, in order of first appearance.
  langs: Vec<Arc<Lang>>,
  pending: Vec<Pending>,
}

#[cfg(feature = "internals")]
pub fn plan_rules(path: &Path, registry: &mut Registry) -> Result<Plan, String> {
  let text = std::fs::read_to_string(path).map_err(|e| format!("Cannot read rule {}: {e}", path.display()))?;
  plan_yaml(&text, path.display(), registry)
}

fn plan_yaml(text: &str, label: impl std::fmt::Display, registry: &mut Registry) -> Result<Plan, String> {
  use serde::Deserialize;
  let mut plan = Plan { langs: Vec::new(), pending: Vec::new() };

  for de in serde_yaml::Deserializer::from_str(text) {
    let doc = Value::deserialize(de).map_err(|e| format!("Cannot parse rule {label}: {e}"))?;
    if doc.is_null() {
      continue;
    }
    let map = doc.as_mapping().ok_or("a rule document must be an object")?;
    let id = doc.get("id").and_then(|v| v.as_str()).ok_or("a rule needs an `id`")?;
    let fail = |e: String| format!("Cannot parse rule {id}: {e}");
    for key in map.keys() {
      let key = key.as_str().unwrap_or("");
      if UNSUPPORTED_FIELDS.contains(&key) {
        return Err(fail(format!("`{key}` is not supported by astscan")));
      }
      if !IGNORED_FIELDS.contains(&key) {
        return Err(fail(format!("unknown field `{key}`")));
      }
    }
    let severity = match doc.get("severity").and_then(Value::as_str).unwrap_or("hint") {
      "hint" => Severity::Hint,
      "info" => Severity::Info,
      "warning" => Severity::Warning,
      "error" => Severity::Error,
      "off" => continue,
      other => return Err(fail(format!("unknown severity `{other}`"))),
    };
    let lang_name = doc.get("language").and_then(|v| v.as_str()).ok_or_else(|| fail("a rule needs a `language`".into()))?;
    let lang = registry.resolve(lang_name).ok_or_else(|| fail(format!("{lang_name} is not supported!")))?;
    if doc.get("rule").is_none() {
      return Err(fail("a rule needs a `rule`".into()));
    }

    let meta = RuleInfo {
      id: id.to_string(),
      severity,
      note: doc.get("note").and_then(Value::as_str).map(str::to_string),
      message: doc.get("message").and_then(Value::as_str).unwrap_or_default().to_string(),
    };

    let unit = match plan.langs.iter().position(|l| Arc::ptr_eq(l, &lang)) {
      Some(unit) => unit,
      None => {
        plan.langs.push(lang);
        plan.langs.len() - 1
      }
    };
    plan.pending.push(Pending { doc, meta, unit });
  }
  Ok(plan)
}

impl Plan {
  #[cfg(feature = "internals")]
  pub fn languages(&self) -> &[Arc<Lang>] {
    &self.langs
  }

  /// Compile every rule. Unit `i` of the result is the grammar `langs[i]`.
  ///
  /// Grammars share nothing, so each is compiled on a thread of its own; a
  /// rule file for twenty languages is ready in the time its slowest one takes.
  pub fn compile(self) -> Result<RuleSet, String> {
    let rule_count = self.pending.len();
    let mut by_unit: Vec<Vec<(usize, Pending)>> = self.langs.iter().map(|_| Vec::new()).collect();
    for (index, pending) in self.pending.into_iter().enumerate() {
      by_unit[pending.unit].push((index, pending));
    }
    let results: Vec<Result<(Unit, Vec<Note>), Note>> = std::thread::scope(|scope| {
      let handles: Vec<_> = self
        .langs
        .iter()
        .zip(by_unit)
        .map(|(lang, pending)| {
          std::thread::Builder::new().stack_size(COMPILER_STACK).spawn_scoped(scope, move || compile_unit(lang, pending))
        })
        .collect();
      handles
        .into_iter()
        .map(|handle| match handle {
          Ok(handle) => handle.join().unwrap_or_else(|_| Err((0, "the rule compiler panicked".to_string()))),
          Err(e) => Err((0, format!("cannot start a rule compiler thread: {e}"))),
        })
        .collect()
    });

    // Report as a compile in file order would: the first rule that fails,
    // and the warnings in the order of the rules they are about.
    let (mut units, mut warnings, mut first_error) = (Vec::new(), Vec::new(), None::<Note>);
    for result in results {
      match result {
        Ok((unit, notes)) => {
          units.push(unit);
          warnings.extend(notes);
        }
        Err(note) => {
          if first_error.as_ref().is_none_or(|e| note.0 < e.0) {
            first_error = Some(note);
          }
        }
      }
    }
    if let Some((_, message)) = first_error {
      return Err(message);
    }
    warnings.sort();
    Ok(RuleSet { units, rule_count, warnings: warnings.into_iter().map(|(_, message)| message).collect() })
  }
}

/// Patterns are parsed and walked recursively.
const COMPILER_STACK: usize = 16 << 20;

/// A message about one rule, with the rule's position in the file.
type Note = (usize, String);

fn compile_unit(lang: &Arc<Lang>, pending: Vec<(usize, Pending)>) -> Result<(Unit, Vec<Note>), Note> {
  let mut vars = Interner::default();
  let mut unit = Unit {
    kind_map: vec![Vec::new(); lang.nkinds],
    lang: lang.clone(),
    groups: Vec::new(),
    var_names: Vec::new(),
    regexes: Vec::new(),
    prefilter: None,
  };
  let mut group_index: HashMap<String, usize> = HashMap::new();
  let mut warnings = Vec::new();

  for (index, Pending { doc, meta, .. }) in pending {
    let fail = |e: String| (index, format!("Cannot parse rule {}: {e}", meta.id));
    let rule = doc.get("rule").expect("checked when the rule was read");
    let (constraints, utils) = (doc.get("constraints"), doc.get("utils"));

    // Same grammar and same matcher: one evaluation, reported under every id.
    let null = Value::Null;
    let key = serde_json::to_string(&[rule, constraints.unwrap_or(&null), utils.unwrap_or(&null)]).map_err(|e| fail(e.to_string()))?;
    if let Some(&g) = group_index.get(&key) {
      unit.groups[g].metas.push(meta);
      continue;
    }
    let core = compile_core(rule, constraints, utils, lang, &mut vars, &mut unit.regexes).map_err(fail)?;
    let g = unit.groups.len();
    match &core.kinds {
      Some(kinds) => {
        for k in kinds.iter() {
          if let Some(slot) = unit.kind_map.get_mut(k as usize) {
            slot.push(g as u32);
          }
        }
      }
      // ast-grep's scan skips a rule it cannot key by kind, and says so.
      None => warnings.push((index, format!("rule `{}` must have kind", meta.id))),
    }
    unit.groups.push(Group { core, metas: vec![meta] });
    group_index.insert(key, g);
  }

  unit.var_names = vars.names;
  unit.prefilter = Prefilter::compile(&unit.groups, &unit.regexes);
  Ok((unit, warnings))
}
