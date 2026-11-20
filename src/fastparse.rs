//! Experimental: a parser driver of our own over a tree-sitter grammar.
//!
//! A compiled tree-sitter grammar is a `TSLanguage`: parse tables, a lex
//! function, optionally an external scanner. tree-sitter's runtime drives
//! them with reference-counted subtrees built for incremental re-parsing.
//! This drives the same tables over arenas that are dropped whole after each
//! file, and writes the result straight into a `FlatTree`.
//!
//! Where the tables offer several actions it does what tree-sitter does: the
//! stack forks into versions that share their common part, versions that
//! reach the same state merge again, and the same rules pick between two
//! trees for one span (dynamic precedence, then a structural comparison).
//! That part is a port of tree-sitter's `stack.c` and of the non-error half
//! of `parser.c`, because the tree has to come out the same.
//!
//! What it leaves out is error recovery. When every version is stuck, `parse`
//! returns `Err` and the caller parses the file with tree-sitter instead. So
//! a tree it does return is the tree tree-sitter would have built;
//! `examples/fastparse_probe.rs` checks that node for node.

use crate::tree::{F_EXTRA, F_NAMED, F_NAMED_LEAF, FlatTree, NONE};
use std::ffi::{c_char, c_void};

const SYM_END: u16 = 0;
const SYM_ERROR: u16 = 65535;
const SYM_ERROR_REPEAT: u16 = 65534;
const NO_LEX: u16 = u16::MAX;
const UNDEFINED: u32 = u32::MAX;
const SERIALIZATION_BUFFER: usize = 1024;
const BOM: i32 = 0xFEFF;

#[repr(C)]
pub struct TSLexer {
  lookahead: i32,
  result_symbol: u16,
  advance: unsafe extern "C" fn(*mut TSLexer, bool),
  mark_end: unsafe extern "C" fn(*mut TSLexer),
  get_column: unsafe extern "C" fn(*mut TSLexer) -> u32,
  is_at_included_range_start: unsafe extern "C" fn(*const TSLexer) -> bool,
  eof: unsafe extern "C" fn(*const TSLexer) -> bool,
  log: unsafe extern "C" fn(*const TSLexer, *const c_char, ...),
}

#[repr(C)]
struct FieldMapEntry {
  field_id: u16,
  child_index: u8,
  inherited: bool,
}

#[repr(C)]
struct MapSlice {
  index: u16,
  length: u16,
}

#[repr(C)]
struct SymbolMetadata {
  visible: bool,
  named: bool,
  supertype: bool,
}

#[repr(C)]
struct ExternalScanner {
  states: *const bool,
  symbol_map: *const u16,
  create: Option<unsafe extern "C" fn() -> *mut c_void>,
  destroy: Option<unsafe extern "C" fn(*mut c_void)>,
  scan: Option<unsafe extern "C" fn(*mut c_void, *mut TSLexer, *const bool) -> bool>,
  serialize: Option<unsafe extern "C" fn(*mut c_void, *mut c_char) -> u32>,
  deserialize: Option<unsafe extern "C" fn(*mut c_void, *const c_char, u32)>,
}

/// `struct TSLanguage` of tree-sitter's parser.h, ABI 14 and 15.
#[repr(C)]
struct RawLanguage {
  abi_version: u32,
  symbol_count: u32,
  alias_count: u32,
  token_count: u32,
  external_token_count: u32,
  state_count: u32,
  large_state_count: u32,
  production_id_count: u32,
  field_count: u32,
  max_alias_sequence_length: u16,
  parse_table: *const u16,
  small_parse_table: *const u16,
  small_parse_table_map: *const u32,
  parse_actions: *const [u8; 8],
  symbol_names: *const *const c_char,
  field_names: *const *const c_char,
  field_map_slices: *const MapSlice,
  field_map_entries: *const FieldMapEntry,
  symbol_metadata: *const SymbolMetadata,
  public_symbol_map: *const u16,
  alias_map: *const u16,
  alias_sequences: *const u16,
  lex_modes: *const u16,
  lex_fn: Option<unsafe extern "C" fn(*mut TSLexer, u16) -> bool>,
  keyword_lex_fn: Option<unsafe extern "C" fn(*mut TSLexer, u16) -> bool>,
  keyword_capture_token: u16,
  external_scanner: ExternalScanner,
  primary_state_ids: *const u16,
  // ABI 15 only from here on.
  name: *const c_char,
  reserved_words: *const u16,
  max_reserved_word_set_size: u16,
}

unsafe extern "C" {
  fn astscan_lexer_log(lexer: *const TSLexer, format: *const c_char, ...);
}

/// The lexer the grammar's lex function and scanner are handed: the
/// `TSLexer` they see, followed by our cursor over one contiguous buffer.
#[repr(C)]
struct Lexer {
  data: TSLexer,
  src: *const u8,
  len: u32,
  pos: u32,
  /// Bytes of the current lookahead; 0 when it has not been read yet.
  lookahead_size: u32,
  token_start: u32,
  token_end: u32,
  at_eof: bool,
}

impl Lexer {
  #[inline]
  unsafe fn read_lookahead(&mut self) {
    if self.pos >= self.len {
      self.at_eof = true;
      self.data.lookahead = 0;
      self.lookahead_size = 1;
      return;
    }
    self.at_eof = false;
    // SAFETY: `pos < len`, and the source is valid UTF-8, so a lead byte is
    // followed by its continuation bytes inside the buffer.
    unsafe {
      let p = self.src.add(self.pos as usize);
      let b0 = *p;
      if b0 < 0x80 {
        self.data.lookahead = b0 as i32;
        self.lookahead_size = 1;
      } else if b0 < 0xE0 {
        self.data.lookahead = ((b0 as i32 & 0x1F) << 6) | (*p.add(1) as i32 & 0x3F);
        self.lookahead_size = 2;
      } else if b0 < 0xF0 {
        self.data.lookahead = ((b0 as i32 & 0x0F) << 12) | ((*p.add(1) as i32 & 0x3F) << 6) | (*p.add(2) as i32 & 0x3F);
        self.lookahead_size = 3;
      } else {
        self.data.lookahead =
          ((b0 as i32 & 0x07) << 18) | ((*p.add(1) as i32 & 0x3F) << 12) | ((*p.add(2) as i32 & 0x3F) << 6) | (*p.add(3) as i32 & 0x3F);
        self.lookahead_size = 4;
      }
    }
  }

  unsafe fn goto(&mut self, pos: u32) {
    self.pos = pos;
    unsafe { self.read_lookahead() };
  }

  /// `ts_lexer_start`: a token begins here.
  unsafe fn start(&mut self) {
    self.token_start = self.pos;
    self.token_end = UNDEFINED;
    self.data.result_symbol = 0;
    if !self.at_eof && self.pos == 0 && self.data.lookahead == BOM {
      unsafe { lexer_advance(&mut self.data, true) };
    }
  }

  /// `ts_lexer_finish`.
  fn finish(&mut self) {
    if self.token_end == UNDEFINED {
      self.token_end = self.pos;
    }
    if self.token_end < self.token_start {
      self.token_start = self.token_end;
    }
  }
}

