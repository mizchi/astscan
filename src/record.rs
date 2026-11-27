//! A match as byte ranges, and its compact encoding for the cache.
//!
//! A line of output is the file's own text and positions wrapped in JSON. All
//! of it follows from the source plus a few numbers: which rule group
//! matched, where, and where each metavariable and label sits. Both a fresh
//! scan and a cache hit produce `Found`s and hand
//! them to the same writer, so what a hit prints is what the scan printed.
//!
//! In the cache a file's matches are a run of varints, a dozen or so bytes
//! per match where the JSON line is several hundred. Nothing in here trusts
//! those bytes: decoding checks every length, and the caller checks every
//! index and offset against the rules and the file before using them.

pub use astscan_core::internals::{Found, Span};

fn put(out: &mut Vec<u8>, mut value: u32) {
  while value >= 0x80 {
    out.push(value as u8 | 0x80);
    value >>= 7;
  }
  out.push(value as u8);
}

fn put_span(out: &mut Vec<u8>, (start, end): Span) {
  put(out, start);
  put(out, end - start);
}

const DEAD: u32 = 1;
const SINGLE: u32 = 2;
const MULTI: u32 = 4;
const LABELS: u32 = 8;

/// The matches that follow belong to this unit.
pub fn write_unit(out: &mut Vec<u8>, unit: u32) {
  put(out, 0);
  put(out, unit);
}

pub fn write_found(out: &mut Vec<u8>, f: &Found) {
  // Zero is taken by `write_unit`.
  put(out, f.group + 1);
  put_span(out, (f.start, f.end));
  // Most matches carry none of the four lists: one byte says which follow.
  let has = |empty: bool, bit: u32| if empty { 0 } else { bit };
  put(
    out,
    has(f.dead.is_empty(), DEAD) | has(f.single.is_empty(), SINGLE) | has(f.multi.is_empty(), MULTI) | has(f.labels.is_empty(), LABELS),
  );
  if !f.dead.is_empty() {
    put(out, f.dead.len() as u32);
    for &i in &f.dead {
      put(out, i);
    }
  }
  if !f.single.is_empty() {
    put(out, f.single.len() as u32);
    for &(var, span) in &f.single {
      put(out, var);
      put_span(out, span);
    }
  }
  if !f.multi.is_empty() {
    put(out, f.multi.len() as u32);
    for &(var, first, count) in &f.multi {
      put(out, var);
      put(out, count);
      for &span in &f.pool[first as usize..(first + count) as usize] {
        put_span(out, span);
      }
    }
  }
  if !f.labels.is_empty() {
    put(out, f.labels.len() as u32);
    for &span in &f.labels {
      put_span(out, span);
    }
  }
}

/// The bytes are not a run of records.
#[derive(Debug, PartialEq, Eq)]
pub struct Corrupt;

pub enum Item {
  Unit(u32),
  /// The `Found` passed to `next` has been filled.
  Found,
}

pub struct Reader<'a> {
  bytes: &'a [u8],
  at: usize,
}

impl<'a> Reader<'a> {
  pub fn new(bytes: &'a [u8]) -> Self {
    Reader { bytes, at: 0 }
  }

  fn get(&mut self) -> Result<u32, Corrupt> {
    let mut value = 0u32;
    for shift in (0..35).step_by(7) {
      let byte = *self.bytes.get(self.at).ok_or(Corrupt)?;
      self.at += 1;
      // The fifth byte holds the top four bits and nothing more.
      if shift == 28 && byte > 0x0F {
        return Err(Corrupt);
      }
      value |= ((byte & 0x7F) as u32) << shift;
      if byte < 0x80 {
        return Ok(value);
      }
    }
    Err(Corrupt)
  }

  fn span(&mut self) -> Result<Span, Corrupt> {
    let start = self.get()?;
    let end = start.checked_add(self.get()?).ok_or(Corrupt)?;
    Ok((start, end))
  }

  /// A count of things that take at least `each` bytes apiece: one that the
  /// bytes left could not hold is refused before anything is reserved for it.
  fn count(&mut self, each: usize) -> Result<usize, Corrupt> {
    let n = self.get()? as usize;
    if n.saturating_mul(each) > self.bytes.len() - self.at {
      return Err(Corrupt);
    }
    Ok(n)
  }

