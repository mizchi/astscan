//! A tree-sitter tree flattened into pre-order struct-of-arrays.
//!
//! Matching never touches the tree-sitter node API. One cursor walk copies
//! what the matcher needs into parallel arrays indexed by pre-order position,
//! which makes the three operations rules lean on cheap:
//!
//! - "every node" is a linear pass over `kind`,
//! - "every descendant of n" is the contiguous index range `n+1..sub_end[n]`,
//! - "parent / previous sibling / next sibling" are array reads.

use tree_sitter::Tree;

pub const NONE: u32 = u32::MAX;

/// Where two flattenings of one tree disagree, as text; `None` when they agree.
pub fn first_difference(a: &FlatTree, b: &FlatTree) -> Option<String> {
  if a.len() != b.len() {
    return Some(format!("node counts {} and {}", a.len(), b.len()));
  }
  for i in 0..a.len() {
    let row = |t: &FlatTree| (t.kind[i], t.flags[i], t.start[i], t.end[i], t.parent[i], t.sub_end[i], t.prev[i], t.field[i]);
    if row(a) != row(b) {
      return Some(format!("node {i}: {:?} and {:?} (kind, flags, start, end, parent, sub_end, prev, field)", row(a), row(b)));
    }
  }
  None
}

pub const F_NAMED: u8 = 1;
pub const F_EXTRA: u8 = 2;
pub const F_MISSING: u8 = 4;
/// No named children. Not the same as "named and childless".
pub const F_NAMED_LEAF: u8 = 8;

#[derive(Default)]
pub struct FlatTree {
  pub kind: Vec<u16>,
  pub flags: Vec<u8>,
  pub start: Vec<u32>,
  pub end: Vec<u32>,
  pub parent: Vec<u32>,
  /// Exclusive pre-order end of the node's subtree; also its next sibling
  /// when that index is still inside the parent's subtree.
  pub sub_end: Vec<u32>,
  pub prev: Vec<u32>,
  /// Field id this node holds in its parent, 0 for none.
  pub field: Vec<u16>,
  stack: Vec<Frame>,
}

struct Frame {
  idx: u32,
  last_child: u32,
  has_named_child: bool,
}

/// The arrays `csrc/flatten.c` fills; field for field its `AstscanFlat`.
#[repr(C)]
struct RawFlat {
  kind: *mut u16,
  flags: *mut u8,
  start: *mut u32,
  end: *mut u32,
  parent: *mut u32,
  sub_end: *mut u32,
  prev: *mut u32,
  field: *mut u16,
  len: u32,
  cap: u32,
  overflow: u32,
}

unsafe extern "C" {
  fn astscan_node_count(tree: *const tree_sitter::ffi::TSTree) -> u32;
  fn astscan_flatten(tree: *const tree_sitter::ffi::TSTree, out: *mut RawFlat);
}

impl FlatTree {
  pub fn len(&self) -> usize {
    self.kind.len()
  }

  #[cfg(feature = "internals")]
  pub fn is_empty(&self) -> bool {
    self.kind.is_empty()
  }

  /// Flatten `tree`, by the direct subtree walk when it succeeds and by the
  /// cursor when it does not.
  pub fn build(&mut self, tree: Tree) {
    let raw = tree.into_raw();
    // SAFETY: `raw` is a live tree for the duration of both calls; every
    // array has room for `cap` elements, the walk writes indices below
    // `cap` only, and `set_len` covers exactly the prefix it wrote.
    let ok = unsafe {
      let cap = astscan_node_count(raw) as usize;
      self.kind.clear();
      self.flags.clear();
      self.start.clear();
      self.end.clear();
      self.parent.clear();
      self.sub_end.clear();
      self.prev.clear();
      self.field.clear();
      self.kind.reserve(cap);
      self.flags.reserve(cap);
      self.start.reserve(cap);
      self.end.reserve(cap);
      self.parent.reserve(cap);
      self.sub_end.reserve(cap);
      self.prev.reserve(cap);
      self.field.reserve(cap);
      let mut out = RawFlat {
        kind: self.kind.as_mut_ptr(),
        flags: self.flags.as_mut_ptr(),
        start: self.start.as_mut_ptr(),
        end: self.end.as_mut_ptr(),
        parent: self.parent.as_mut_ptr(),
        sub_end: self.sub_end.as_mut_ptr(),
        prev: self.prev.as_mut_ptr(),
        field: self.field.as_mut_ptr(),
        len: 0,
        cap: cap as u32,
        overflow: 0,
      };
      astscan_flatten(raw, &mut out);
      let ok = out.overflow == 0 && out.len as usize == cap;
      let len = if ok { cap } else { 0 };
      self.kind.set_len(len);
      self.flags.set_len(len);
      self.start.set_len(len);
      self.end.set_len(len);
      self.parent.set_len(len);
      self.sub_end.set_len(len);
      self.prev.set_len(len);
      self.field.set_len(len);
      ok
    };
    // SAFETY: `raw` came from `into_raw` above and has not been freed.
    let tree = unsafe { Tree::from_raw(raw) };
    if !ok {
      self.build_with_cursor(&tree);
    }
  }

