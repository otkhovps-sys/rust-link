//! Microsoft QuickXorHash, the content hash OneDrive reports for every file
//! (personal and business), used to verify uploads end to end.

use base64::{engine::general_purpose::STANDARD, Engine};

const WIDTH_IN_BITS: usize = 160;
const SHIFT: usize = 11;
const BITS_IN_LAST_CELL: usize = 32;

#[derive(Debug, Clone, Default)]
pub struct QuickXorHash {
    data: [u64; 3],
    length: u64,
    shift_so_far: usize,
}

impl QuickXorHash {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        let len = bytes.len();
        let mut vector_index = self.shift_so_far / 64;
        let mut vector_offset = self.shift_so_far % 64;
        for i in 0..len.min(WIDTH_IN_BITS) {
            let is_last_cell = vector_index == self.data.len() - 1;
            let bits_in_cell = if is_last_cell { BITS_IN_LAST_CELL } else { 64 };
            let mut xored: u8 = 0;
            let mut j = i;
            while j < len {
                xored ^= bytes[j];
                j += WIDTH_IN_BITS;
            }
            if vector_offset <= bits_in_cell - 8 {
                self.data[vector_index] ^= (xored as u64) << vector_offset;
            } else {
                let next = if is_last_cell { 0 } else { vector_index + 1 };
                let low = bits_in_cell - vector_offset;
                self.data[vector_index] ^= (xored as u64) << vector_offset;
                self.data[next] ^= (xored as u64) >> low;
            }
            vector_offset += SHIFT;
            while vector_offset >= bits_in_cell {
                vector_index = if is_last_cell { 0 } else { vector_index + 1 };
                vector_offset -= bits_in_cell;
            }
        }
        self.shift_so_far = (self.shift_so_far + SHIFT * (len % WIDTH_IN_BITS)) % WIDTH_IN_BITS;
        self.length += len as u64;
    }

    pub fn finalize(&self) -> [u8; 20] {
        let mut out = [0u8; 20];
        out[0..8].copy_from_slice(&self.data[0].to_le_bytes());
        out[8..16].copy_from_slice(&self.data[1].to_le_bytes());
        out[16..20].copy_from_slice(&self.data[2].to_le_bytes()[..4]);
        for (i, b) in self.length.to_le_bytes().iter().enumerate() {
            out[WIDTH_IN_BITS / 8 - 8 + i] ^= b;
        }
        out
    }

    pub fn base64(&self) -> String {
        STANDARD.encode(self.finalize())
    }
}

/// Hash a file without loading it fully into memory.
pub async fn hash_file(path: &std::path::Path) -> std::io::Result<String> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = QuickXorHash::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.base64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng};

    /// Bit-level reference: byte `i` is XORed into a 160-bit ring at bit `i * 11`.
    fn naive(data: &[u8]) -> [u8; 20] {
        let mut ring = [0u8; 20];
        for (i, byte) in data.iter().enumerate() {
            let start = (i * SHIFT) % WIDTH_IN_BITS;
            for bit in 0..8 {
                if byte >> bit & 1 == 1 {
                    let pos = (start + bit) % WIDTH_IN_BITS;
                    ring[pos / 8] ^= 1 << (pos % 8);
                }
            }
        }
        for (i, b) in (data.len() as u64).to_le_bytes().iter().enumerate() {
            ring[12 + i] ^= b;
        }
        ring
    }

    fn hash(data: &[u8]) -> String {
        let mut h = QuickXorHash::new();
        h.update(data);
        h.base64()
    }

    #[test]
    fn known_vectors() {
        assert_eq!(hash(b""), "AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        assert_eq!(hash(&[0x4A]), "SgAAAAAAAAAAAAAAAQAAAAAAAAA=");
        assert_eq!(hash(&[0xB5, 0xB4]), "taAFAAAAAAAAAAAAAgAAAAAAAAA=");
    }

    #[test]
    fn matches_reference_for_random_inputs_and_chunking() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        for len in [1usize, 19, 20, 159, 160, 161, 333, 1000, 4097] {
            let data: Vec<u8> = (0..len).map(|_| rng.gen()).collect();
            let expected = naive(&data);
            let mut whole = QuickXorHash::new();
            whole.update(&data);
            assert_eq!(whole.finalize(), expected, "len {len}");

            let mut chunked = QuickXorHash::new();
            let mut rest = &data[..];
            while !rest.is_empty() {
                let n = rng.gen_range(1..=rest.len().min(257));
                chunked.update(&rest[..n]);
                rest = &rest[n..];
            }
            assert_eq!(chunked.finalize(), expected, "chunked len {len}");
        }
    }
}
