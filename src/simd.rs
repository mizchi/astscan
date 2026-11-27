//! SIMD search for bytes that must be escaped in JSON strings.

#[inline]
fn needs_escape(b: u8) -> bool {
  b < 0x20 || b == b'"' || b == b'\\'
}

#[inline]
fn find_escape_scalar(hay: &[u8]) -> Option<usize> {
  hay.iter().position(|&b| needs_escape(b))
}

#[cfg(target_arch = "aarch64")]
mod imp {
  use core::arch::aarch64::*;

  #[inline]
  pub fn find_escape(hay: &[u8]) -> Option<usize> {
    let n = hay.len();
    let mut i = 0;
    // SAFETY: NEON is baseline on aarch64; every load is 16 bytes inside `hay`.
    unsafe {
      let quote = vdupq_n_u8(b'"');
      let slash = vdupq_n_u8(b'\\');
      let space = vdupq_n_u8(0x20);
      while i + 16 <= n {
        let v = vld1q_u8(hay.as_ptr().add(i));
        let m = vorrq_u8(vorrq_u8(vceqq_u8(v, quote), vceqq_u8(v, slash)), vcltq_u8(v, space));
        // Four bits per byte: shift-right-narrow turns the 128-bit mask into a u64.
        let mask = vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(m))));
        if mask != 0 {
          return Some(i + (mask.trailing_zeros() as usize >> 2));
        }
        i += 16;
      }
    }
    super::find_escape_scalar(&hay[i..]).map(|o| i + o)
  }
}

#[cfg(target_arch = "x86_64")]
mod imp {
  use core::arch::x86_64::*;

  #[inline]
  pub fn find_escape(hay: &[u8]) -> Option<usize> {
    let n = hay.len();
    let mut i = 0;
    // SAFETY: SSE2 is baseline on x86_64; every load is 16 bytes inside `hay`.
    unsafe {
      let quote = _mm_set1_epi8(b'"' as i8);
      let slash = _mm_set1_epi8(b'\\' as i8);
      // Unsigned `< 0x20` as a signed compare after flipping the top bit.
      let bias = _mm_set1_epi8(i8::MIN);
      let limit = _mm_set1_epi8((0x20u8 ^ 0x80) as i8);
      while i + 16 <= n {
        let v = _mm_loadu_si128(hay.as_ptr().add(i) as *const __m128i);
        let ctl = _mm_cmplt_epi8(_mm_xor_si128(v, bias), limit);
        let m = _mm_or_si128(_mm_or_si128(_mm_cmpeq_epi8(v, quote), _mm_cmpeq_epi8(v, slash)), ctl);
        let mask = _mm_movemask_epi8(m) as u32;
        if mask != 0 {
          return Some(i + mask.trailing_zeros() as usize);
        }
        i += 16;
      }
    }
    super::find_escape_scalar(&hay[i..]).map(|o| i + o)
  }
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
mod imp {
  pub fn find_escape(hay: &[u8]) -> Option<usize> {
    super::find_escape_scalar(hay)
  }
}

pub use imp::find_escape;

/// Characters in a UTF-8 slice: bytes that are not continuation bytes.
#[inline]
pub fn count_chars(bytes: &[u8]) -> usize {
  bytes.iter().filter(|&&b| b & 0xC0 != 0x80).count()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn find_escape_agrees_with_scalar_for_every_byte() {
    for b in 0..=255u8 {
      for at in [0usize, 5, 15, 16, 17, 31, 40] {
        let mut hay = vec![b'a'; 48];
        hay[at] = b;
        assert_eq!(find_escape(&hay), find_escape_scalar(&hay), "byte {b} at {at}");
      }
    }
  }

  #[test]
  fn count_chars_counts_code_points() {
    assert_eq!(count_chars("aé日🎉".as_bytes()), 4);
  }
}
