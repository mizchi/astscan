mod cache;
mod output;
mod record;
mod scan;
mod simd;

use astscan_core::internals::{HTML, INJECTABLE, ScanOptions, Target, plan_rules};
use astscan_core::{ParserKind, Registry, load_sgconfig};
use cache::{Cache, ruleset_key};
use ignore::WalkState;
use ignore::types::TypesBuilder;
use output::Mode;
use scan::{Compiled, Shared, Worker};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const USAGE: &str = "\
astscan: run ast-grep rules over a source tree, extraction only

Usage: astscan scan --rule <FILE> [OPTIONS] [--] [PATHS]...

Options:
  -r, --rule <FILE>     Rule file: ast-grep rules as multi-document YAML
  -c, --config <FILE>   sgconfig.yml, read for `customLanguages`
      --json[=<STYLE>]  stream: ast-grep's JSON lines (default)
                        slim:   one line per match, no source text, all rule ids
  -j, --threads <N>     Worker threads (default: all cores)
      --parser <NAME>   tree-sitter (default), or fast: astscan's own driver over
                        the same grammars, falling back where it gives up
      --cache <DIR>     Keep results in DIR and reuse them for files that did not
                        change since a run with the same rules and output style
      --stats           Print file, byte and match counts to stderr
  -h, --help            Print this help
  -V, --version         Print version
";

struct Args {
  rule: PathBuf,
  config: Option<PathBuf>,
  mode: Mode,
  threads: usize,
  stats: bool,
  fast_parser: bool,
  cache: Option<PathBuf>,
  paths: Vec<PathBuf>,
}

fn parse_args() -> Result<Option<Args>, String> {
  let mut argv = std::env::args().skip(1).peekable();
  match argv.peek().map(String::as_str) {
    Some("scan") => {
      argv.next();
    }
    Some("-h" | "--help") | None => {
      print!("{USAGE}");
      return Ok(None);
    }
    Some("-V" | "--version") => {
      println!("astscan {}", env!("CARGO_PKG_VERSION"));
      return Ok(None);
    }
    Some(other) => return Err(format!("unknown command `{other}`; only `scan` is supported")),
  }
  let (mut rule, mut config, mut mode, mut threads, mut stats, mut paths) = (None, None, Mode::Stream, 0usize, false, Vec::new());
  let mut fast_parser = false;
  let mut cache = None;
  let mut only_paths = false;
  while let Some(arg) = argv.next() {
    if only_paths {
      paths.push(PathBuf::from(arg));
      continue;
    }
    let (flag, inline) = match arg.split_once('=') {
      Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
      // `-j8`: a short flag with its value attached.
      _ if arg.len() > 2 && arg.starts_with('-') && !arg.starts_with("--") => (arg[..2].to_string(), Some(arg[2..].to_string())),
      _ => (arg.clone(), None),
    };
    let mut value = |name: &str| inline.clone().or_else(|| argv.next()).ok_or_else(|| format!("`{name}` needs a value"));
    match flag.as_str() {
      "--" => only_paths = true,
      "-h" | "--help" => {
        print!("{USAGE}");
        return Ok(None);
      }
      "-r" | "--rule" => rule = Some(PathBuf::from(value("--rule")?)),
      "-c" | "--config" => config = Some(PathBuf::from(value("--config")?)),
      "-j" | "--threads" => threads = value("--threads")?.parse().map_err(|_| "`--threads` needs a number".to_string())?,
      "--stats" => stats = true,
      "--cache" => cache = Some(PathBuf::from(value("--cache")?)),
      "--parser" => {
        fast_parser = match value("--parser")?.as_str() {
          "fast" => true,
          "tree-sitter" => false,
          other => return Err(format!("unsupported `--parser={other}`; use `tree-sitter` or `fast`")),
        }
      }
      "--json" => {
        mode = match inline.as_deref() {
          None | Some("stream") => Mode::Stream,
          Some("slim") => Mode::Slim,
          Some(other) => return Err(format!("unsupported `--json={other}`; use `stream` or `slim`")),
        }
      }
      f if f.starts_with('-') && f.len() > 1 => return Err(format!("unknown option `{f}`")),
      _ => paths.push(PathBuf::from(arg)),
    }
  }
  if paths.is_empty() {
    paths.push(PathBuf::from("."));
  }
  let rule = rule.ok_or("`--rule <FILE>` is required")?;
  Ok(Some(Args { rule, config, mode, threads, stats, fast_parser, cache, paths }))
}

/// Tree-sitter requires its allocator never to return null for a non-zero size.
fn or_abort(p: *mut core::ffi::c_void, size: usize) -> *mut core::ffi::c_void {
  if p.is_null() && size != 0 {
    eprintln!("astscan: out of memory");
    std::process::abort();
  }
  p
}
unsafe extern "C" fn ts_malloc(size: usize) -> *mut core::ffi::c_void {
  or_abort(unsafe { libmimalloc_sys::mi_malloc(size) }, size)
}
unsafe extern "C" fn ts_calloc(count: usize, size: usize) -> *mut core::ffi::c_void {
  let Some(total) = count.checked_mul(size) else { std::process::abort() };
  or_abort(unsafe { libmimalloc_sys::mi_zalloc(total) }, total)
}
unsafe extern "C" fn ts_realloc(p: *mut core::ffi::c_void, size: usize) -> *mut core::ffi::c_void {
  or_abort(unsafe { libmimalloc_sys::mi_realloc(p, size) }, size)
}

