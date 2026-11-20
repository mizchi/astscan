//! Languages: the built-in grammars and ones loaded from a dynamic library.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tree_sitter::Language;

pub struct Lang {
  /// Canonical name, as reported in output (`Jsx` resolves to `JavaScript`).
  pub name: String,
  pub ts: Language,
  /// What `$` becomes inside a pattern so the pattern parses as code.
  pub expando: char,
  pub meta: char,
  pub exts: Vec<String>,
  /// Indexed by kind id: does the kind's name contain "comment"?
  pub comment_kind: Vec<bool>,
  pub nkinds: usize,
  _lib: Option<libloading::Library>,
}

impl Lang {
  fn new(name: &str, ts: Language, expando: char, meta: char, exts: Vec<String>, lib: Option<libloading::Library>) -> Self {
    let nkinds = ts.node_kind_count();
    let comment_kind = (0..nkinds).map(|i| ts.node_kind_for_id(i as u16).is_some_and(|k| k.contains("comment"))).collect();
    Lang { name: name.to_string(), ts, expando, meta, exts, comment_kind, nkinds, _lib: lib }
  }

  /// 0 means the grammar has no such kind.
  pub fn kind_to_id(&self, kind: &str) -> u16 {
    self.ts.id_for_node_kind(kind, true)
  }
  pub fn field_to_id(&self, field: &str) -> Option<u16> {
    self.ts.field_id_for_name(field).map(|f| f.get())
  }

  /// Rewrite `$VAR` to the expando character where the language cannot parse `$`.
  pub fn pre_process_pattern(&self, query: &str) -> String {
    if self.expando == self.meta {
      return query.to_string();
    }
    let mut ret = String::with_capacity(query.len());
    let mut count = 0;
    for c in query.chars() {
      if c == self.meta {
        count += 1;
        continue;
      }
      let need_replace = matches!(c, 'A'..='Z' | '_') || count == 3;
      let sigil = if need_replace { self.expando } else { self.meta };
      ret.extend(std::iter::repeat_n(sigil, count));
      count = 0;
      ret.push(c);
    }
    let sigil = if count == 3 { self.expando } else { self.meta };
    ret.extend(std::iter::repeat_n(sigil, count));
    ret
  }
}

struct Builtin {
  name: &'static str,
  aliases: &'static [&'static str],
  exts: &'static [&'static str],
  expando: char,
  ts: fn() -> Language,
}

