//! Reusable match storage shared by the scanner and its CLI adapter.

/// A byte range of the source.
pub type Span = (u32, u32);

#[derive(Default, Debug, PartialEq, Eq, Clone)]
pub struct Found {
  /// Index of the rule group within its unit.
  pub group: u32,
  pub start: u32,
  pub end: u32,
  /// Indices, within the group, of the rules an ignore comment silenced here.
  pub dead: Vec<u32>,
  /// `(var, span)`; a later entry for a var shadows an earlier one.
  pub single: Vec<(u32, Span)>,
  /// `(var, first, count)` into `pool`.
  pub multi: Vec<(u32, u32, u32)>,
  pub pool: Vec<Span>,
  /// ast-grep's "secondary" labels.
  pub labels: Vec<Span>,
}

impl Found {
  pub fn clear(&mut self) {
    self.dead.clear();
    self.single.clear();
    self.multi.clear();
    self.pool.clear();
    self.labels.clear();
  }

  /// The largest offset any of its spans reaches, and whether every span is
  /// start-before-end: what a caller compares with the length of the file.
  #[cfg(feature = "internals")]
  pub fn extent(&self) -> Option<u32> {
    let own = [(self.start, self.end)];
    let spans = own.iter().chain(self.single.iter().map(|(_, s)| s)).chain(&self.pool).chain(&self.labels);
    let mut max = 0;
    for &(start, end) in spans {
      if start > end {
        return None;
      }
      max = max.max(end);
    }
    Some(max)
  }
}
