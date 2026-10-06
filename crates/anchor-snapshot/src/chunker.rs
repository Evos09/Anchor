//! Content-defined chunking: FastCDC with normalized chunking (Xia et al.,
//! "The Design of Fast Content-Defined Chunking for Data Deduplication
//! Based Storage Systems", IEEE TPDS 2020), implemented in-tree (ADR-0006).
//!
//! A gear rolling hash is advanced one byte at a time; a chunk ends where
//! the hash's masked bits are all zero. Below the target average a harder
//! mask (more bits) is used and above it an easier one, which concentrates
//! chunk sizes around the average. Boundaries depend only on nearby
//! content, so an insertion shifts at most the chunks around it.

use std::io::Read;

/// No chunk is shorter than this (except a file's last chunk).
pub const MIN_CHUNK: usize = 16 << 10;
/// Target average chunk size.
pub const AVG_CHUNK: usize = 64 << 10;
/// No chunk is longer than this.
pub const MAX_CHUNK: usize = 256 << 10;

/// 2^16 = AVG_CHUNK; normalization level 2 uses 16 ± 2 mask bits.
const MASK_S: u64 = spread_mask(18);
const MASK_L: u64 = spread_mask(14);

/// A mask with `bits` one-bits spread over the hash's upper 48 bits (the
/// low bits of a gear hash depend on too few input bytes).
const fn spread_mask(bits: u32) -> u64 {
    let mut m = 0u64;
    let mut i = 0;
    while i < bits {
        // Evenly spaced positions in [16, 64).
        let pos = 16 + (i * 48) / bits;
        m |= 1u64 << pos;
        i += 1;
    }
    m
}

/// The gear table: 256 pseudo-random 64-bit values from a fixed SplitMix64
/// seed. Part of the on-disk format: changing it changes chunk boundaries
/// (not correctness, only deduplication across versions).
const GEAR: [u64; 256] = gear_table();

const fn gear_table() -> [u64; 256] {
    let mut t = [0u64; 256];
    let mut state: u64 = 0x616e_6368_6f72_6364; // "anchorcd"
    let mut i = 0;
    while i < 256 {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        t[i] = z ^ (z >> 31);
        i += 1;
    }
    t
}

/// Length of the first chunk of `data` (`data` holds at most
/// [`MAX_CHUNK`] meaningful bytes; more are ignored).
pub fn cut_point(data: &[u8]) -> usize {
    let n = data.len().min(MAX_CHUNK);
    if n <= MIN_CHUNK {
        return n;
    }
    let normal = AVG_CHUNK.min(n);
    let mut hash: u64 = 0;
    let mut i = MIN_CHUNK;
    while i < normal {
        hash = (hash << 1).wrapping_add(GEAR[data[i] as usize]);
        if hash & MASK_S == 0 {
            return i + 1;
        }
        i += 1;
    }
    while i < n {
        hash = (hash << 1).wrapping_add(GEAR[data[i] as usize]);
        if hash & MASK_L == 0 {
            return i + 1;
        }
        i += 1;
    }
    n
}

/// Split an in-memory buffer into chunks.
pub fn chunks(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let n = cut_point(rest);
        out.push(&rest[..n]);
        rest = &rest[n..];
    }
    out
}

/// Streaming chunker over a reader; memory use is bounded by
/// 2 × [`MAX_CHUNK`].
#[derive(Debug)]
pub struct StreamChunker<R> {
    reader: R,
    buf: Vec<u8>,
    start: usize,
    eof: bool,
}

impl<R: Read> StreamChunker<R> {
    pub fn new(reader: R) -> Self {
        StreamChunker {
            reader,
            buf: Vec::with_capacity(2 * MAX_CHUNK),
            start: 0,
            eof: false,
        }
    }

    fn fill(&mut self) -> std::io::Result<()> {
        if self.start > 0 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        while !self.eof && self.buf.len() < MAX_CHUNK {
            let old = self.buf.len();
            self.buf.resize(2 * MAX_CHUNK, 0);
            match self.reader.read(&mut self.buf[old..]) {
                Ok(0) => {
                    self.buf.truncate(old);
                    self.eof = true;
                }
                Ok(k) => self.buf.truncate(old + k),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => self.buf.truncate(old),
                Err(e) => {
                    self.buf.truncate(old);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// The next chunk, or `None` at end of input.
    pub fn next_chunk(&mut self) -> std::io::Result<Option<&[u8]>> {
        if self.buf.len() - self.start < MAX_CHUNK && !self.eof {
            self.fill()?;
        }
        let avail = &self.buf[self.start..];
        if avail.is_empty() {
            return Ok(None);
        }
        let n = cut_point(avail);
        let s = self.start;
        self.start += n;
        Ok(Some(&self.buf[s..s + n]))
    }

    pub fn into_inner(self) -> R {
        self.reader
    }
}