  /// The same arrays through the public cursor API: the reference `build` is checked against.
  pub fn build_with_cursor(&mut self, tree: &Tree) {
    self.kind.clear();
    self.flags.clear();
    self.start.clear();
    self.end.clear();
    self.parent.clear();
    self.sub_end.clear();
    self.prev.clear();
    self.field.clear();
    self.stack.clear();

    let mut cur = tree.walk();
    loop {
      let n = cur.node();
      let idx = self.kind.len() as u32;
      let named = n.is_named();
      let (parent, prev) = match self.stack.last_mut() {
        Some(top) => {
          let p = top.last_child;
          top.last_child = idx;
          top.has_named_child |= named;
          (top.idx, p)
        }
        None => (NONE, NONE),
      };
      let mut flags = 0u8;
      if named {
        flags |= F_NAMED;
      }
      if n.is_extra() {
        flags |= F_EXTRA;
      }
      if n.is_missing() {
        flags |= F_MISSING;
      }
      self.kind.push(n.kind_id());
      self.flags.push(flags);
      self.start.push(n.start_byte() as u32);
      self.end.push(n.end_byte() as u32);
      self.parent.push(parent);
      self.prev.push(prev);
      self.sub_end.push(idx + 1);
      self.field.push(cur.field_id().map_or(0, |f| f.get()));

      if cur.goto_first_child() {
        self.stack.push(Frame { idx, last_child: NONE, has_named_child: false });
        continue;
      }
      self.flags[idx as usize] |= F_NAMED_LEAF;
      loop {
        if cur.goto_next_sibling() {
          break;
        }
        if !cur.goto_parent() {
          return;
        }
        let top = self.stack.pop().expect("a parent was pushed when descending");
        self.sub_end[top.idx as usize] = self.kind.len() as u32;
        if !top.has_named_child {
          self.flags[top.idx as usize] |= F_NAMED_LEAF;
        }
      }
    }
  }

  #[inline]
  pub fn is_named(&self, n: u32) -> bool {
    self.flags[n as usize] & F_NAMED != 0
  }
  #[inline]
  pub fn is_extra(&self, n: u32) -> bool {
    self.flags[n as usize] & F_EXTRA != 0
  }
  #[inline]
  pub fn is_named_leaf(&self, n: u32) -> bool {
    self.flags[n as usize] & F_NAMED_LEAF != 0
  }
  #[cfg(feature = "internals")]
  pub fn is_leaf(&self, n: u32) -> bool {
    self.sub_end[n as usize] == n + 1
  }
  #[inline]
  pub fn next_sibling(&self, n: u32) -> u32 {
    let p = self.parent[n as usize];
    if p == NONE {
      return NONE;
    }
    let e = self.sub_end[n as usize];
    if e < self.sub_end[p as usize] { e } else { NONE }
  }
  /// The next sibling as tree-sitter's `ts_node_next_sibling` finds it: by
  /// position, so a zero-width node sitting exactly at this node's end (a
  /// MISSING token, say) is passed over.
  #[inline]
  pub fn ts_next_sibling(&self, n: u32) -> u32 {
    let end = self.end[n as usize];
    let mut c = self.next_sibling(n);
    while c != NONE && self.end[c as usize] <= end {
      c = self.next_sibling(c);
    }
    c
  }
  /// Where a cursor lands for `goto_first_child_for_byte(start of n)` on n's
  /// parent: n itself unless n is zero-width, in which case the node after it.
  /// The root stands in as its own parent, as ast-grep's sibling walk has it.
  pub fn sibling_anchor(&self, n: u32) -> u32 {
    let (start, end) = (self.start[n as usize], self.end[n as usize]);
    let parent = self.parent[n as usize];
    if parent != NONE && end > start {
      return n;
    }
    let base = if parent == NONE { n } else { parent };
    let mut kids = self.children(base);
    while let Some(c) = kids.next(self) {
      if self.end[c as usize] > start {
        return c;
      }
    }
    NONE
  }
  #[inline]
  pub fn children(&self, n: u32) -> Kids {
    Kids { cur: n + 1, end: self.sub_end[n as usize] }
  }
  pub fn child_by_field(&self, n: u32, field: u16) -> Option<u32> {
    let mut k = self.children(n);
    while let Some(c) = k.next(self) {
      if self.field[c as usize] == field {
        return Some(c);
      }
    }
    None
  }
}

/// Cursor over one node's direct children. Copy, so a peek is a read.
#[derive(Clone, Copy)]
pub struct Kids {
  cur: u32,
  end: u32,
}

impl Kids {
  #[inline]
  pub fn peek(&self) -> Option<u32> {
    (self.cur < self.end).then_some(self.cur)
  }
  #[inline]
  pub fn next(&mut self, t: &FlatTree) -> Option<u32> {
    if self.cur >= self.end {
      return None;
    }
    let c = self.cur;
    self.cur = t.sub_end[c as usize];
    Some(c)
  }
}
