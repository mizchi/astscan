//! JSON output, written by hand into a byte buffer.
//!
//! Two shapes. `Stream` is ast-grep's `--json=stream` line for line, so a
//! consumer of ast-grep reads it unchanged. `Slim` drops everything that is
//! a copy of the source (`text`, `lines`, `labels`) and reports one line per
//! match with every rule id that matched: a consumer that already has the
//! file slices it by `byteOffset`.

use crate::simd::{count_chars, find_escape};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
  Stream,
  Slim,
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Append `s` as a JSON string, escaping exactly what serde_json escapes.
pub fn write_json_str(out: &mut Vec<u8>, s: &[u8]) {
  out.push(b'"');
  write_json_escaped(out, s);
  out.push(b'"');
}

pub fn write_json_escaped(out: &mut Vec<u8>, mut s: &[u8]) {
  while let Some(i) = find_escape(s) {
    out.extend_from_slice(&s[..i]);
    let b = s[i];
    match b {
      b'"' => out.extend_from_slice(b"\\\""),
      b'\\' => out.extend_from_slice(b"\\\\"),
      b'\n' => out.extend_from_slice(b"\\n"),
      b'\r' => out.extend_from_slice(b"\\r"),
      b'\t' => out.extend_from_slice(b"\\t"),
      0x08 => out.extend_from_slice(b"\\b"),
      0x0c => out.extend_from_slice(b"\\f"),
      _ => out.extend_from_slice(&[b'\\', b'u', b'0', b'0', HEX[(b >> 4) as usize], HEX[(b & 15) as usize]]),
    }
    s = &s[i + 1..];
  }
  out.extend_from_slice(s);
}

#[inline]
pub fn write_num(out: &mut Vec<u8>, n: usize) {
  out.extend_from_slice(itoa::Buffer::new().format(n).as_bytes());
}

/// Line and column lookup for one file: built once, on its first match.
#[derive(Default)]
pub struct LineIndex {
  /// Byte offsets of every `\n`.
  newlines: Vec<u32>,
  /// For a file with non-ASCII text: characters before every 64th byte, so a
  /// column on a megabyte-long line costs one block, not the line.
  chars_before: Vec<u32>,
  ascii: bool,
}

#[derive(Clone, Copy)]
pub struct IndexedRange {
  start: u32,
  end: u32,
  start_position: (usize, usize),
  end_position: (usize, usize),
}

const BLOCK: usize = 64;

impl LineIndex {
  pub fn build(&mut self, src: &[u8]) {
    self.newlines.clear();
    self.newlines.extend(memchr::memchr_iter(b'\n', src).map(|i| i as u32));
    self.ascii = src.is_ascii();
    self.chars_before.clear();
    if !self.ascii {
      let mut total = 0u32;
      for block in src.chunks(BLOCK) {
        self.chars_before.push(total);
        total += count_chars(block) as u32;
      }
    }
  }
  #[inline]
  pub fn line(&self, offset: u32) -> usize {
    self.newlines.partition_point(|&p| p < offset)
  }
  #[inline]
  fn line_start(&self, line: usize) -> usize {
    if line == 0 { 0 } else { self.newlines[line - 1] as usize + 1 }
  }
  #[inline]
  pub fn range(&self, src: &[u8], start: u32, end: u32) -> IndexedRange {
    IndexedRange { start, end, start_position: self.position(src, start), end_position: self.position(src, end) }
  }

  #[inline]
  fn chars_up_to(&self, src: &[u8], offset: usize) -> usize {
    let block = offset / BLOCK;
    match self.chars_before.get(block) {
      Some(&before) => before as usize + count_chars(&src[block * BLOCK..offset]),
      // `offset` is the end of a source whose length is a multiple of the block size.
      None => self.chars_before.last().map_or(0, |&b| b as usize) + count_chars(&src[(block - 1) * BLOCK..offset]),
    }
  }
  /// `(line, column)`, the column counted in characters.
  #[inline]
  pub fn position(&self, src: &[u8], offset: u32) -> (usize, usize) {
    let line = self.line(offset);
    let start = self.line_start(line);
    let offset = offset as usize;
    let col = if self.ascii { offset - start } else { self.chars_up_to(src, offset) - self.chars_up_to(src, start) };
    (line, col)
  }
}