unsafe extern "C" fn lexer_advance(lexer: *mut TSLexer, skip: bool) {
  // SAFETY: every `TSLexer` handed to a grammar is the head of a `Lexer`.
  let l = unsafe { &mut *(lexer as *mut Lexer) };
  if l.at_eof {
    return;
  }
  l.pos += l.lookahead_size;
  if skip {
    l.token_start = l.pos;
  }
  unsafe { l.read_lookahead() };
}

unsafe extern "C" fn lexer_mark_end(lexer: *mut TSLexer) {
  let l = unsafe { &mut *(lexer as *mut Lexer) };
  l.token_end = l.pos;
}

unsafe extern "C" fn lexer_get_column(lexer: *mut TSLexer) -> u32 {
  let l = unsafe { &*(lexer as *const Lexer) };
  // SAFETY: `pos <= len`.
  let before = unsafe { std::slice::from_raw_parts(l.src, l.pos as usize) };
  let line_start = memchr::memrchr(b'\n', before).map_or(0, |i| i + 1);
  let mut column = crate::simd::count_chars(&before[line_start..]) as u32;
  // A byte order mark at the head of the file takes no column.
  if line_start == 0 && before.starts_with(&[0xEF, 0xBB, 0xBF]) {
    column -= 1;
  }
  column
}

unsafe extern "C" fn lexer_is_at_included_range_start(lexer: *const TSLexer) -> bool {
  let l = unsafe { &*(lexer as *const Lexer) };
  !l.at_eof && l.pos == 0
}

unsafe extern "C" fn lexer_eof(lexer: *const TSLexer) -> bool {
  unsafe { (*(lexer as *const Lexer)).at_eof }
}

/// The table cell for `(state, symbol)` as the grammar stores it: a dense row
/// for the first states, groups of symbols sharing a value for the rest.
unsafe fn sparse_lookup(l: &RawLanguage, state: u16, symbol: u16) -> u16 {
  unsafe {
    if (state as u32) < l.large_state_count {
      *l.parse_table.add(state as usize * l.symbol_count as usize + symbol as usize)
    } else {
      let index = *l.small_parse_table_map.add((state as u32 - l.large_state_count) as usize);
      let mut data = l.small_parse_table.add(index as usize);
      let group_count = *data;
      data = data.add(1);
      for _ in 0..group_count {
        let value = *data;
        let count = *data.add(1) as usize;
        data = data.add(2);
        for j in 0..count {
          if *data.add(j) == symbol {
            return value;
          }
        }
        data = data.add(count);
      }
      0
    }
  }
}

/// Above this many cells a grammar keeps its own compressed table.
const DENSE_TABLE_LIMIT: usize = 1 << 25;

/// The whole parse table of a grammar as one array, a row per state. Most
/// states of a grammar are stored compressed, and searching a row's groups
/// for a symbol was a quarter of the parse. Built once per grammar and shared
/// by every thread; `None` for a grammar too large to be worth it.
fn dense_table(lang: *const RawLanguage) -> Option<std::sync::Arc<[u16]>> {
  /// By grammar address.
  type Built = (usize, Option<std::sync::Arc<[u16]>>);
  static TABLES: std::sync::Mutex<Vec<Built>> = std::sync::Mutex::new(Vec::new());
  let mut tables = TABLES.lock().unwrap_or_else(|e| e.into_inner());
  if let Some((_, table)) = tables.iter().find(|(key, _)| *key == lang as usize) {
    return table.clone();
  }
  // SAFETY: a grammar is static data, and its tables are as long as its counts say.
  let l = unsafe { &*lang };
  let (states, symbols) = (l.state_count as usize, l.symbol_count as usize);
  let table = (states * symbols <= DENSE_TABLE_LIMIT).then(|| {
    let mut cells = vec![0u16; states * symbols];
    unsafe {
      let large = (l.large_state_count as usize).min(states);
      cells[..large * symbols].copy_from_slice(std::slice::from_raw_parts(l.parse_table, large * symbols));
      for state in large..states {
        let row = &mut cells[state * symbols..(state + 1) * symbols];
        let mut data = l.small_parse_table.add(*l.small_parse_table_map.add(state - large) as usize);
        let group_count = *data;
        data = data.add(1);
        for _ in 0..group_count {
          let value = *data;
          let count = *data.add(1) as usize;
          data = data.add(2);
          for j in 0..count {
            let symbol = *data.add(j) as usize;
            // A symbol sits in one group; were it in two, the search takes the first.
            if symbol < symbols && row[symbol] == 0 {
              row[symbol] = value;
            }
          }
          data = data.add(count);
        }
      }
    }
    std::sync::Arc::<[u16]>::from(cells)
  });
  tables.push((lang as usize, table.clone()));
  table
}

const F_NODE_EXTRA: u8 = 1;
const F_NODE_KEYWORD: u8 = 2;
const F_NODE_EXTERNAL: u8 = 4;

/// A subtree: a token, or a reduction over children.
#[derive(Clone, Copy)]
struct Node {
  symbol: u16,
  production: u16,
  padding: u32,
  size: u32,
  /// Into `Parser::children`.
  first_child: u32,
  child_count: u32,
  dynamic_precedence: i32,
  /// For a token: the parse state it was lexed in.
  lex_state: u16,
  flags: u8,
  /// For a token from the external scanner: the scanner's state after it.
  scanner_state: u32,
}

impl Node {
  #[inline]
  fn extra(&self) -> bool {
    self.flags & F_NODE_EXTRA != 0
  }
}

/// An edge of the graph-structured stack: the node below, and the subtree
/// that was pushed to get from there to here.
#[derive(Clone, Copy)]
struct Link {
  node: u32,
  subtree: u32,
}

const MAX_LINK_COUNT: usize = 8;
const MAX_ITERATOR_COUNT: usize = 64;
const MAX_VERSION_COUNT: usize = 6;
const MAX_VERSION_COUNT_OVERFLOW: usize = 4;

/// A node of the graph-structured stack. Almost every node has one link, so
/// the first is inline and the rest, when a merge adds them, live in `more`.
#[derive(Clone, Copy)]
struct StackNode {
  state: u16,
  link_count: u8,
  position: u32,
  dynamic_precedence: i32,
  first: Link,
  /// Into `Parser::more_links`, or `NONE`.
  more: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
  Active,
  /// No action for the lookahead: dead unless every other version is too.
  Paused,
  Halted,
}

/// One version of the stack: its top node and the scanner state it carries.
#[derive(Clone, Copy)]
struct Head {
  node: u32,
  scanner_state: u32,
  status: Status,
}

/// What one path of a pop took off the stack, bottom first: a range of
/// `Parser::popped`, and the version left standing where the path ended.
#[derive(Clone, Copy)]
struct Slice {
  start: u32,
  len: u32,
  version: usize,
}

impl Slice {
  #[inline]
  fn range(&self) -> std::ops::Range<usize> {
    self.start as usize..(self.start + self.len) as usize
  }
}

/// One path of a pop on its way down. The subtrees it has passed are a
/// linked list in `Parser::trail`, newest first, so a path that forks at a
/// merged node shares everything above the fork with its sibling.
#[derive(Clone, Copy)]
struct StackIterator {
  node: u32,
  /// Into `Parser::trail`, or `NONE`.
  trail: u32,
  subtree_count: u32,
}

