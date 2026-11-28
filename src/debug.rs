use crate::split_option;
use astscan_core::{Registry, load_sgconfig};
use serde_json::json;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use tree_sitter::{Node, Parser};

const USAGE: &str = "\
Usage: astscan debug [OPTIONS] [--] [FILE|-]

Print the Tree-sitter syntax tree as an S-expression. Read stdin when FILE
is omitted or is `-`.

Options:
  -l, --lang <LANGUAGE>  Grammar to use (default: infer from FILE)
  -c, --config <FILE>    sgconfig.yml, read for `customLanguages`
      --json             Emit nodes, parent IDs, fields and UTF-8 byte offsets
  -h, --help             Print this help
";

struct Args {
  file: PathBuf,
  language: Option<String>,
  config: Option<PathBuf>,
  json: bool,
}

fn parse_args(mut argv: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
  let (mut file, mut language, mut config, mut json) = (None, None, None, false);
  let mut only_file = false;
  while let Some(arg) = argv.next() {
    let (flag, inline) = if only_file { ("", None) } else { split_option(&arg) };
    let mut value = |name: &str| inline.map(str::to_owned).or_else(|| argv.next()).ok_or_else(|| format!("`{name}` needs a value"));
    match flag {
      "--" => only_file = true,
      "-h" | "--help" => {
        print!("{USAGE}");
        return Ok(None);
      }
      "-l" | "--lang" => language = Some(value("--lang")?),
      "-c" | "--config" => config = Some(PathBuf::from(value("--config")?)),
      "--json" if inline.is_none() => json = true,
      option if option.starts_with('-') && option.len() > 1 => return Err(format!("unknown debug option `{option}`")),
      _ => {
        if file.replace(PathBuf::from(arg)).is_some() {
          return Err("`debug` accepts one input file".into());
        }
      }
    }
  }
  Ok(Some(Args { file: file.unwrap_or_else(|| PathBuf::from("-")), language, config, json }))
}

fn write_json(out: &mut impl Write, language: &str, root: Node<'_>) -> io::Result<()> {
  out.write_all(b"{\"language\":")?;
  serde_json::to_writer(&mut *out, language)?;
  write!(out, ",\"hasErrors\":{},\"nodes\":[", root.has_error())?;
  let mut cursor = root.walk();
  let mut parents = Vec::new();
  let mut id = 0usize;
  loop {
    let node = cursor.node();
    if id != 0 {
      out.write_all(b",")?;
    }
    serde_json::to_writer(
      &mut *out,
      &json!({
        "id": id,
        "parent": parents.last(),
        "kind": node.kind(),
        "field": cursor.field_name(),
        "named": node.is_named(),
        "extra": node.is_extra(),
        "missing": node.is_missing(),
        "startByte": node.start_byte(),
        "endByte": node.end_byte(),
      }),
    )?;
    let current = id;
    id += 1;
    if cursor.goto_first_child() {
      parents.push(current);
      continue;
    }
    while !cursor.goto_next_sibling() {
      if !cursor.goto_parent() {
        return writeln!(out, "]}}");
      }
      parents.pop();
    }
  }
}

pub fn run(argv: impl Iterator<Item = String>) -> Result<ExitCode, String> {
  let Some(args) = parse_args(argv)? else { return Ok(ExitCode::SUCCESS) };
  let from_stdin = args.file == Path::new("-");
  let mut registry = Registry::default();
  if let Some(config) = &args.config {
    // SAFETY: --config selects a trusted local file and its native grammars.
    unsafe { load_sgconfig(config, &mut registry) }?;
  }
  let language = match args.language {
    Some(language) => language,
    None if from_stdin => return Err("`--lang` is required for stdin".into()),
    None => registry
      .name_for_path(&args.file)
      .ok_or_else(|| format!("cannot infer a language for {}; pass `--lang`", args.file.display()))?
      .to_owned(),
  };
  let grammar = registry.grammar(&language).ok_or_else(|| format!("unknown language `{language}`"))?;
  let mut source = String::new();
  if from_stdin {
    io::stdin().lock().read_to_string(&mut source).map_err(|e| format!("cannot read stdin: {e}"))?;
  } else {
    source = std::fs::read_to_string(&args.file).map_err(|e| format!("cannot read {}: {e}", args.file.display()))?;
  }
  if source.len() > u32::MAX as usize {
    return Err("source exceeds the Tree-sitter byte offset limit".into());
  }
  let mut parser = Parser::new();
  // SAFETY: grammar stays alive until after the parser and tree are dropped.
  parser.set_language(unsafe { grammar.tree_sitter_language() }).map_err(|e| e.to_string())?;
  let tree = parser.parse(source.as_bytes(), None).ok_or("parse failed")?;
  let root = tree.root_node();
  let mut out = BufWriter::new(io::stdout().lock());
  let result = if args.json { write_json(&mut out, &grammar.name, root) } else { writeln!(out, "{}", root.to_sexp()) };
  match result.and_then(|()| out.flush()) {
    Ok(()) => Ok(ExitCode::SUCCESS),
    Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(ExitCode::SUCCESS),
    Err(e) => Err(e.to_string()),
  }
}
