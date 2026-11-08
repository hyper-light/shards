//! Huffman codes as Go's compress/flate makes them (huffman_code.go): each code a
//! length-limited canonical code whose lengths come from the package-merge-like
//! `bitCounts`, its codes assigned in literal order within each length.

/// The number of literal and length codes (inflate.go `maxNumLit`).
pub const MAX_NUM_LIT: usize = 286;

/// `maxBitsLimit`.
const MAX_BITS_LIMIT: usize = 16;

/// A code: its bits, reversed for writing least significant first, and its length.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Hcode {
    pub code: u16,
    pub len: u16,
}

#[derive(Debug, Clone, Copy)]
struct LiteralNode {
    literal: u16,
    freq: i32,
}

#[derive(Debug, Clone, Copy, Default)]
struct LevelInfo {
    level: i32,
    last_freq: i32,
    next_char_freq: i32,
    next_pair_freq: i32,
    needed: i32,
}

/// `huffmanEncoder`: the codes of an alphabet.
#[derive(Debug, Clone)]
pub struct Encoder {
    pub codes: Vec<Hcode>,
}

impl Encoder {
    pub fn new(size: usize) -> Encoder {
        Encoder {
            codes: vec![Hcode::default(); size],
        }
    }

    /// `generateFixedLiteralEncoding`.
    pub fn fixed_literal() -> Encoder {
        let mut h = Encoder::new(MAX_NUM_LIT);
        for (ch, c) in (0u16..).zip(h.codes.iter_mut()) {
            let (bits, size) = match ch {
                0..144 => (ch + 48, 8),
                144..256 => (ch + 400 - 144, 9),
                256..280 => (ch - 256, 7),
                _ => (ch + 192 - 280, 8),
            };
            *c = Hcode {
                code: reverse_bits(bits, size),
                len: size,
            };
        }
        h
    }

    /// `generateFixedOffsetEncoding`.
    pub fn fixed_offset() -> Encoder {
        let mut h = Encoder::new(30);
        for (ch, c) in (0u16..).zip(h.codes.iter_mut()) {
            *c = Hcode {
                code: reverse_bits(ch, 5),
                len: 5,
            };
        }
        h
    }

    /// `bitLength`: the bits `freq` takes in these codes.
    pub fn bit_length(&self, freq: &[i32]) -> i64 {
        freq.iter()
            .zip(&self.codes)
            .filter(|(f, _)| **f != 0)
            .map(|(f, c)| i64::from(*f) * i64::from(c.len))
            .sum()
    }

    /// `generate`: the minimum codes for `freq`, none longer than `max_bits`.
    pub fn generate(&mut self, freq: &[i32], max_bits: i32) {
        let mut list: Vec<LiteralNode> = Vec::with_capacity(freq.len() + 1);
        for (i, &f) in (0u16..).zip(freq) {
            if f != 0 {
                list.push(LiteralNode { literal: i, freq: f });
            } else if let Some(c) = self.codes.get_mut(usize::from(i)) {
                c.len = 0;
            }
        }
        if list.len() <= 2 {
            // Each of one or two literals is one bit, its code its place (not reversed).
            for (i, node) in (0u16..).zip(&list) {
                if let Some(c) = self.codes.get_mut(usize::from(node.literal)) {
                    *c = Hcode { code: i, len: 1 };
                }
            }
            return;
        }
        // byFreq: by frequency, then literal; the keys are distinct, so any sort is Go's.
        list.sort_unstable_by(|a, b| a.freq.cmp(&b.freq).then(a.literal.cmp(&b.literal)));
        let bit_count = bit_counts(&list, max_bits);
        self.assign(&bit_count, &mut list);
    }

    /// `assignEncodingAndSize`: the literals of each length, longest first from the
    /// list's least frequent, given consecutive codes in literal order.
    fn assign(&mut self, bit_count: &[i32], list: &mut Vec<LiteralNode>) {
        let mut code: u16 = 0;
        for (n, &bits) in bit_count.iter().enumerate() {
            code <<= 1;
            if n == 0 || bits == 0 {
                continue;
            }
            let take = usize::try_from(bits).unwrap_or(0).min(list.len());
            let mut chunk = list.split_off(list.len() - take);
            // byLiteral: literals are distinct.
            chunk.sort_unstable_by_key(|node| node.literal);
            let len = u16::try_from(n).unwrap_or(0);
            for node in chunk {
                if let Some(c) = self.codes.get_mut(usize::from(node.literal)) {
                    *c = Hcode {
                        code: reverse_bits(code, len),
                        len,
                    };
                }
                code = code.wrapping_add(1);
            }
        }
    }
}

fn freq_at(list: &[LiteralNode], i: i32) -> i32 {
    // Past the list is its `maxNode`, of frequency MaxInt32.
    usize::try_from(i)
        .ok()
        .and_then(|i| list.get(i))
        .map_or(i32::MAX, |n| n.freq)
}

