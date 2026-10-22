//! Content-defined chunking (FastCDC-style gear hash with normalized chunking): an edit in
//! the middle of a stream only changes the chunks around it, so unchanged data dedups.

pub const MIN: usize = 512 * 1024;
pub const AVG: usize = 2 * 1024 * 1024;
pub const MAX: usize = 8 * 1024 * 1024;

// harder to cut before AVG, easier after (normalized chunking, level 2)
const MASK_S: u64 = (1 << 23) - 1; // log2(AVG) + 2 bits
const MASK_L: u64 = (1 << 19) - 1; // log2(AVG) - 2 bits

const fn gear() -> [u64; 256] {
    // fixed table (splitmix64), so chunk boundaries are the same on every machine
    let mut t = [0u64; 256];
    let mut x: u64 = 0x6f67_2d68_6f73_7421;
    let mut i = 0;
    while i < 256 {
        x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        t[i] = z ^ (z >> 31);
        i += 1;
    }
    t
}
static GEAR: [u64; 256] = gear();

/// Finds cut points in a growing buffer without re-reading it: feed it the buffer (which
/// starts a new chunk) after each append.
#[derive(Default)]
pub struct Cutter {
    pos: usize,
    h: u64,
}

impl Cutter {
    /// Length of the first chunk of `data`, or None until there is a cut point (or MAX bytes).
    /// After a cut the caller drops that many bytes from the front and keeps calling.
    pub fn find(&mut self, data: &[u8]) -> Option<usize> {
        if self.pos < MIN {
            self.pos = MIN;
            self.h = 0;
        }
        let n = data.len().min(MAX);
        while self.pos < n {
            self.h = (self.h << 1).wrapping_add(GEAR[data[self.pos] as usize]);
            let mask = if self.pos < AVG { MASK_S } else { MASK_L };
            self.pos += 1;
            if self.h & mask == 0 {
                return Some(self.cut());
            }
        }
        if n == MAX { Some(self.cut()) } else { None }
    }

    fn cut(&mut self) -> usize {
        let at = self.pos;
        self.pos = 0;
        self.h = 0;
        at
    }
}

/// Where to cut `data` (which starts a new chunk): see [`Cutter::find`].
pub fn cut(data: &[u8]) -> Option<usize> {
    Cutter::default().find(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn split(mut d: &[u8]) -> Vec<Vec<u8>> {
        let mut out = vec![];
        while !d.is_empty() {
            let n = cut(d).unwrap_or(d.len());
            out.push(d[..n].to_vec());
            d = &d[n..];
        }
        out
    }

    #[test]
    fn incremental_matches_one_shot() {
        let a = noise(20 << 20, 3);
        let one = split(&a);
        let (mut c, mut buf, mut got) = (Cutter::default(), Vec::new(), Vec::new());
        for piece in a.chunks(777) {
            buf.extend_from_slice(piece);
            while let Some(n) = c.find(&buf) {
                got.push(buf[..n].to_vec());
                buf.drain(..n);
            }
        }
        if !buf.is_empty() {
            got.push(buf);
        }
        assert_eq!(one, got);
    }

    #[test]
    fn boundaries_resync_after_an_insert() {
        let a = noise(40 << 20, 7);
        let mut b = a.clone();
        b.splice(10 << 20..10 << 20, b"inserted bytes".iter().copied());
        let (ca, cb) = (split(&a), split(&b));
        assert!(ca.iter().all(|c| c.len() >= MIN.min(c.len()) && c.len() <= MAX));
        let avg = a.len() / ca.len();
        assert!(avg > AVG / 2 && avg < AVG * 2, "avg {avg}");
        let same = cb.iter().filter(|c| ca.contains(c)).count();
        assert!(same + 3 >= ca.len(), "{same} of {} chunks shared", ca.len());
    }
}