#[derive(Clone, Copy)]
struct Entry {
  actions: *const [u8; 8],
  count: u8,
  reusable: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Preference {
  TakeLeft,
  PreferLeft,
  None,
  PreferRight,
  TakeRight,
}

/// Why a parse was handed back to tree-sitter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bail {
  /// Every stack version is stuck: the source needs error recovery.
  Error,
  /// A grammar this driver does not read (ABI older than 14).
  Unsupported,
}

pub struct Parser {
  lang: *const RawLanguage,
  /// The grammar's parse table uncompressed, or null. Points into `_dense`.
  dense: *const u16,
  _dense: Option<std::sync::Arc<[u16]>>,
  symbol_count: usize,
  /// Keeps the grammar (and a dylib it may live in) alive.
  _owner: tree_sitter::Language,
  lexer: Box<Lexer>,
  scanner: *mut c_void,
  /// Serialized scanner states by id; id 0 is the empty state.
  scanner_states: Vec<Vec<u8>>,
  scanner_state_ids: std::collections::HashMap<Vec<u8>, u32>,
  serialized: Box<[u8; SERIALIZATION_BUFFER]>,
  nodes: Vec<Node>,
  children: Vec<u32>,
  stack_nodes: Vec<StackNode>,
  more_links: Vec<[Link; MAX_LINK_COUNT - 1]>,
  heads: Vec<Head>,
  iterators: Vec<StackIterator>,
  /// `(subtree, the entry before it)`: see `StackIterator`.
  trail: Vec<(u32, u32)>,
  slices: Vec<Slice>,
  /// The subtrees of every slice of the last pop.
  popped: Vec<u32>,
  /// The token last lexed, reusable by another version at the same place.
  cached_token: u32,
  cached_position: u32,
  cached_scanner_state: u32,
  finished: u32,
  accept_count: usize,
  scratch: Vec<u32>,
  compare_stack: Vec<(u32, u32)>,
  walks: Vec<Walk>,
  owners: Vec<Owner>,
}

impl Drop for Parser {
  fn drop(&mut self) {
    // SAFETY: `scanner` came from this grammar's `create`.
    unsafe {
      if let Some(destroy) = (*self.lang).external_scanner.destroy
        && !self.scanner.is_null()
      {
        destroy(self.scanner);
      }
    }
  }
}

impl Parser {
  pub fn new(language: &tree_sitter::Language) -> Result<Self, Bail> {
    let owner = language.clone();
    let lang = owner.clone().into_raw() as *const RawLanguage;
    // SAFETY: a `tree_sitter::Language` wraps a pointer to a static `TSLanguage`.
    let raw = unsafe { &*lang };
    if raw.abi_version < 14 || raw.lex_fn.is_none() {
      return Err(Bail::Unsupported);
    }
    let scanner = match raw.external_scanner.create {
      // SAFETY: the grammar's own constructor.
      Some(create) => unsafe { create() },
      None => std::ptr::null_mut(),
    };
    let lexer = Box::new(Lexer {
      data: TSLexer {
        lookahead: 0,
        result_symbol: 0,
        advance: lexer_advance,
        mark_end: lexer_mark_end,
        get_column: lexer_get_column,
        is_at_included_range_start: lexer_is_at_included_range_start,
        eof: lexer_eof,
        log: astscan_lexer_log,
      },
      src: std::ptr::null(),
      len: 0,
      pos: 0,
      lookahead_size: 0,
      token_start: 0,
      token_end: 0,
      at_eof: false,
    });
    let dense_owner = dense_table(lang);
    Ok(Parser {
      lang,
      dense: dense_owner.as_ref().map_or(std::ptr::null(), |t| t.as_ptr()),
      _dense: dense_owner,
      symbol_count: raw.symbol_count as usize,
      _owner: owner,
      lexer,
      scanner,
      scanner_states: vec![Vec::new()],
      scanner_state_ids: Default::default(),
      serialized: Box::new([0; SERIALIZATION_BUFFER]),
      nodes: Vec::new(),
      children: Vec::new(),
      stack_nodes: Vec::new(),
      more_links: Vec::new(),
      heads: Vec::new(),
      iterators: Vec::new(),
      trail: Vec::new(),
      slices: Vec::new(),
      popped: Vec::new(),
      cached_token: NONE,
      cached_position: 0,
      cached_scanner_state: 0,
      finished: NONE,
      accept_count: 0,
      scratch: Vec::new(),
      compare_stack: Vec::new(),
      walks: Vec::new(),
      owners: Vec::new(),
    })
  }

  #[inline]
  fn lang(&self) -> &RawLanguage {
    // SAFETY: see `new`.
    unsafe { &*self.lang }
  }

  // ------------------------------------------------------------- the tables

  /// `ts_language_lookup`: the table cell for `(state, symbol)`.
  #[inline]
  unsafe fn lookup(&self, state: u16, symbol: u16) -> u16 {
    // SAFETY: `dense` has a row per state and a column per symbol, or is null.
    unsafe {
      if self.dense.is_null() {
        sparse_lookup(self.lang(), state, symbol)
      } else {
        *self.dense.add(state as usize * self.symbol_count + symbol as usize)
      }
    }
  }

  /// `ts_language_table_entry`.
  #[inline]
  unsafe fn entry(&self, state: u16, symbol: u16) -> Entry {
    if symbol == SYM_ERROR || symbol == SYM_ERROR_REPEAT {
      return Entry { actions: std::ptr::null(), count: 0, reusable: false };
    }
    unsafe {
      let e = self.lang().parse_actions.add(self.lookup(state, symbol) as usize);
      Entry { actions: e.add(1), count: (*e)[0], reusable: (*e)[1] != 0 }
    }
  }

  /// `ts_language_next_state`.
  unsafe fn next_state(&self, state: u16, symbol: u16) -> u16 {
    if symbol == SYM_ERROR || symbol == SYM_ERROR_REPEAT {
      return 0;
    }
    unsafe {
      if (symbol as u32) < self.lang().token_count {
        let e = self.entry(state, symbol);
        if e.count > 0 {
          let a = &*e.actions.add(e.count as usize - 1);
          if a[0] == 0 {
            return if a[4] != 0 { state } else { u16::from_ne_bytes([a[2], a[3]]) };
          }
        }
        0
      } else {
        self.lookup(state, symbol)
      }
    }
  }

  /// `(lex_state, external_lex_state, reserved_word_set_id)` of a parse state.
  #[inline]
  unsafe fn lex_mode(&self, state: u16) -> (u16, u16, u16) {
    let l = self.lang();
    unsafe {
      if l.abi_version < 15 {
        let m = l.lex_modes.add(state as usize * 2);
        (*m, *m.add(1), 0)
      } else {
        let m = l.lex_modes.add(state as usize * 3);
        (*m, *m.add(1), *m.add(2))
      }
    }
  }