const WORKER_STACK: usize = 1 << 30;
/// Patterns are parsed and walked recursively; the main thread's stack was the measure.
const COMPILER_STACK: usize = 16 << 20;

/// Everything but the scanned file that decides what a file's matches are:
/// this build, the rules, and the grammars loaded from outside. Not the
/// output style: the cache holds matches, and either style is written from them.
fn cache_key(args: &Args, registry: &Registry) -> Result<u128, String> {
  let read = |path: &PathBuf| std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()));
  // The size of the executable stands in for a build id between releases.
  let build = std::env::current_exe().and_then(std::fs::metadata).map_or(0, |m| m.len());
  let mut parts: Vec<Vec<u8>> = vec![
    env!("CARGO_PKG_VERSION").as_bytes().to_vec(),
    build.to_le_bytes().to_vec(),
    read(&args.rule)?,
    match &args.config {
      Some(config) => read(config)?,
      None => Vec::new(),
    },
  ];
  for library in &registry.custom_libraries {
    parts.push(read(library)?);
  }
  let parts: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
  Ok(ruleset_key(&parts))
}

/// `(size, path, what to scan it as)`.
type Found = (u64, PathBuf, Target);

/// One walker thread's finds, handed to the shared list in batches and on drop.
struct Batch<'a> {
  found: &'a Mutex<Vec<Found>>,
  local: Vec<Found>,
}

impl Batch<'_> {
  fn hand_over(&mut self) {
    self.found.lock().expect("no panic while holding the lock").append(&mut self.local);
  }
}

impl Drop for Batch<'_> {
  fn drop(&mut self) {
    self.hand_over();
  }
}

