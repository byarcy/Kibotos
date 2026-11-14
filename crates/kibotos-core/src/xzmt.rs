//! Multi-threaded xz decoding for files made of several blocks (what
//! `xz -T` and 7-Zip produce), with memory bounded up front.
//!
//! A multi-threaded decoder holds one whole block per thread in memory.
//! Block sizes come from the file's index, which is untrusted, so before
//! choosing it this module reads the index itself and only goes parallel
//! when every block is small; the thread count keeps the total under
//! [`MEMORY_BUDGET`]. Anything else (one block, several streams, an index it
//! cannot read) uses the ordinary streaming decoder.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

/// Most memory the decoded blocks in flight may take.
const MEMORY_BUDGET: u64 = 512 << 20;
/// Larger blocks are decoded on one thread, streaming.
const MAX_BLOCK: u64 = 128 << 20;

/// Uncompressed size of each block, from a single-stream file's index.
fn block_sizes(file: &mut File) -> io::Result<Vec<u64>> {
    let len = file.seek(SeekFrom::End(0))?;
    if len < 12 + 12 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    // Stream footer: CRC32 (4), backward size (4), flags (2), "YZ".
    let mut footer = [0u8; 12];
    file.seek(SeekFrom::Start(len - 12))?;
    file.read_exact(&mut footer)?;
    if &footer[10..12] != b"YZ" {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let index_size = (u32::from_le_bytes(footer[4..8].try_into().unwrap()) as u64 + 1) * 4;
    if index_size > 16 << 20 || index_size + 12 + 12 > len {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut index = vec![0u8; index_size as usize];
    file.seek(SeekFrom::Start(len - 12 - index_size))?;
    file.read_exact(&mut index)?;

    let mut pos = 0usize;
    let mut varint = || -> io::Result<u64> {
        let mut v = 0u64;
        for shift in (0..63).step_by(7) {
            let b = *index.get(pos).ok_or(io::ErrorKind::InvalidData)?;
            pos += 1;
            v |= u64::from(b & 0x7F) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(io::ErrorKind::InvalidData.into())
    };
    // Index indicator byte (0x00), then the record count.
    if varint()? != 0 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let count = varint()?;
    if count > 1 << 20 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut sizes = Vec::with_capacity(count as usize);
    let mut compressed_total = 0u64;
    for _ in 0..count {
        let unpadded = varint()?;
        sizes.push(varint()?);
        compressed_total = compressed_total.saturating_add(unpadded.div_ceil(4) * 4);
    }
    // One stream only: header (12) + blocks + index + footer (12) must be
    // the whole file. Concatenated streams or padding go single-threaded.
    if 12 + compressed_total + index_size + 12 != len {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(sizes)
}

/// How many threads to use for this file; 0 or 1 means stream it.
fn workers_for(sizes: &[u64]) -> u32 {
    let largest = sizes.iter().copied().max().unwrap_or(0);
    if sizes.len() < 2 || largest == 0 || largest > MAX_BLOCK {
        return 0;
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as u64;
    // Each thread holds a block's input and output.
    let by_memory = MEMORY_BUDGET / (largest * 2);
    cores.min(8).min(by_memory).min(sizes.len() as u64) as u32
}

/// A decoder for an `.xz` file: multi-threaded when that is safe and useful.
pub(crate) fn decoder(mut file: File) -> io::Result<Box<dyn Read>> {
    let workers = block_sizes(&mut file).map(|s| workers_for(&s)).unwrap_or(0);
    file.seek(SeekFrom::Start(0))?;
    if workers >= 2 {
        return Ok(Box::new(lzma_rust2::XzReaderMt::new(file, false, workers)?));
    }
    Ok(Box::new(lzma_rust2::XzReader::new(io::BufReader::with_capacity(64 * 1024, file), true)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_count_respects_memory() {
        assert_eq!(workers_for(&[]), 0);
        assert_eq!(workers_for(&[10 << 20]), 0, "one block: nothing to split");
        assert_eq!(workers_for(&[200 << 20, 1]), 0, "a block too large");
        let n = workers_for(&[24 << 20; 13]);
        assert!(n <= 8 && u64::from(n) * (24 << 20) * 2 <= MEMORY_BUDGET);
        assert_eq!(workers_for(&[100 << 20; 4]).min(2), 2);
    }
}