  unsafe fn is_reserved_word(&self, state: u16, symbol: u16) -> bool {
    let l = self.lang();
    if l.abi_version < 15 || l.reserved_words.is_null() || l.max_reserved_word_set_size == 0 {
      return false;
    }
    unsafe {
      let (_, _, set) = self.lex_mode(state);
      let base = l.reserved_words.add(set as usize * l.max_reserved_word_set_size as usize);
      for i in 0..l.max_reserved_word_set_size as usize {
        let word = *base.add(i);
        if word == symbol {
          return true;
        }
        if word == 0 {
          break;
        }
      }
    }
    false
  }

  // ------------------------------------------------------------------ lexing

  fn intern_scanner_state(&mut self, len: usize, current: u32) -> u32 {
    let bytes = &self.serialized[..len];
    if self.scanner_states[current as usize] == bytes {
      return current;
    }
    if let Some(&id) = self.scanner_state_ids.get(bytes) {
      return id;
    }
    let id = self.scanner_states.len() as u32;
    self.scanner_states.push(bytes.to_vec());
    self.scanner_state_ids.insert(bytes.to_vec(), id);
    id
  }

  /// `ts_parser__lex`: the next token for a version in `state`, as a leaf.
  /// `None` after a non-terminal extra, where there is no lookahead. A place
  /// where no token is valid yields an error leaf, which no state accepts.
  unsafe fn lex(&mut self, version: usize, state: u16) -> Option<u32> {
    let (lex_state, external_lex_state, _) = unsafe { self.lex_mode(state) };
    if lex_state == NO_LEX {
      return None;
    }
    let position = self.stack_nodes[self.heads[version].node as usize].position;
    let head_scanner_state = self.heads[version].scanner_state;
    let l = self.lang;
    let lexer: *mut Lexer = &mut *self.lexer;
    let mut found_external = false;
    let mut scanner_state = 0;
    unsafe {
      if (*lexer).pos != position || (*lexer).lookahead_size == 0 {
        (*lexer).goto(position);
      }
      if external_lex_state != 0
        && let Some(scan) = (*l).external_scanner.scan
      {
        (*lexer).start();
        if let Some(deserialize) = (*l).external_scanner.deserialize {
          let saved = &self.scanner_states[head_scanner_state as usize];
          deserialize(self.scanner, saved.as_ptr() as *const c_char, saved.len() as u32);
        }
        let valid = (*l).external_scanner.states.add(external_lex_state as usize * (*l).external_token_count as usize);
        let mut found = scan(self.scanner, &mut (*lexer).data, valid);
        (*lexer).finish();
        if found {
          let len = match (*l).external_scanner.serialize {
            Some(serialize) => serialize(self.scanner, self.serialized.as_mut_ptr() as *mut c_char) as usize,
            None => 0,
          };
          scanner_state = self.intern_scanner_state(len, head_scanner_state);
          // An empty token that changes nothing and is an extra would loop forever.
          if (*lexer).token_end <= position && scanner_state == head_scanner_state {
            let symbol = *(*l).external_scanner.symbol_map.add((*lexer).data.result_symbol as usize);
            if self.next_state(state, symbol) == state {
              found = false;
            }
          }
          found_external = found;
        }
        if !found_external {
          (*lexer).goto(position);
        }
      }
      let mut lexed = true;
      if !found_external {
        (*lexer).start();
        let lex_fn = (*l).lex_fn.expect("checked in `new`");
        lexed = lex_fn(&mut (*lexer).data, lex_state);
        (*lexer).finish();
      }

      let mut symbol = (*lexer).data.result_symbol;
      let (token_start, token_end) = ((*lexer).token_start, (*lexer).token_end);
      let mut flags = 0;
      if !lexed {
        symbol = SYM_ERROR;
      } else if found_external {
        symbol = *(*l).external_scanner.symbol_map.add(symbol as usize);
        flags |= F_NODE_EXTERNAL;
      } else if symbol == (*l).keyword_capture_token
        && symbol != 0
        && let Some(keyword_lex) = (*l).keyword_lex_fn
      {
        (*lexer).goto(token_start);
        (*lexer).start();
        let is_keyword = keyword_lex(&mut (*lexer).data, 0);
        (*lexer).finish();
        let keyword = (*lexer).data.result_symbol;
        if is_keyword {
          flags |= F_NODE_KEYWORD;
          if (*lexer).token_end == token_end && (self.entry(state, keyword).count > 0 || self.is_reserved_word(state, keyword)) {
            symbol = keyword;
          }
        }
      }
      // tree-sitter creates the end-of-file leaf as an extra.
      if symbol == SYM_END {
        flags |= F_NODE_EXTRA;
      }
      let id = self.nodes.len() as u32;
      self.nodes.push(Node {
        symbol,
        production: 0,
        padding: token_start.saturating_sub(position),
        size: token_end.saturating_sub(token_start),
        first_child: 0,
        child_count: 0,
        dynamic_precedence: 0,
        lex_state: state,
        flags,
        scanner_state: if found_external { scanner_state } else { 0 },
      });
      Some(id)
    }
  }

  /// `ts_parser__can_reuse_first_leaf`: may a token lexed in one state stand
  /// as the lookahead in another?
  unsafe fn can_reuse_token(&self, state: u16, token: u32, entry: &Entry) -> bool {
    let t = &self.nodes[token as usize];
    unsafe {
      let current = self.lex_mode(state);
      let leaf = self.lex_mode(t.lex_state);
      if current.0 == NO_LEX {
        return false;
      }
      let word = self.lang().keyword_capture_token;
      if entry.count > 0 && leaf == current && (t.symbol != word || (t.flags & F_NODE_KEYWORD == 0 && t.lex_state == state)) {
        return true;
      }
      if t.size == 0 && t.symbol != SYM_END {
        return false;
      }
      current.1 == 0 && entry.reusable
    }
  }

  // ------------------------------------------------------------ the subtrees

  #[inline]
  fn total(&self, node: u32) -> u32 {
    let n = &self.nodes[node as usize];
    n.padding + n.size
  }

  /// What a subtree adds to the precedence of a stack: nothing for a leaf.
  #[inline]
  fn subtree_precedence(&self, node: u32) -> i32 {
    let n = &self.nodes[node as usize];
    if n.child_count == 0 { 0 } else { n.dynamic_precedence }
  }

  /// Build a parent over `children`, as `ts_subtree_new_node` sizes it.
  fn new_parent(&mut self, symbol: u16, production: u16, children: &[u32]) -> u32 {
    let first_child = self.children.len() as u32;
    self.children.extend_from_slice(children);
    let (mut padding, mut size, mut precedence) = (0, 0, 0);
    for (i, &c) in children.iter().enumerate() {
      let n = &self.nodes[c as usize];
      if i == 0 {
        padding = n.padding;
        size = n.size;
      } else {
        size += n.padding + n.size;
      }
      if n.child_count != 0 {
        precedence += n.dynamic_precedence;
      }
    }
    let id = self.nodes.len() as u32;
    self.nodes.push(Node {
      symbol,
      production,
      padding,
      size,
      first_child,
      child_count: children.len() as u32,
      dynamic_precedence: precedence,
      lex_state: 0,
      flags: 0,
      scanner_state: 0,
    });
    id
  }

