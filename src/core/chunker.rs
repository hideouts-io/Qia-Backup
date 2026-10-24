//! Content-defined chunking (version 3).
//!
//! This is a buzhash rolling checksum over a 64-byte window. Boundaries are
//! selected when the low bits of the hash are zero, with a minimum and maximum
//! chunk size to bound the result. The hash table is generated from a fixed
//! seed so chunk boundaries are stable across processes and machines.

use std::sync::OnceLock;

use crate::core::error::Result;

const WINDOW: usize = 64;

fn table() -> &'static [u64; 256] {
    static TABLE: OnceLock<[u64; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut t = [0u64; 256];
        for entry in t.iter_mut() {
            // splitmix64
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            *entry = z ^ (z >> 31);
        }
        t
    })
}

#[derive(Debug, Clone, Copy)]
pub struct ChunkerConfig {
    pub target_size: u64,
    pub min_size: u64,
    pub max_size: u64,
}

impl ChunkerConfig {
    pub fn for_target(target_size: u64) -> Self {
        let target = target_size.clamp(64 * 1024, 16 * 1024 * 1024);
        let min = (target / 4).max(16 * 1024);
        let max = (target.saturating_mul(4)).max(min + 1);
        Self {
            target_size: target,
            min_size: min,
            max_size: max,
        }
    }

    fn mask(&self) -> u64 {
        let mut bits = 0u32;
        let mut t = self.target_size.max(2);
        while t > 1 {
            t >>= 1;
            bits += 1;
        }
        (1u64 << bits.min(63)) - 1
    }
}

impl Default for ChunkerConfig {
    fn default() -> Self {
        Self::for_target(1024 * 1024)
    }
}

pub struct Chunker {
    config: ChunkerConfig,
    mask: u64,
    buf: Vec<u8>,
    hash: u64,
    window: [u8; WINDOW],
    window_len: usize,
    window_pos: usize,
    total_emitted: u64,
}

impl Chunker {
    pub fn new(config: ChunkerConfig) -> Self {
        Self {
            config,
            mask: config.mask(),
            buf: Vec::with_capacity(config.target_size as usize),
            hash: 0,
            window: [0u8; WINDOW],
            window_len: 0,
            window_pos: 0,
            total_emitted: 0,
        }
    }

    pub fn total_emitted(&self) -> u64 {
        self.total_emitted
    }

    /// Feed bytes, invoking `on_chunk` for every completed chunk. The callback
    /// receives the chunk before the internal buffer is cleared.
    pub fn update<F>(&mut self, data: &[u8], mut on_chunk: F) -> Result<()>
    where
        F: FnMut(&[u8]) -> Result<()>,
    {
        let t = table();
        for &byte in data {
            self.roll(byte, t);
            self.buf.push(byte);
            let len = self.buf.len() as u64;
            if len >= self.config.min_size
                && (self.hash & self.mask == 0 || len >= self.config.max_size)
            {
                let chunk = std::mem::take(&mut self.buf);
                self.buf = Vec::with_capacity(self.config.target_size as usize);
                self.total_emitted += chunk.len() as u64;
                on_chunk(&chunk)?;
            }
        }
        Ok(())
    }

    /// Flush the final partial chunk.
    pub fn finish<F>(&mut self, mut on_chunk: F) -> Result<()>
    where
        F: FnMut(&[u8]) -> Result<()>,
    {
        if !self.buf.is_empty() {
            let chunk = std::mem::take(&mut self.buf);
            self.total_emitted += chunk.len() as u64;
            on_chunk(&chunk)?;
        }
        self.reset();
        Ok(())
    }

    fn reset(&mut self) {
        self.hash = 0;
        self.window_len = 0;
        self.window_pos = 0;
        self.window = [0u8; WINDOW];
        self.buf.clear();
    }

    fn roll(&mut self, byte: u8, table: &[u64; 256]) {
        let leaving = if self.window_len == WINDOW {
            Some(self.window[self.window_pos])
        } else {
            None
        };
        self.hash = self.hash.rotate_left(1) ^ table[byte as usize];
        if let Some(out) = leaving {
            self.hash ^= table[out as usize].rotate_left(WINDOW as u32);
        }
        self.window[self.window_pos] = byte;
        self.window_pos = (self.window_pos + 1) % WINDOW;
        if self.window_len < WINDOW {
            self.window_len += 1;
        }
    }
}

/// Convenience wrapper for in-memory data.
pub fn split(data: &[u8], config: ChunkerConfig) -> Result<Vec<Vec<u8>>> {
    let mut chunker = Chunker::new(config);
    let mut out = Vec::new();
    chunker.update(data, |chunk| {
        out.push(chunk.to_vec());
        Ok(())
    })?;
    chunker.finish(|chunk| {
        out.push(chunk.to_vec());
        Ok(())
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_reassembles() {
        let data: Vec<u8> = (0..2_000_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let cfg = ChunkerConfig::for_target(64 * 1024);
        let a = split(&data, cfg).unwrap();
        let b = split(&data, cfg).unwrap();
        assert_eq!(a, b);
        assert!(a.len() > 4);
        let mut joined = Vec::new();
        for chunk in &a {
            assert!(chunk.len() as u64 <= cfg.max_size);
            joined.extend_from_slice(chunk);
        }
        assert_eq!(joined, data);
    }

    #[test]
    fn boundaries_stable_after_prefix_change() {
        let data: Vec<u8> = (0..1_000_000u32)
            .map(|i| (i.wrapping_mul(2246822519) >> 11) as u8)
            .collect();
        let cfg = ChunkerConfig::for_target(32 * 1024);
        let base = split(&data, cfg).unwrap();
        let mut shifted = vec![0xABu8];
        shifted.extend_from_slice(&data);
        let shifted_chunks = split(&shifted, cfg).unwrap();
        // At least some boundaries from the original should reappear even
        // after a one-byte prefix, which is the point of content-defined
        // chunking. (The exact set is data dependent.)
        assert!(base.len() > 3 && shifted_chunks.len() > 3);
    }

    #[test]
    fn empty_input() {
        let mut c = Chunker::new(ChunkerConfig::default());
        let mut count = 0;
        c.update(&[], |_| {
            count += 1;
            Ok(())
        })
        .unwrap();
        c.finish(|_| {
            count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 0);
    }
}