fn run() -> Result<ExitCode, String> {
  let Some(args) = parse_args()? else { return Ok(ExitCode::SUCCESS) };
  let started = std::time::Instant::now();
  // Tree-sitter allocates a node per token; give it the same allocator as the rest.
  // SAFETY: called before any other tree-sitter call, with one allocator family.
  unsafe {
    tree_sitter::set_allocator(Some(tree_sitter::Allocator {
      malloc: ts_malloc,
      calloc: ts_calloc,
      realloc: ts_realloc,
      free: libmimalloc_sys::mi_free,
    }));
  }
  let mut registry = Registry::default();
  if let Some(config) = &args.config {
    // SAFETY: --config selects a trusted local file and its native grammars.
    unsafe { load_sgconfig(config, &mut registry) }?;
  }
  let plan = plan_rules(&args.rule, &mut registry)?;
  let loaded = started.elapsed();
  if plan.languages().is_empty() {
    return Ok(ExitCode::SUCCESS);
  }
  // Compiling the rules is the slow half of loading and nothing below needs
  // its result until a file has to be scanned, so it runs beside the walk.
  // A run answered wholly from the cache only ever waits for it at the end.
  let langs = plan.languages().to_vec();
  let unit_for = |name: &str| langs.iter().position(|l| l.name == name);
  let compiled: &'static OnceLock<Result<Compiled, String>> = Box::leak(Box::new(OnceLock::new()));
  let compiling = std::thread::Builder::new().stack_size(COMPILER_STACK).spawn(move || {
    let result = plan.compile().map(|rules| {
      for warning in rules.warnings() {
        eprintln!("{warning}");
      }
      Compiled::new(rules)
    });
    let _ = compiled.set(result);
  });
  if let Err(e) = compiling {
    return Err(format!("cannot start the rule compiler thread: {e}"));
  }

  // Walk only files some rule's grammar claims.
  let mut types = TypesBuilder::new();
  for lang in &langs {
    for ext in &lang.exts {
      types.add("astscan", &format!("*.{ext}")).map_err(|e| e.to_string())?;
    }
  }
  // ast-grep scans the scripts and styles embedded in HTML with the rules of
  // their language, so HTML is walked whenever such a language has rules.
  let html_exts: Vec<String> = registry.grammar(HTML).map(|l| l.exts.clone()).unwrap_or_default();
  let scan_html =
    unit_for(HTML).is_some() || INJECTABLE.iter().any(|alias| Registry::builtin_name(alias).is_some_and(|name| unit_for(name).is_some()));
  if scan_html {
    for ext in &html_exts {
      types.add("astscan", &format!("*.{ext}")).map_err(|e| e.to_string())?;
    }
  }
  types.select("astscan");
  let threads = if args.threads == 0 { std::thread::available_parallelism().map_or(1, |n| n.get()) } else { args.threads };
  let mut walk = ignore::WalkBuilder::new(&args.paths[0]);
  for p in &args.paths[1..] {
    walk.add(p);
  }
  walk.threads(threads).types(types.build().map_err(|e| e.to_string())?);

  let cache = match &args.cache {
    Some(dir) => Some(Cache::open(dir, cache_key(&args, &registry)?)),
    None => None,
  };
  let shared = Shared {
    rules: compiled,
    mode: args.mode,
    cache: cache.as_ref(),
    error_matches: AtomicUsize::new(0),
    match_count: AtomicUsize::new(0),
    file_count: AtomicUsize::new(0),
    byte_count: AtomicUsize::new(0),
    options: ScanOptions {
      parser: if args.fast_parser { ParserKind::Fast } else { ParserKind::TreeSitter },
      check_flatten: std::env::var_os("ASTSCAN_CHECK_FLATTEN").is_some(),
      timed: args.stats,
    },
    fast_parsed: Default::default(),
    phase_nanos: Default::default(),
    slow: args.stats.then(Default::default),
  };
  // List first, scan second: with the whole list in hand the largest files
  // start first, so one big file does not begin when everything else is done.
  let registry = &registry;
  let shared_ref = &shared;
  let found: Mutex<Vec<Found>> = Mutex::new(Vec::new());
  walk.build_parallel().run(|| {
    let mut batch = Batch { found: &found, local: Vec::new() };
    Box::new(move |entry| {
      let Ok(entry) = entry else { return WalkState::Continue };
      if !entry.file_type().is_some_and(|t| t.is_file()) {
        return WalkState::Continue;
      }
      let path = entry.path();
      let target = match registry.name_for_path(path) {
        Some(HTML) => scan_html.then_some(Target::Html),
        Some(name) => unit_for(name).map(Target::Unit),
        None => None,
      };
      if let Some(target) = target {
        let size = entry.metadata().map_or(0, |m| m.len());
        batch.local.push((size, entry.into_path(), target));
        if batch.local.len() >= 256 {
          batch.hand_over();
        }
      }
      WalkState::Continue
    })
  });
  let mut files = found.into_inner().expect("walk is done");
  files.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
  let walked = started.elapsed();

  let next = AtomicUsize::new(0);
  let files_ref = &files;
  std::thread::scope(|scope| {
    for _ in 0..threads.min(files.len()).max(1) {
      // Flattening recurses once per level of nesting, and generated code
      // nests deeply. The stack is address space, committed only as touched.
      let spawned = std::thread::Builder::new().stack_size(WORKER_STACK).spawn_scoped(scope, || {
        let mut worker = Worker::new(shared_ref);
        loop {
          let i = next.fetch_add(1, Ordering::Relaxed);
          let Some((_, path, target)) = files_ref.get(i) else { break };
          worker.scan_file(path, *target);
        }
      });
      if let Err(e) = spawned {
        eprintln!("astscan: cannot start a worker thread: {e}");
        std::process::exit(2);
      }
    }
  });

  // A rule that does not compile is reported here, after the workers have
  // found nothing to do: no result of such a rule file can be in the cache.
  let rules = match compiled.wait() {
    Ok(rules) => rules,
    Err(e) => return Err(e.clone()),
  };
  if let Some(cache) = &cache {
    cache.finish();
  }
  if args.stats {
    eprintln!(
      "astscan: {} rules in {} groups, {} files, {} bytes, {} matches, load {:.1}ms, walk {:.1}ms, total {:.1}ms, {} threads",
      rules.rules.rule_count(),
      rules.group_count(),
      shared.file_count.load(Ordering::Relaxed),
      shared.byte_count.load(Ordering::Relaxed),
      shared.match_count.load(Ordering::Relaxed),
      loaded.as_secs_f64() * 1e3,
      (walked - loaded).as_secs_f64() * 1e3,
      started.elapsed().as_secs_f64() * 1e3,
      threads,
    );
    let ms = |i: usize| shared.phase_nanos[i].load(Ordering::Relaxed) as f64 / 1e6;
    eprintln!("astscan:   cpu: parse {:.1}ms, flatten {:.1}ms, match+write {:.1}ms", ms(0), ms(1), ms(2));
    if args.fast_parser {
      let (back, own) = (shared.fast_parsed[0].load(Ordering::Relaxed), shared.fast_parsed[1].load(Ordering::Relaxed));
      eprintln!("astscan:   fast parser: {own} files, {back} handed back to tree-sitter");
    }
    if let Some(cache) = &cache {
      eprintln!("astscan:   cache: {} hits, {} misses", cache.hits.load(Ordering::Relaxed), cache.misses.load(Ordering::Relaxed));
    }
    if let Some(slow) = &shared.slow {
      let mut slow = slow.lock().expect("workers are done");
      slow.sort_unstable_by_key(|e| std::cmp::Reverse(e.0));
      for (micros, path) in slow.iter().take(5) {
        eprintln!("astscan:   {:.1}ms {path}", *micros as f64 / 1e3);
      }
    }
  }
  // ast-grep exits 1 when a rule of severity `error` matched.
  Ok(if shared.error_matches.load(Ordering::Relaxed) > 0 { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

fn main() -> ExitCode {
  match run() {
    Ok(code) => code,
    Err(e) => {
      eprintln!("Error: {e}");
      ExitCode::from(2)
    }
  }
}