  /// `ts_subtree_compare`: an arbitrary but fixed order on tree shapes.
  fn compare(&mut self, left: u32, right: u32) -> i32 {
    self.compare_stack.clear();
    self.compare_stack.push((left, right));
    while let Some((l, r)) = self.compare_stack.pop() {
      let (a, b) = (self.nodes[l as usize], self.nodes[r as usize]);
      let result = if a.symbol < b.symbol {
        -1
      } else if b.symbol < a.symbol {
        1
      } else if a.child_count < b.child_count {
        -1
      } else if b.child_count < a.child_count {
        1
      } else {
        0
      };
      if result != 0 {
        return result;
      }
      for i in (0..a.child_count as usize).rev() {
        self.compare_stack.push((self.children[a.first_child as usize + i], self.children[b.first_child as usize + i]));
      }
    }
    0
  }

  /// `ts_parser__select_tree`: should `right` replace `left`?
  fn select_tree(&mut self, left: u32, right: u32) -> bool {
    if left == NONE {
      return true;
    }
    if right == NONE {
      return false;
    }
    let (lp, rp) = (self.subtree_precedence(left), self.subtree_precedence(right));
    if rp > lp {
      return true;
    }
    if lp > rp {
      return false;
    }
    self.compare(left, right) == 1
  }

  // --------------------------------------------------------------- the stack

  #[inline]
  fn link(&self, node: u32, i: usize) -> Link {
    let n = &self.stack_nodes[node as usize];
    if i == 0 { n.first } else { self.more_links[n.more as usize][i - 1] }
  }

  fn set_link_subtree(&mut self, node: u32, i: usize, subtree: u32) {
    if i == 0 {
      self.stack_nodes[node as usize].first.subtree = subtree;
    } else {
      let more = self.stack_nodes[node as usize].more;
      self.more_links[more as usize][i - 1].subtree = subtree;
    }
  }

  /// `ts_stack_push`.
  fn push(&mut self, version: usize, subtree: u32, state: u16) {
    let previous = self.heads[version].node;
    let p = &self.stack_nodes[previous as usize];
    let node = StackNode {
      state,
      link_count: 1,
      position: p.position + self.total(subtree),
      dynamic_precedence: p.dynamic_precedence + self.subtree_precedence(subtree),
      first: Link { node: previous, subtree },
      more: NONE,
    };
    self.heads[version].node = self.stack_nodes.len() as u32;
    self.stack_nodes.push(node);
  }

  /// `stack__subtree_is_equivalent`, with no errors in play.
  fn subtrees_equivalent(&self, left: u32, right: u32) -> bool {
    if left == right {
      return true;
    }
    let (a, b) = (&self.nodes[left as usize], &self.nodes[right as usize]);
    a.symbol == b.symbol
      && a.padding == b.padding
      && a.size == b.size
      && a.child_count == b.child_count
      && a.extra() == b.extra()
      && a.scanner_state == b.scanner_state
  }

  /// `stack_node_add_link`: give `target` another way down, folding it into
  /// an existing link when the two reach equivalent places.
  fn add_link(&mut self, target: u32, link: Link) {
    if link.node == target {
      return;
    }
    let count = self.stack_nodes[target as usize].link_count as usize;
    for i in 0..count {
      let existing = self.link(target, i);
      if !self.subtrees_equivalent(existing.subtree, link.subtree) {
        continue;
      }
      if existing.node == link.node {
        if self.subtree_precedence(link.subtree) > self.subtree_precedence(existing.subtree) {
          self.set_link_subtree(target, i, link.subtree);
          self.stack_nodes[target as usize].dynamic_precedence =
            self.stack_nodes[link.node as usize].dynamic_precedence + self.subtree_precedence(link.subtree);
        }
        return;
      }
      let (e, n) = (self.stack_nodes[existing.node as usize], self.stack_nodes[link.node as usize]);
      if e.state == n.state && e.position == n.position {
        for j in 0..n.link_count as usize {
          let below = self.link(link.node, j);
          self.add_link(existing.node, below);
        }
        let precedence = n.dynamic_precedence + self.subtree_precedence(link.subtree);
        if precedence > self.stack_nodes[target as usize].dynamic_precedence {
          self.stack_nodes[target as usize].dynamic_precedence = precedence;
        }
        return;
      }
    }
    if count == MAX_LINK_COUNT {
      return;
    }
    if self.stack_nodes[target as usize].more == NONE {
      self.stack_nodes[target as usize].more = self.more_links.len() as u32;
      self.more_links.push([Link { node: NONE, subtree: NONE }; MAX_LINK_COUNT - 1]);
    }
    let more = self.stack_nodes[target as usize].more;
    self.more_links[more as usize][count - 1] = link;
    self.stack_nodes[target as usize].link_count += 1;
    let precedence = self.stack_nodes[link.node as usize].dynamic_precedence + self.subtree_precedence(link.subtree);
    if precedence > self.stack_nodes[target as usize].dynamic_precedence {
      self.stack_nodes[target as usize].dynamic_precedence = precedence;
    }
  }

  /// `ts_stack_can_merge`.
  fn can_merge(&self, v1: usize, v2: usize) -> bool {
    let (h1, h2) = (&self.heads[v1], &self.heads[v2]);
    let (n1, n2) = (&self.stack_nodes[h1.node as usize], &self.stack_nodes[h2.node as usize]);
    h1.status == Status::Active
      && h2.status == Status::Active
      && n1.state == n2.state
      && n1.position == n2.position
      && h1.scanner_state == h2.scanner_state
  }

  /// `ts_stack_merge`: fold version `v2` into `v1` if they are at the same place.
  fn merge(&mut self, v1: usize, v2: usize) -> bool {
    if !self.can_merge(v1, v2) {
      return false;
    }
    let (target, source) = (self.heads[v1].node, self.heads[v2].node);
    for i in 0..self.stack_nodes[source as usize].link_count as usize {
      let link = self.link(source, i);
      self.add_link(target, link);
    }
    self.heads.remove(v2);
    true
  }

  /// `ts_stack_renumber_version`: version `v1` takes the place of `v2`.
  fn renumber(&mut self, v1: usize, v2: usize) {
    if v1 == v2 {
      return;
    }
    self.heads[v2] = self.heads[v1];
    self.heads.remove(v1);
  }

  /// `ts_stack__add_slice`.
  fn add_slice(&mut self, slices: &mut Vec<Slice>, original_version: usize, node: u32, start: u32, len: u32) {
    for i in (0..slices.len()).rev() {
      let version = slices[i].version;
      if self.heads[version].node == node {
        slices.insert(i + 1, Slice { start, len, version });
        return;
      }
    }
    let original = self.heads[original_version];
    self.heads.push(Head { node, scanner_state: original.scanner_state, status: Status::Active });
    slices.push(Slice { start, len, version: self.heads.len() - 1 });
  }

