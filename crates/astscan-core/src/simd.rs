//! SIMD search for a node kind in the flattened syntax tree.

#[inline]
fn find_u16_scalar(hay: &[u16], needle: u16) -> Option<usize> {
  hay.iter().position(|&k| k == needle)
}

#[cfg(target_arch = "aarch64")]
mod imp {
  use core::arch::aarch64::*;

  #[inline]
  pub fn find_u16(hay: &[u16], needle: u16) -> Option<usize> {
    let n = hay.len();
    let mut i = 0;
    // SAFETY: NEON is baseline on aarch64; every load is 8 lanes inside `hay`.
    unsafe {
      let v = vdupq_n_u16(needle);
      while i + 8 <= n {
        let eq = vceqq_u16(vld1q_u16(hay.as_ptr().add(i)), v);
        // Narrow each 16-bit lane to a byte: one u64 with 0xFF per hit.
        let mask = vget_lane_u64::<0>(vreinterpret_u64_u8(vmovn_u16(eq)));
        if mask != 0 {
          return Some(i + (mask.trailing_zeros() as usize >> 3));
        }
        i += 8;
      }
    }
    super::find_u16_scalar(&hay[i..], needle).map(|o| i + o)
  }
}

#[cfg(target_arch = "x86_64")]
mod imp {
  use core::arch::x86_64::*;

  #[inline]
  pub fn find_u16(hay: &[u16], needle: u16) -> Option<usize> {
    let n = hay.len();
    let mut i = 0;
    // SAFETY: SSE2 is baseline on x86_64; every load is 8 lanes inside `hay`.
    unsafe {
      let v = _mm_set1_epi16(needle as i16);
      while i + 8 <= n {
        let eq = _mm_cmpeq_epi16(_mm_loadu_si128(hay.as_ptr().add(i) as *const __m128i), v);
        let mask = _mm_movemask_epi8(eq) as u32;
        if mask != 0 {
          return Some(i + (mask.trailing_zeros() as usize >> 1));
        }
        i += 8;
      }
    }
    super::find_u16_scalar(&hay[i..], needle).map(|o| i + o)
  }
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
mod imp {
  use super::find_u16_scalar;

  pub fn find_u16(hay: &[u16], needle: u16) -> Option<usize> {
    find_u16_scalar(hay, needle)
  }
}

pub use imp::find_u16;

/// Characters in a UTF-8 slice: bytes that are not continuation bytes.
#[inline]
pub fn count_chars(bytes: &[u8]) -> usize {
  bytes.iter().filter(|&&b| b & 0xC0 != 0x80).count()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn find_u16_agrees_with_scalar_at_every_offset() {
    for len in 0..40 {
      for at in 0..len {
        let mut hay = vec![7u16; len];
        hay[at] = 9;
        assert_eq!(find_u16(&hay, 9), Some(at), "len {len} at {at}");
        assert_eq!(find_u16(&hay, 3), None);
      }
    }
  }
}
