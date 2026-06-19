//! Faithful port of MediaPipe's vendored Plan9 UTF routines
//! (rune.c / runetype.c / runetypebody.h). Bit-exact with the C originals,
//! including the same character tables, so tokenization matches the tflite op.

use crate::tables::*;

const RUNEERROR: i32 = 0xFFFD;
const RUNEMAX: i32 = 0x10FFFF;

// Bit layout constants from rune.c
const TX: i32 = 0x80;
const T2: i32 = 0xC0;
const T3: i32 = 0xE0;
const T4: i32 = 0xF0;
const T5: i32 = 0xF8;
const RUNE1: i32 = 0x7F;
const RUNE2: i32 = 0x7FF;
const RUNE3: i32 = 0xFFFF;
const RUNE4: i32 = 0x1FFFFF;
const MASKX: i32 = 0x3F;
const TESTX: i32 = 0xC0;
const BAD: i32 = RUNEERROR;

/// Decode one rune from `s`, reading at most `s.len()` bytes.
/// Returns (rune, bytes_consumed). bytes_consumed == 0 means truncated/badlen.
pub fn charntorune(s: &[u8]) -> (i32, usize) {
    let length = s.len() as i32;
    if length <= 0 {
        return (BAD, 0);
    }
    let c = s[0] as i32;
    if c < TX {
        return (c, 1);
    }
    if length <= 1 {
        return (BAD, 0);
    }
    let c1 = (s[1] as i32) ^ TX;
    if c1 & TESTX != 0 {
        return (BAD, 1);
    }
    if c < T3 {
        if c < T2 {
            return (BAD, 1);
        }
        let l = ((c << 6) | c1) & RUNE2;
        if l <= RUNE1 {
            return (BAD, 1);
        }
        return (l, 2);
    }
    if length <= 2 {
        return (BAD, 0);
    }
    let c2 = (s[2] as i32) ^ TX;
    if c2 & TESTX != 0 {
        return (BAD, 1);
    }
    if c < T4 {
        let l = ((((c << 6) | c1) << 6) | c2) & RUNE3;
        if l <= RUNE2 {
            return (BAD, 1);
        }
        return (l, 3);
    }
    if length <= 3 {
        return (BAD, 0);
    }
    let c3 = (s[3] as i32) ^ TX;
    if c3 & TESTX != 0 {
        return (BAD, 1);
    }
    if c < T5 {
        let l = ((((((c << 6) | c1) << 6) | c2) << 6) | c3) & RUNE4;
        if l <= RUNE3 {
            return (BAD, 1);
        }
        if l > RUNEMAX {
            return (BAD, 1);
        }
        return (l, 4);
    }
    (BAD, 1)
}

/// Encode one rune to UTF-8, appending to `out`.
pub fn runetochar(out: &mut Vec<u8>, rune: i32) {
    let mut c = rune as u32;
    if c <= RUNE1 as u32 {
        out.push(c as u8);
        return;
    }
    if c <= RUNE2 as u32 {
        out.push((T2 as u32 | (c >> 6)) as u8);
        out.push((TX as u32 | (c & MASKX as u32)) as u8);
        return;
    }
    if c > RUNEMAX as u32 {
        c = RUNEERROR as u32;
    }
    if c <= RUNE3 as u32 {
        out.push((T3 as u32 | (c >> 12)) as u8);
        out.push((TX as u32 | ((c >> 6) & MASKX as u32)) as u8);
        out.push((TX as u32 | (c & MASKX as u32)) as u8);
        return;
    }
    out.push((T4 as u32 | (c >> 18)) as u8);
    out.push((TX as u32 | ((c >> 12) & MASKX as u32)) as u8);
    out.push((TX as u32 | ((c >> 6) & MASKX as u32)) as u8);
    out.push((TX as u32 | (c & MASKX as u32)) as u8);
}

/// Port of rbsearch: binary search over `n` groups of stride `ne` in flat `t`.
/// Returns the element index of the group start whose first value <= c, or None.
fn rbsearch(c: i32, t: &[i32], n: usize, ne: usize) -> Option<usize> {
    let mut base = 0usize; // element index of current group start
    let mut n = n;
    while n > 1 {
        let m = n >> 1;
        let p = base + m * ne;
        if c >= t[p] {
            base = p;
            n -= m;
        } else {
            n = m;
        }
    }
    if n != 0 && c >= t[base] {
        Some(base)
    } else {
        None
    }
}

pub fn isalpharune(c: i32) -> bool {
    if let Some(p) = rbsearch(c, &ISALPHAR, ISALPHAR.len() / 2, 2) {
        if c >= ISALPHAR[p] && c <= ISALPHAR[p + 1] {
            return true;
        }
    }
    if let Some(p) = rbsearch(c, &ISALPHAS, ISALPHAS.len(), 1) {
        if c == ISALPHAS[p] {
            return true;
        }
    }
    false
}

pub fn tolowerrune(c: i32) -> i32 {
    const BIAS: i32 = 1048576;
    if let Some(p) = rbsearch(c, &TOLOWERR, TOLOWERR.len() / 3, 3) {
        if c >= TOLOWERR[p] && c <= TOLOWERR[p + 1] {
            return c + TOLOWERR[p + 2] - BIAS;
        }
    }
    if let Some(p) = rbsearch(c, &TOLOWERP, TOLOWERP.len() / 3, 3) {
        if c >= TOLOWERP[p] && c <= TOLOWERP[p + 1] && ((c - TOLOWERP[p]) & 1) == 0 {
            return c + TOLOWERP[p + 2] - BIAS;
        }
    }
    if let Some(p) = rbsearch(c, &TOLOWERS, TOLOWERS.len() / 2, 2) {
        if c == TOLOWERS[p] {
            return c + TOLOWERS[p + 1] - BIAS;
        }
    }
    c
}

/// Port of LowercaseUnicodeStr: lowercases alpha runes, leaves others as-is.
pub fn lowercase_unicode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0usize;
    while i < input.len() {
        let (mut rune, br) = charntorune(&input[i..]);
        if br == 0 {
            break;
        }
        if isalpharune(rune) {
            rune = tolowerrune(rune);
        }
        runetochar(&mut out, rune);
        i += br;
    }
    out
}