  /// `stack__iter`: walk down from a version's top along every link, and
  /// hand back what each path popped, as ranges of `self.popped`. `goal` is
  /// the number of non-extra subtrees to pop; `None` pops to the bottom.
  fn pop(&mut self, version: usize, goal: Option<u32>, slices: &mut Vec<Slice>) {
    slices.clear();
    self.popped.clear();
    self.trail.clear();
    let mut iterators = std::mem::take(&mut self.iterators);
    iterators.clear();
    iterators.push(StackIterator { node: self.heads[version].node, trail: NONE, subtree_count: 0 });
    while !iterators.is_empty() {
      let mut i = 0;
      let mut size = iterators.len();
      while i < size {
        let node = iterators[i].node;
        let link_count = self.stack_nodes[node as usize].link_count as usize;
        let (should_pop, goal_reached) = match goal {
          Some(n) => (iterators[i].subtree_count == n, iterators[i].subtree_count == n),
          None => (link_count == 0, false),
        };
        let should_stop = goal_reached || link_count == 0;
        if should_pop {
          // The trail runs from the deepest subtree up, which is child order.
          let start = self.popped.len() as u32;
          let mut at = iterators[i].trail;
          while at != NONE {
            let (subtree, before) = self.trail[at as usize];
            self.popped.push(subtree);
            at = before;
          }
          self.add_slice(slices, version, node, start, self.popped.len() as u32 - start);
        }
        if should_stop {
          iterators.remove(i);
          size -= 1;
          continue;
        }
        for j in 1..=link_count {
          let (link, at) = if j == link_count {
            (self.link(node, 0), i)
          } else {
            if iterators.len() >= MAX_ITERATOR_COUNT {
              continue;
            }
            let copy = iterators[i];
            iterators.push(copy);
            (self.link(node, j), iterators.len() - 1)
          };
          let it = &mut iterators[at];
          it.node = link.node;
          self.trail.push((link.subtree, it.trail));
          it.trail = self.trail.len() as u32 - 1;
          if !self.nodes[link.subtree as usize].extra() {
            it.subtree_count += 1;
          }
        }
        i += 1;
      }
    }
    self.iterators = iterators;
  }

  /// Where the children of a popped range end and its trailing extras begin.
  /// Extras after the last real child are not its children: they go back on top.
  fn children_end(&self, popped: &[u32], range: std::ops::Range<usize>) -> usize {
    let mut end = range.end;
    while end > range.start && self.nodes[popped[end - 1] as usize].extra() {
      end -= 1;
    }
    end
  }

  // -------------------------------------------------------------- the parser

  /// `ts_parser__reduce`. Returns the version a reduction created, if one
  /// outlived the merges.
  unsafe fn reduce(
    &mut self,
    version: usize,
    symbol: u16,
    count: u8,
    precedence: i32,
    production: u16,
    end_of_non_terminal_extra: bool,
  ) -> Option<usize> {
    let initial_version_count = self.heads.len();
    let mut slices = std::mem::take(&mut self.slices);
    self.pop(version, Some(count as u32), &mut slices);
    let popped = std::mem::take(&mut self.popped);
    let halted = self.heads.iter().filter(|h| h.status == Status::Halted).count();
    let mut removed = 0;
    let mut i = 0;
    while i < slices.len() {
      let first_version = slices[i].version;
      let slice_version = first_version - removed;
      if slice_version > MAX_VERSION_COUNT + MAX_VERSION_COUNT_OVERFLOW + halted {
        self.heads.remove(slice_version);
        removed += 1;
        while i + 1 < slices.len() && slices[i + 1].version == first_version {
          i += 1;
        }
        i += 1;
        continue;
      }

      let range = slices[i].range();
      let end = self.children_end(&popped, range.clone());
      let mut trailing = end..range.end;
      let mut parent = self.new_parent(symbol, production, &popped[range.start..end]);

      // Several paths popped into the same version: keep the better children.
      while i + 1 < slices.len() && slices[i + 1].version == first_version {
        i += 1;
        let range = slices[i].range();
        let end = self.children_end(&popped, range.clone());
        let candidate = self.new_parent(symbol, production, &popped[range.start..end]);
        if self.select_tree(parent, candidate) {
          trailing = end..range.end;
          parent = candidate;
        }
      }

      let state = self.stack_nodes[self.heads[slice_version].node as usize].state;
      let next = unsafe { self.next_state(state, symbol) };
      if end_of_non_terminal_extra && next == state {
        self.nodes[parent as usize].flags |= F_NODE_EXTRA;
      }
      self.nodes[parent as usize].dynamic_precedence += precedence;
      self.push(slice_version, parent, next);
      for &extra in &popped[trailing] {
        self.push(slice_version, extra, next);
      }
      for j in 0..slice_version {
        if j != version && self.merge(j, slice_version) {
          removed += 1;
          break;
        }
      }
      i += 1;
    }
    self.slices = slices;
    self.popped = popped;
    (self.heads.len() > initial_version_count).then_some(initial_version_count)
  }

  /// A reduction when there is one version and one action: no fork can come
  /// of it, so it rewrites the stack in place instead of going through a new
  /// version. `false` when a merged node is in the way and the general path
  /// has to sort the paths out.
  unsafe fn reduce_in_place(&mut self, symbol: u16, count: u8, precedence: i32, production: u16, end_of_non_terminal_extra: bool) -> bool {
    let mut kids = std::mem::take(&mut self.scratch);
    kids.clear();
    let mut node = self.heads[0].node;
    let mut wanted = count;
    while wanted > 0 {
      let n = &self.stack_nodes[node as usize];
      if n.link_count != 1 {
        self.scratch = kids;
        return false;
      }
      kids.push(n.first.subtree);
      if !self.nodes[n.first.subtree as usize].extra() {
        wanted -= 1;
      }
      node = n.first.node;
    }
    kids.reverse();
    // Extras after the last real child are not its children: they go back on top.
    let mut end = kids.len();
    while end > 0 && self.nodes[kids[end - 1] as usize].extra() {
      end -= 1;
    }
    let parent = self.new_parent(symbol, production, &kids[..end]);
    let state = self.stack_nodes[node as usize].state;
    let next = unsafe { self.next_state(state, symbol) };
    if end_of_non_terminal_extra && next == state {
      self.nodes[parent as usize].flags |= F_NODE_EXTRA;
    }
    self.nodes[parent as usize].dynamic_precedence += precedence;
    self.heads[0].node = node;
    self.push(0, parent, next);
    for &extra in &kids[end..] {
      self.push(0, extra, next);
    }
    self.scratch = kids;
    true
  }

  /// `ts_parser__accept`: on each path to the bottom, the last non-extra
  /// subtree is the root and everything around it becomes its children.
  fn accept(&mut self, version: usize, lookahead: u32) {
    self.push(version, lookahead, 1);
    let mut slices = std::mem::take(&mut self.slices);
    self.pop(version, None, &mut slices);
    let popped = std::mem::take(&mut self.popped);
    let popped_version = slices.first().map(|s| s.version);
    for slice in &slices {
      let trees = &popped[slice.range()];
      let Some(at) = trees.iter().rposition(|&n| !self.nodes[n as usize].extra()) else { continue };
      let root = self.nodes[trees[at] as usize];
      let mut kids = std::mem::take(&mut self.scratch);
      kids.clear();
      kids.extend_from_slice(&trees[..at]);
      kids.extend_from_slice(&self.children[root.first_child as usize..(root.first_child + root.child_count) as usize]);
      kids.extend_from_slice(&trees[at + 1..]);
      let candidate = self.new_parent(root.symbol, root.production, &kids);
      self.scratch = kids;
      self.accept_count += 1;
      if self.select_tree(self.finished, candidate) {
        self.finished = candidate;
      }
    }
    self.slices = slices;
    self.popped = popped;
    if let Some(v) = popped_version {
      self.heads.remove(v);
    }
    self.heads[version].status = Status::Halted;
  }