/// ast-grep's built-in languages, in its own order: the order an extension
/// is resolved in, with the expando character each one needs.
const BUILTINS: &[Builtin] = &[
  Builtin {
    name: "Bash",
    aliases: &["bash"],
    exts: &["bash", "bats", "cgi", "command", "env", "fcgi", "ksh", "sh", "tmux", "tool", "zsh"],
    expando: '$',
    ts: || tree_sitter_bash::LANGUAGE.into(),
  },
  Builtin { name: "C", aliases: &["c"], exts: &["c", "h"], expando: '\u{10000}', ts: || tree_sitter_c::LANGUAGE.into() },
  Builtin {
    name: "Cpp",
    aliases: &["cc", "c++", "cpp", "cxx"],
    exts: &["cc", "hpp", "cpp", "c++", "hh", "cxx", "cu", "ino"],
    expando: '\u{10000}',
    ts: || tree_sitter_cpp::LANGUAGE.into(),
  },
  Builtin { name: "CSharp", aliases: &["cs", "csharp"], exts: &["cs"], expando: 'µ', ts: || tree_sitter_c_sharp::LANGUAGE.into() },
  Builtin { name: "Css", aliases: &["css"], exts: &["css", "scss"], expando: '_', ts: || tree_sitter_css::LANGUAGE.into() },
  Builtin { name: "Dart", aliases: &["dart"], exts: &["dart"], expando: '$', ts: || tree_sitter_dart::LANGUAGE.into() },
  Builtin { name: "Elixir", aliases: &["ex", "elixir"], exts: &["ex", "exs"], expando: 'µ', ts: || tree_sitter_elixir::LANGUAGE.into() },
  Builtin { name: "Go", aliases: &["go", "golang"], exts: &["go"], expando: 'µ', ts: || tree_sitter_go::LANGUAGE.into() },
  Builtin { name: "Haskell", aliases: &["hs", "haskell"], exts: &["hs"], expando: 'µ', ts: || tree_sitter_haskell::LANGUAGE.into() },
  Builtin {
    name: "Hcl",
    aliases: &["hcl"],
    exts: &["hcl", "nomad", "tf", "tfvars", "workflow"],
    expando: 'µ',
    ts: || tree_sitter_hcl::LANGUAGE.into(),
  },
  Builtin { name: "Html", aliases: &["html"], exts: &["html", "htm", "xhtml"], expando: 'z', ts: || tree_sitter_html::LANGUAGE.into() },
  Builtin { name: "Java", aliases: &["java"], exts: &["java"], expando: '$', ts: || tree_sitter_java::LANGUAGE.into() },
  Builtin {
    name: "JavaScript",
    aliases: &["javascript", "js", "jsx"],
    exts: &["cjs", "js", "mjs", "jsx"],
    expando: '$',
    ts: || tree_sitter_javascript::LANGUAGE.into(),
  },
  Builtin { name: "Json", aliases: &["json"], exts: &["json"], expando: '$', ts: || tree_sitter_json::LANGUAGE.into() },
  Builtin {
    name: "Kotlin",
    aliases: &["kotlin", "kt"],
    exts: &["kt", "ktm", "kts"],
    expando: 'µ',
    ts: || tree_sitter_kotlin::LANGUAGE.into(),
  },
  Builtin { name: "Lua", aliases: &["lua"], exts: &["lua"], expando: '$', ts: || tree_sitter_lua::LANGUAGE.into() },
  Builtin {
    name: "Markdown",
    aliases: &["markdown", "md"],
    exts: &["markdown", "md"],
    expando: '$',
    ts: || tree_sitter_md::LANGUAGE.into(),
  },
  Builtin { name: "Nix", aliases: &["nix"], exts: &["nix"], expando: '_', ts: || tree_sitter_nix::LANGUAGE.into() },
  Builtin { name: "Php", aliases: &["php"], exts: &["php"], expando: 'µ', ts: || tree_sitter_php::LANGUAGE_PHP_ONLY.into() },
  Builtin {
    name: "Python",
    aliases: &["py", "python"],
    exts: &["py", "py3", "pyi", "bzl", "bazel"],
    expando: 'µ',
    ts: || tree_sitter_python::LANGUAGE.into(),
  },
  Builtin {
    name: "Ruby",
    aliases: &["rb", "ruby"],
    exts: &["rb", "rbw", "gemspec"],
    expando: 'µ',
    ts: || tree_sitter_ruby::LANGUAGE.into(),
  },
  Builtin { name: "Rust", aliases: &["rs", "rust"], exts: &["rs"], expando: 'µ', ts: || tree_sitter_rust::LANGUAGE.into() },
  Builtin { name: "Scala", aliases: &["scala"], exts: &["scala", "sc", "sbt"], expando: '$', ts: || tree_sitter_scala::LANGUAGE.into() },
  Builtin { name: "Solidity", aliases: &["sol", "solidity"], exts: &["sol"], expando: '$', ts: || tree_sitter_solidity::LANGUAGE.into() },
  Builtin { name: "Swift", aliases: &["swift"], exts: &["swift"], expando: 'µ', ts: || tree_sitter_swift::LANGUAGE.into() },
  Builtin { name: "Tsx", aliases: &["tsx"], exts: &["tsx"], expando: '$', ts: || tree_sitter_typescript::LANGUAGE_TSX.into() },
  Builtin {
    name: "TypeScript",
    aliases: &["ts", "typescript"],
    exts: &["ts", "cts", "mts"],
    expando: '$',
    ts: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
  },
  Builtin { name: "Yaml", aliases: &["yaml", "yml"], exts: &["yaml", "yml"], expando: '$', ts: || tree_sitter_yaml::LANGUAGE.into() },
];

/// A grammar declared in `sgconfig.yml`'s `customLanguages`.
pub struct CustomDecl {
  pub name: String,
  pub library_path: std::path::PathBuf,
  pub symbol: String,
  pub meta_var_char: Option<char>,
  pub expando_char: Option<char>,
  pub extensions: Vec<String>,
}

#[derive(Default)]
pub struct Registry {
  custom: Vec<Arc<Lang>>,
  /// The dynamic libraries the custom grammars were loaded from.
  pub custom_libraries: Vec<std::path::PathBuf>,
  builtin: HashMap<&'static str, Arc<Lang>>,
}

impl Registry {
  pub fn register_custom(&mut self, decl: CustomDecl) -> Result<(), String> {
    let path = std::fs::canonicalize(&decl.library_path)
      .map_err(|e| format!("cannot get the absolute path of dynamic lib {}: {e}", decl.library_path.display()))?;
    // SAFETY: loading a tree-sitter grammar the user declared; the symbol is
    // the grammar's constructor and returns a pointer to static data.
    let (lib, ts) = unsafe {
      let lib = libloading::Library::new(&path).map_err(|e| format!("cannot load lib {}: {e}", path.display()))?;
      let func: libloading::Symbol<unsafe extern "C" fn() -> Language> =
        lib.get(decl.symbol.as_bytes()).map_err(|e| format!("cannot read symbol {}: {e}", decl.symbol))?;
      let ts = func();
      (lib, ts)
    };
    let version = ts.abi_version();
    if !(tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION..=tree_sitter::LANGUAGE_VERSION).contains(&version) {
      return Err(format!("Incompatible tree-sitter parser version `{version}`"));
    }
    let meta = decl.meta_var_char.unwrap_or('$');
    let expando = decl.expando_char.unwrap_or(meta);
    self.custom.push(Arc::new(Lang::new(&decl.name, ts, expando, meta, decl.extensions, Some(lib))));
    self.custom_libraries.push(path);
    Ok(())
  }