/// `{"byteOffset":{"start":S,"end":E},"start":{"line":L,"column":C},"end":{…}}`
pub fn write_range(out: &mut Vec<u8>, range: IndexedRange) {
  let IndexedRange { start, end, start_position: (sl, sc), end_position: (el, ec) } = range;
  out.extend_from_slice(b"{\"byteOffset\":{\"start\":");
  write_num(out, start as usize);
  out.extend_from_slice(b",\"end\":");
  write_num(out, end as usize);
  out.extend_from_slice(b"},\"start\":{\"line\":");
  write_num(out, sl);
  out.extend_from_slice(b",\"column\":");
  write_num(out, sc);
  out.extend_from_slice(b"},\"end\":{\"line\":");
  write_num(out, el);
  out.extend_from_slice(b",\"column\":");
  write_num(out, ec);
  out.extend_from_slice(b"}}");
}

/// The source lines a match touches, reusing the escaped contents of its `text` field.
pub fn write_lines(out: &mut Vec<u8>, src: &[u8], idx: &LineIndex, range: IndexedRange, escaped_match: &[u8]) {
  let leading = idx.line_start(range.start_position.0);
  let trailing = idx.newlines.get(range.end_position.0).map_or(src.len(), |&offset| offset as usize);
  let end = range.end as usize;
  let trailing_chars = if idx.ascii { trailing - end } else { idx.chars_up_to(src, trailing) - idx.chars_up_to(src, end) };
  out.extend_from_slice(b",\"lines\":\"");
  write_json_escaped(out, &src[leading..range.start as usize]);
  out.extend_from_slice(escaped_match);
  write_json_escaped(out, &src[end..trailing]);
  out.extend_from_slice(b"\",\"charCount\":{\"leading\":");
  write_num(out, range.start_position.1);
  out.extend_from_slice(b",\"trailing\":");
  write_num(out, trailing_chars);
  out.push(b'}');
}

#[cfg(test)]
mod tests {
  use super::*;

  fn escaped(s: &str) -> String {
    let mut out = Vec::new();
    write_json_str(&mut out, s.as_bytes());
    String::from_utf8(out).unwrap()
  }

  #[test]
  fn escapes_what_serde_json_escapes() {
    for s in ["plain", "quote\"back\\slash", "a\nb\tc\rd", "\u{8}\u{c}\u{1}\u{1f}", "日本語 \u{7f} 🎉", "", &"x\"".repeat(40)] {
      assert_eq!(escaped(s), serde_json::to_string(s).unwrap());
    }
  }

  #[test]
  fn positions_count_characters_not_bytes() {
    let src = "aé\n日本x";
    let mut idx = LineIndex::default();
    idx.build(src.as_bytes());
    assert_eq!(idx.position(src.as_bytes(), 3), (0, 2));
    assert_eq!(idx.position(src.as_bytes(), 4), (1, 0));
    assert_eq!(idx.position(src.as_bytes(), 10), (1, 2));
  }

  #[test]
  fn indexed_lines_preserve_line_endings_and_character_counts() {
    for src in ["", "plain\ntext\n", "é前\r\n\n next 日本\n終", &"é".repeat(32)] {
      let bytes = src.as_bytes();
      let offsets: Vec<_> = src.char_indices().map(|(i, _)| i).chain([src.len()]).collect();
      let mut idx = LineIndex::default();
      idx.build(bytes);
      let mut out = Vec::new();
      for (i, &start) in offsets.iter().enumerate() {
        for &end in &offsets[i..] {
          out.clear();
          out.push(b'{');
          out.extend_from_slice(b"\"test\":0");
          let mut escaped_match = Vec::new();
          write_json_escaped(&mut escaped_match, &bytes[start..end]);
          write_lines(&mut out, bytes, &idx, idx.range(bytes, start as u32, end as u32), &escaped_match);
          out.push(b'}');
          let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
          let line_start = src[..start].rfind('\n').map_or(0, |i| i + 1);
          let line_end = src[end..].find('\n').map_or(src.len(), |i| end + i);
          assert_eq!(value["lines"], &src[line_start..line_end]);
          assert_eq!(value["charCount"]["leading"], src[line_start..start].chars().count());
          assert_eq!(value["charCount"]["trailing"], src[end..line_end].chars().count());
        }
      }
    }
  }

  #[test]
  fn columns_on_a_long_line_agree_with_a_plain_count() {
    // Long enough to cross several blocks, with the end on a block boundary.
    let src = "é日x".repeat(64);
    assert_eq!(src.len() % BLOCK, 0);
    let mut idx = LineIndex::default();
    idx.build(src.as_bytes());
    for (chars, (offset, _)) in src.char_indices().enumerate() {
      assert_eq!(idx.position(src.as_bytes(), offset as u32), (0, chars));
    }
    assert_eq!(idx.position(src.as_bytes(), src.len() as u32), (0, src.chars().count()));
  }
}