  /// `ts_parser__advance`: take one version as far as its next shift.
  unsafe fn advance(&mut self, version: usize) -> Result<(), Bail> {
    unsafe {
      let mut state = self.stack_nodes[self.heads[version].node as usize].state;
      let position = self.stack_nodes[self.heads[version].node as usize].position;
      let scanner_state = self.heads[version].scanner_state;
      let mut entry = Entry { actions: std::ptr::null(), count: 0, reusable: false };

      // A token another version lexed at this place may serve this one too.
      let mut lookahead = None;
      if self.cached_token != NONE && self.cached_position == position && self.cached_scanner_state == scanner_state {
        let cached_entry = self.entry(state, self.nodes[self.cached_token as usize].symbol);
        if self.can_reuse_token(state, self.cached_token, &cached_entry) {
          lookahead = Some(self.cached_token);
          entry = cached_entry;
        }
      }
      let mut needs_lex = lookahead.is_none();
      loop {
        if needs_lex {
          needs_lex = false;
          lookahead = self.lex(version, state);
          match lookahead {
            Some(token) => {
              self.cached_token = token;
              self.cached_position = position;
              self.cached_scanner_state = scanner_state;
              entry = self.entry(state, self.nodes[token as usize].symbol);
            }
            // After a non-terminal extra the reduction to make is stored under the end symbol.
            None => entry = self.entry(state, SYM_END),
          }
        }

        let mut did_reduce = false;
        let mut reduced_in_place = false;
        let mut last_reduction_version = None;
        for i in 0..entry.count as usize {
          let a = &*entry.actions.add(i);
          match a[0] {
            0 => {
              // A `repetition` shift only marks the entry.
              if a[5] != 0 {
                continue;
              }
              let extra = a[4] != 0;
              let next = if extra { state } else { u16::from_ne_bytes([a[2], a[3]]) };
              let mut token = lookahead.ok_or(Bail::Error)?;
              if self.nodes[token as usize].extra() != extra {
                // The token may be shared with the cache or another version.
                let mut copy = self.nodes[token as usize];
                copy.flags = (copy.flags & !F_NODE_EXTRA) | if extra { F_NODE_EXTRA } else { 0 };
                token = self.nodes.len() as u32;
                self.nodes.push(copy);
              }
              self.push(version, token, next);
              if self.nodes[token as usize].flags & F_NODE_EXTERNAL != 0 {
                self.heads[version].scanner_state = self.nodes[token as usize].scanner_state;
              }
              return Ok(());
            }
            1 => {
              if entry.count == 1
                && self.heads.len() == 1
                && self.reduce_in_place(
                  u16::from_ne_bytes([a[2], a[3]]),
                  a[1],
                  i16::from_ne_bytes([a[4], a[5]]) as i32,
                  u16::from_ne_bytes([a[6], a[7]]),
                  lookahead.is_none(),
                )
              {
                reduced_in_place = true;
                continue;
              }
              let reduced = self.reduce(
                version,
                u16::from_ne_bytes([a[2], a[3]]),
                a[1],
                i16::from_ne_bytes([a[4], a[5]]) as i32,
                u16::from_ne_bytes([a[6], a[7]]),
                lookahead.is_none(),
              );
              did_reduce = true;
              if reduced.is_some() {
                last_reduction_version = reduced;
              }
            }
            2 => {
              let eof = lookahead.ok_or(Bail::Error)?;
              self.accept(version, eof);
              return Ok(());
            }
            // A recover action exists only in the error state.
            _ => return Err(Bail::Error),
          }
        }

        // Carry on in the stack a reduction made, with the same lookahead.
        if reduced_in_place || last_reduction_version.is_some() {
          if let Some(reduced) = last_reduction_version {
            self.renumber(reduced, version);
          }
          state = self.stack_nodes[self.heads[version].node as usize].state;
          match lookahead {
            None => needs_lex = true,
            Some(token) => entry = self.entry(state, self.nodes[token as usize].symbol),
          }
          continue;
        }
        // Every reduction merged into a version that already exists.
        if did_reduce {
          self.heads[version].status = Status::Halted;
          return Ok(());
        }
        // A keyword that is not valid here may still be valid as a plain word.
        if let Some(token) = lookahead {
          let t = self.nodes[token as usize];
          let word = self.lang().keyword_capture_token;
          if t.flags & F_NODE_KEYWORD != 0 && t.symbol != word && !self.is_reserved_word(state, t.symbol) {
            entry = self.entry(state, word);
            if entry.count > 0 {
              let mut copy = t;
              copy.symbol = word;
              lookahead = Some(self.nodes.len() as u32);
              self.nodes.push(copy);
              continue;
            }
          }
        }
        if state == 0 {
          return Err(Bail::Error);
        }
        self.heads[version].status = Status::Paused;
        return Ok(());
      }
    }
  }

  /// `ts_parser__compare_versions`, for stacks that hold no error: a stuck
  /// version loses outright, otherwise dynamic precedence states a preference.
  fn compare_versions(&self, left: (bool, i32), right: (bool, i32)) -> Preference {
    match (left.0, right.0) {
      (false, true) => Preference::TakeLeft,
      (true, false) => Preference::TakeRight,
      _ if left.1 > right.1 => Preference::PreferLeft,
      _ if right.1 > left.1 => Preference::PreferRight,
      _ => Preference::None,
    }
  }

  #[inline]
  fn version_status(&self, version: usize) -> (bool, i32) {
    let h = &self.heads[version];
    (h.status == Status::Paused, self.stack_nodes[h.node as usize].dynamic_precedence)
  }

  /// `ts_parser__condense_stack`: drop finished and beaten versions, merge
  /// the ones that met, and order the rest best first. `Ok(true)` when a
  /// version without error remains.
  fn condense(&mut self) -> Result<bool, Bail> {
    let mut any_clean = false;
    let mut i = 0isize;
    while (i as usize) < self.heads.len() {
      if self.heads[i as usize].status == Status::Halted {
        self.heads.remove(i as usize);
        continue;
      }
      let status_i = self.version_status(i as usize);
      if !status_i.0 {
        any_clean = true;
      }
      let mut j = 0isize;
      while j < i {
        let status_j = self.version_status(j as usize);
        match self.compare_versions(status_j, status_i) {
          Preference::TakeLeft => {
            self.heads.remove(i as usize);
            i -= 1;
            j = i;
          }
          Preference::PreferLeft | Preference::None => {
            if self.merge(j as usize, i as usize) {
              i -= 1;
              j = i;
            }
          }
          Preference::PreferRight => {
            if self.merge(j as usize, i as usize) {
              i -= 1;
              j = i;
            } else {
              self.heads.swap(i as usize, j as usize);
            }
          }
          Preference::TakeRight => {
            self.heads.remove(j as usize);
            i -= 1;
            j -= 1;
          }
        }
        j += 1;
      }
      i += 1;
    }
    self.heads.truncate(MAX_VERSION_COUNT);

    // What is left is either all live or all stuck. All stuck is where
    // tree-sitter starts error recovery, which this driver does not have.
    if self.heads.first().is_some_and(|h| h.status == Status::Paused) {
      if self.finished != NONE {
        return Ok(false);
      }
      return Err(Bail::Error);
    }
    Ok(any_clean)
  }