  /// Built-in names and aliases are case-insensitive; a declared language is its exact name.
  pub fn resolve(&mut self, name: &str) -> Option<Arc<Lang>> {
    for b in BUILTINS {
      if b.aliases.iter().any(|a| name.eq_ignore_ascii_case(a)) {
        let lang = self
          .builtin
          .entry(b.name)
          .or_insert_with(|| Arc::new(Lang::new(b.name, (b.ts)(), b.expando, '$', b.exts.iter().map(|s| s.to_string()).collect(), None)));
        return Some(lang.clone());
      }
    }
    self.custom.iter().find(|l| l.name == name).cloned()
  }

  /// The canonical name behind a built-in alias, without loading the grammar.
  pub fn builtin_name(alias: &str) -> Option<&'static str> {
    BUILTINS.iter().find(|b| b.aliases.iter().any(|a| alias.eq_ignore_ascii_case(a))).map(|b| b.name)
  }

  /// The canonical name of the language a path belongs to: declared languages first.
  pub fn name_for_path(&self, path: &Path) -> Option<&str> {
    let ext = path.extension()?.to_str()?;
    if let Some(l) = self.custom.iter().find(|l| l.exts.iter().any(|e| e == ext)) {
      return Some(&l.name);
    }
    BUILTINS.iter().find(|b| b.exts.contains(&ext)).map(|b| b.name)
  }
}

/// The language whose files carry other languages inside them.
pub const HTML: &str = "Html";
/// What a `lang` attribute can name that ast-grep scans as embedded code.
pub const INJECTABLE: &[&str] = &["css", "js", "ts", "tsx"];

/// Byte ranges of `<script>` and `<style>` bodies in an HTML document, each
/// with the language its `lang` attribute names (`js` / `css` when it has none).
pub struct HtmlInjections {
  parser: tree_sitter::Parser,
  script: u16,
  style: u16,
  raw_text: u16,
  attribute: u16,
  attribute_name: u16,
  attribute_value: u16,
}

impl Default for HtmlInjections {
  fn default() -> Self {
    let ts: Language = tree_sitter_html::LANGUAGE.into();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&ts).expect("the bundled HTML grammar is compatible");
    let id = |kind: &str| ts.id_for_node_kind(kind, true);
    HtmlInjections {
      parser,
      script: id("script_element"),
      style: id("style_element"),
      raw_text: id("raw_text"),
      attribute: id("attribute"),
      attribute_name: id("attribute_name"),
      attribute_value: id("attribute_value"),
    }
  }
}

fn dfs<'t>(node: tree_sitter::Node<'t>, visit: &mut impl FnMut(tree_sitter::Node<'t>) -> bool) -> bool {
  if visit(node) {
    return true;
  }
  (0..node.child_count()).any(|i| dfs(node.child(i).expect("child index is in range"), visit))
}

fn find<'t>(node: tree_sitter::Node<'t>, kind: u16) -> Option<tree_sitter::Node<'t>> {
  let mut found = None;
  dfs(node, &mut |n| {
    if n.kind_id() == kind {
      found = Some(n);
    }
    found.is_some()
  });
  found
}

impl HtmlInjections {
  pub fn extract(&mut self, src: &str) -> Vec<(String, tree_sitter::Range)> {
    let Some(tree) = self.parser.parse(src.as_bytes(), None) else { return Vec::new() };
    let text = |n: tree_sitter::Node| &src[n.start_byte()..n.end_byte()];
    let mut out = Vec::new();
    for (element, default) in [(self.script, "js"), (self.style, "css")] {
      dfs(tree.root_node(), &mut |node| {
        if node.kind_id() != element {
          return false;
        }
        let mut lang = None;
        dfs(node, &mut |attr| {
          if attr.kind_id() == self.attribute
            && find(attr, self.attribute_name).is_some_and(|name| text(name) == "lang")
            && let Some(value) = find(attr, self.attribute_value)
          {
            lang = Some(text(value).to_string());
          }
          lang.is_some()
        });
        let content = (0..node.child_count()).filter_map(|i| node.child(i)).find(|c| c.kind_id() == self.raw_text);
        if let Some(content) = content {
          out.push((lang.unwrap_or_else(|| default.to_string()), content.range()));
        }
        false
      });
    }
    out
  }
}