  /// The next item; `Ok(None)` at the end.
  pub fn next(&mut self, f: &mut Found) -> Result<Option<Item>, Corrupt> {
    if self.at == self.bytes.len() {
      return Ok(None);
    }
    let tag = self.get()?;
    if tag == 0 {
      return Ok(Some(Item::Unit(self.get()?)));
    }
    f.clear();
    f.group = tag - 1;
    (f.start, f.end) = self.span()?;
    let lists = self.get()?;
    if lists > (DEAD | SINGLE | MULTI | LABELS) {
      return Err(Corrupt);
    }
    if lists & DEAD != 0 {
      for _ in 0..self.count(1)? {
        f.dead.push(self.get()?);
      }
    }
    if lists & SINGLE != 0 {
      for _ in 0..self.count(3)? {
        f.single.push((self.get()?, self.span()?));
      }
    }
    if lists & MULTI != 0 {
      for _ in 0..self.count(2)? {
        let var = self.get()?;
        let count = self.count(2)?;
        f.multi.push((var, f.pool.len() as u32, count as u32));
        for _ in 0..count {
          f.pool.push(self.span()?);
        }
      }
    }
    if lists & LABELS != 0 {
      for _ in 0..self.count(2)? {
        f.labels.push(self.span()?);
      }
    }
    Ok(Some(Item::Found))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample() -> Found {
    Found {
      group: 163,
      start: 70_000,
      end: 70_123,
      dead: vec![1],
      single: vec![(0, (70_004, 70_010)), (5, (3, 9)), (0, (70_011, 70_011))],
      multi: vec![(2, 0, 2), (7, 2, 0), (300, 2, 1)],
      pool: vec![(70_020, 70_030), (70_032, 70_040), (4_000_000_000, u32::MAX)],
      labels: vec![(0, 70_123)],
    }
  }

  fn decode(bytes: &[u8]) -> Result<Vec<(u32, Found)>, Corrupt> {
    let (mut reader, mut found, mut unit, mut all) = (Reader::new(bytes), Found::default(), u32::MAX, Vec::new());
    while let Some(item) = reader.next(&mut found)? {
      match item {
        Item::Unit(u) => unit = u,
        Item::Found => all.push((unit, found.clone())),
      }
    }
    Ok(all)
  }

  #[test]
  fn what_is_written_is_what_is_read() {
    let plain = Found { group: 0, start: 5, end: 9, ..Found::default() };
    let mut bytes = Vec::new();
    write_unit(&mut bytes, 3);
    write_found(&mut bytes, &sample());
    write_found(&mut bytes, &plain);
    write_unit(&mut bytes, 0);
    write_found(&mut bytes, &plain);
    assert_eq!(decode(&bytes), Ok(vec![(3, sample()), (3, plain.clone()), (0, plain)]));
    assert_eq!(decode(&[]), Ok(Vec::new()));
  }

  #[test]
  fn a_plain_match_is_a_few_bytes() {
    let mut bytes = Vec::new();
    write_found(&mut bytes, &Found { group: 12, start: 40_000, end: 40_025, ..Found::default() });
    assert!(bytes.len() <= 6, "{} bytes", bytes.len());
  }

  #[test]
  fn damaged_bytes_are_refused_not_followed() {
    let mut bytes = Vec::new();
    write_unit(&mut bytes, 1);
    write_found(&mut bytes, &sample());
    // Cut anywhere inside: an error, or a shorter but well-formed run; never a panic.
    for cut in 1..bytes.len() {
      let _ = decode(&bytes[..cut]);
    }
    assert_eq!(decode(&bytes[..bytes.len() - 1]), Err(Corrupt));
    // Any single byte changed.
    for at in 0..bytes.len() {
      for value in [0x00, 0x7F, 0x80, 0xFF] {
        let mut damaged = bytes.clone();
        damaged[at] = value;
        let _ = decode(&damaged);
      }
    }
    // A count far beyond what the bytes could hold reserves nothing.
    assert_eq!(decode(&[1, 0, 0, DEAD as u8, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F]), Err(Corrupt));
    // A list flag that does not exist.
    assert_eq!(decode(&[1, 0, 0, 16]), Err(Corrupt));
    // A varint that does not fit 32 bits, and a span that wraps.
    assert_eq!(decode(&[0xFF, 0xFF, 0xFF, 0xFF, 0x7F]), Err(Corrupt));
    assert_eq!(decode(&[1, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 1, 0]), Err(Corrupt));
  }

  #[test]
  fn extent_is_the_furthest_offset_of_well_formed_spans() {
    assert_eq!(sample().extent(), Some(u32::MAX));
    assert_eq!(Found { start: 2, end: 9, ..Found::default() }.extent(), Some(9));
    assert_eq!(Found { start: 9, end: 2, ..Found::default() }.extent(), None);
  }
}