  /// Parse `src` into `out`. `Err` leaves `out` unspecified.
  pub fn parse(&mut self, src: &str, out: &mut FlatTree) -> Result<(), Bail> {
    if src.len() >= u32::MAX as usize {
      return Err(Bail::Unsupported);
    }
    self.nodes.clear();
    self.children.clear();
    self.stack_nodes.clear();
    self.more_links.clear();
    self.heads.clear();
    self.scanner_states.truncate(1);
    self.scanner_state_ids.clear();
    self.cached_token = NONE;
    self.finished = NONE;
    self.accept_count = 0;
    self.lexer.src = src.as_ptr();
    self.lexer.len = src.len() as u32;
    self.lexer.pos = 0;
    self.lexer.lookahead_size = 0;
    self.lexer.at_eof = false;
    // The bottom of the stack: state 1, nothing below.
    self.stack_nodes.push(StackNode {
      state: 1,
      link_count: 0,
      position: 0,
      dynamic_precedence: 0,
      first: Link { node: NONE, subtree: NONE },
      more: NONE,
    });
    self.heads.push(Head { node: 0, scanner_state: 0, status: Status::Active });

    let mut last_position = 0u32;
    loop {
      let mut version = 0;
      let mut version_count;
      loop {
        version_count = self.heads.len();
        if version >= version_count {
          break;
        }
        while self.heads[version].status == Status::Active {
          // SAFETY: the tables are indexed with states and symbols they
          // produced, and the lexer reads inside `src`, which outlives this call.
          unsafe { self.advance(version)? };
          let position = self.stack_nodes[self.heads[version].node as usize].position;
          if position > last_position || (version > 0 && position == last_position) {
            last_position = position;
            break;
          }
        }
        version += 1;
      }
      let any_clean = self.condense()?;
      if self.finished != NONE && !any_clean {
        break;
      }
      if version_count == 0 {
        break;
      }
    }
    if self.finished == NONE {
      return Err(Bail::Error);
    }
    self.flatten(self.finished, out);
    Ok(())
  }

  /// The arena as pre-order arrays: what `csrc/flatten.c` does to a
  /// tree-sitter tree. Iterative, because without tree-sitter's rebalancing a
  /// long list is a chain of hidden nodes as deep as the list is long.
  fn flatten(&mut self, root: u32, out: &mut FlatTree) {
    out.kind.clear();
    out.flags.clear();
    out.start.clear();
    out.end.clear();
    out.parent.clear();
    out.sub_end.clear();
    out.prev.clear();
    out.field.clear();
    // SAFETY: see `new`; symbols and production ids index the grammar's own tables.
    let l = unsafe { &*self.lang };
    let r = self.nodes[root as usize];
    let (public, named) = unsafe { (*l.public_symbol_map.add(r.symbol as usize), (*l.symbol_metadata.add(r.symbol as usize)).named) };
    out.kind.push(public);
    out.flags.push(if named { F_NAMED } else { 0 });
    out.start.push(r.padding);
    out.end.push(r.padding + r.size);
    out.parent.push(NONE);
    out.sub_end.push(0);
    out.prev.push(NONE);
    out.field.push(0);

    let mut walks = std::mem::take(&mut self.walks);
    let mut owners = std::mem::take(&mut self.owners);
    walks.clear();
    owners.clear();
    walks.push(Walk { parent: root, next: 0, position: r.padding, structural: 0, inherited_field: 0, visible: true });
    owners.push(Owner { idx: 0, last_child: NONE, has_named_child: false });
    while let Some(top) = walks.last_mut() {
      let p = &self.nodes[top.parent as usize];
      if top.next == p.child_count {
        if top.visible {
          let owner = owners.pop().expect("a visible walk pushed its owner");
          if !owner.has_named_child {
            out.flags[owner.idx as usize] |= F_NAMED_LEAF;
          }
          out.sub_end[owner.idx as usize] = out.kind.len() as u32;
        }
        walks.pop();
        continue;
      }
      let child_id = self.children[(p.first_child + top.next) as usize];
      let child = &self.nodes[child_id as usize];
      if top.next > 0 {
        top.position += child.padding;
      }
      let position = top.position;
      let (mut alias, mut field) = (0u16, 0u16);
      if !child.extra() {
        unsafe {
          if p.production != 0 {
            alias = *l.alias_sequences.add(p.production as usize * l.max_alias_sequence_length as usize + top.structural as usize);
          }
          if l.field_count != 0 {
            let slice = &*l.field_map_slices.add(p.production as usize);
            let fields = l.field_map_entries.add(slice.index as usize);
            for f in 0..slice.length as usize {
              let entry = &*fields.add(f);
              if !entry.inherited && entry.child_index as u32 == top.structural {
                field = entry.field_id;
                break;
              }
            }
          }
        }
        if field == 0 {
          field = top.inherited_field;
        }
        top.structural += 1;
      }
      top.next += 1;
      top.position += child.size;

      let meta = unsafe { &*l.symbol_metadata.add(child.symbol as usize) };
      if meta.visible || alias != 0 {
        let idx = out.kind.len() as u32;
        let named = if alias != 0 { unsafe { (*l.symbol_metadata.add(alias as usize)).named } } else { meta.named };
        let mut flags = 0u8;
        if named {
          flags |= F_NAMED;
        }
        if child.extra() {
          flags |= F_EXTRA;
        }
        let owner = owners.last_mut().expect("the root owns everything");
        out.kind.push(unsafe { *l.public_symbol_map.add(if alias != 0 { alias } else { child.symbol } as usize) });
        out.start.push(position);
        out.end.push(position + child.size);
        out.parent.push(owner.idx);
        out.prev.push(owner.last_child);
        out.field.push(field);
        owner.last_child = idx;
        owner.has_named_child |= named;
        if child.child_count > 0 {
          out.flags.push(flags);
          out.sub_end.push(0);
          walks.push(Walk { parent: child_id, next: 0, position, structural: 0, inherited_field: 0, visible: true });
          owners.push(Owner { idx, last_child: NONE, has_named_child: false });
        } else {
          out.flags.push(flags | F_NAMED_LEAF);
          out.sub_end.push(idx + 1);
        }
      } else if child.child_count > 0 {
        // A hidden node: its visible descendants belong to the node above.
        walks.push(Walk { parent: child_id, next: 0, position, structural: 0, inherited_field: field, visible: false });
      }
    }
    self.walks = walks;
    self.owners = owners;
  }
}

/// A node whose children are being emitted.
struct Walk {
  parent: u32,
  next: u32,
  position: u32,
  structural: u32,
  inherited_field: u16,
  /// Whether `parent` is itself in the output, and so owns what is emitted.
  visible: bool,
}

/// A node in the output that is still collecting children.
struct Owner {
  idx: u32,
  last_child: u32,
  has_named_child: bool,
}