fn lv(levels: &[LevelInfo; MAX_BITS_LIMIT], i: i32) -> LevelInfo {
    usize::try_from(i)
        .ok()
        .and_then(|i| levels.get(i))
        .copied()
        .unwrap_or_default()
}

fn lv_mut(levels: &mut [LevelInfo; MAX_BITS_LIMIT], i: i32) -> Option<&mut LevelInfo> {
    usize::try_from(i).ok().and_then(|i| levels.get_mut(i))
}

fn leaf(counts: &[[i32; MAX_BITS_LIMIT]; MAX_BITS_LIMIT], i: i32, j: i32) -> i32 {
    match (usize::try_from(i), usize::try_from(j)) {
        (Ok(i), Ok(j)) => counts.get(i).and_then(|r| r.get(j)).copied().unwrap_or(0),
        _ => 0,
    }
}

fn set_leaf(counts: &mut [[i32; MAX_BITS_LIMIT]; MAX_BITS_LIMIT], i: i32, j: i32, v: i32) {
    if let (Ok(i), Ok(j)) = (usize::try_from(i), usize::try_from(j))
        && let Some(c) = counts.get_mut(i).and_then(|r| r.get_mut(j))
    {
        *c = v;
    }
}

/// `bitCounts`: how many literals of `list` (by increasing frequency, three or more)
/// each code length takes, none longer than `max_bits`.
fn bit_counts(list: &[LiteralNode], max_bits: i32) -> Vec<i32> {
    let n = i32::try_from(list.len()).unwrap_or(i32::MAX);
    let max_bits = max_bits.min(n - 1);
    let mut levels = [LevelInfo::default(); MAX_BITS_LIMIT];
    let mut leaf_counts = [[0i32; MAX_BITS_LIMIT]; MAX_BITS_LIMIT];
    for level in 1..=max_bits {
        if let Some(l) = lv_mut(&mut levels, level) {
            *l = LevelInfo {
                level,
                last_freq: freq_at(list, 1),
                next_char_freq: freq_at(list, 2),
                next_pair_freq: if level == 1 {
                    i32::MAX
                } else {
                    freq_at(list, 0).wrapping_add(freq_at(list, 1))
                },
                needed: 0,
            };
        }
        set_leaf(&mut leaf_counts, level, level, 2);
    }
    if let Some(l) = lv_mut(&mut levels, max_bits) {
        l.needed = 2 * n - 4;
    }
    let mut level = max_bits;
    loop {
        let mut l = lv(&levels, level);
        if l.next_pair_freq == i32::MAX && l.next_char_freq == i32::MAX {
            // Out of leaves and pairs: this level is done, and none below is visited again.
            l.needed = 0;
            if let Some(x) = lv_mut(&mut levels, level) {
                *x = l;
            }
            if let Some(up) = lv_mut(&mut levels, level + 1) {
                up.next_pair_freq = i32::MAX;
            }
            level += 1;
            continue;
        }
        let prev_freq = l.last_freq;
        if l.next_char_freq < l.next_pair_freq {
            // A leaf.
            let next = leaf(&leaf_counts, level, level) + 1;
            l.last_freq = l.next_char_freq;
            set_leaf(&mut leaf_counts, level, level, next);
            l.next_char_freq = freq_at(list, next);
        } else {
            // A pair from the level below.
            l.last_freq = l.next_pair_freq;
            for j in 0..level {
                let below = leaf(&leaf_counts, level - 1, j);
                set_leaf(&mut leaf_counts, level, j, below);
            }
            if let Some(down) = lv_mut(&mut levels, l.level - 1) {
                down.needed = 2;
            }
        }
        l.needed -= 1;
        if let Some(x) = lv_mut(&mut levels, level) {
            *x = l;
        }
        if l.needed == 0 {
            if l.level == max_bits {
                break;
            }
            if let Some(up) = lv_mut(&mut levels, l.level + 1) {
                up.next_pair_freq = prev_freq.wrapping_add(l.last_freq);
            }
            level += 1;
        } else {
            while lv(&levels, level - 1).needed > 0 {
                level -= 1;
            }
        }
    }
    let mut bit_count = vec![0i32; usize::try_from(max_bits + 1).unwrap_or(1)];
    for (bits, level) in (1usize..).zip((1..=max_bits).rev()) {
        if let Some(b) = bit_count.get_mut(bits) {
            *b = leaf(&leaf_counts, max_bits, level) - leaf(&leaf_counts, max_bits, level - 1);
        }
    }
    bit_count
}

/// `reverseBits`: the low `len` bits of `number`, reversed.
pub fn reverse_bits(number: u16, len: u16) -> u16 {
    number
        .checked_shl(u32::from(16u16.saturating_sub(len)))
        .unwrap_or(0)
        .reverse_bits()
}
