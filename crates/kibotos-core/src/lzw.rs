//! Decoder for Unix `compress` files (`.Z`, `.taz`): LZW with 9 to 16-bit
//! codes. It follows `compress.c`, including its quirk of skipping to the
//! end of a group of codes whenever the code width changes or the table is
//! cleared.

use std::io::{self, Read};

const MAGIC: [u8; 2] = [0x1F, 0x9D];
const CLEAR: usize = 256;
const INIT_BITS: u32 = 9;

pub(crate) struct LzwReader<R> {
    input: R,
    /// Input bytes not yet consumed; `buf[0]` is at bit `base_bits` of the file.
    buf: Vec<u8>,
    base_bits: u64,
    posbits: u64,
    /// Where the codes of the current width began: codes come in groups of
    /// `n_bits` bytes counted from here, and a width change skips the rest
    /// of the current group.
    group_base: u64,
    eof: bool,
    maxbits: u32,
    block_mode: bool,
    n_bits: u32,
    maxcode: usize,
    maxmaxcode: usize,
    free_ent: usize,
    oldcode: Option<usize>,
    finchar: u8,
    prefix: Vec<u16>,
    suffix: Vec<u8>,
    /// Decoded bytes not yet returned.
    out: Vec<u8>,
    out_pos: usize,
    started: bool,
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("compress (.Z): {msg}"))
}

impl<R: Read> LzwReader<R> {
    pub fn new(input: R) -> LzwReader<R> {
        LzwReader {
            input,
            buf: Vec::new(),
            base_bits: 0,
            posbits: 0,
            group_base: 0,
            eof: false,
            maxbits: 16,
            block_mode: true,
            n_bits: INIT_BITS,
            maxcode: (1 << INIT_BITS) - 1,
            maxmaxcode: 1 << 16,
            free_ent: 256,
            oldcode: None,
            finchar: 0,
            prefix: vec![0; 1 << 16],
            suffix: (0..=255u8).chain(std::iter::repeat_n(0, (1 << 16) - 256)).collect(),
            out: Vec::with_capacity(64 * 1024),
            out_pos: 0,
            started: false,
        }
    }

    fn start(&mut self) -> io::Result<()> {
        let mut head = [0u8; 3];
        self.input.read_exact(&mut head).map_err(|_| bad("truncated header"))?;
        if head[..2] != MAGIC {
            return Err(bad("not a .Z file"));
        }
        self.maxbits = u32::from(head[2] & 0x1F);
        self.block_mode = head[2] & 0x80 != 0;
        if !(INIT_BITS..=16).contains(&self.maxbits) {
            return Err(bad("unsupported code size"));
        }
        self.maxmaxcode = 1 << self.maxbits;
        self.free_ent = if self.block_mode { 257 } else { 256 };
        self.base_bits = 24;
        self.posbits = 24;
        self.group_base = 24;
        self.started = true;
        Ok(())
    }

    /// Makes sure the bits up to `end` (absolute) are in `buf`, if the file has them.
    fn fill(&mut self, end: u64) -> io::Result<bool> {
        // Drop bytes already consumed.
        let consumed = ((self.posbits - self.base_bits) / 8) as usize;
        if consumed > 0 && consumed >= self.buf.len() / 2 {
            let consumed = consumed.min(self.buf.len());
            self.buf.drain(..consumed);
            self.base_bits += consumed as u64 * 8;
        }
        while self.base_bits + self.buf.len() as u64 * 8 < end {
            if self.eof {
                return Ok(false);
            }
            let old = self.buf.len();
            self.buf.resize(old + 64 * 1024, 0);
            let n = loop {
                match self.input.read(&mut self.buf[old..]) {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    r => break r?,
                }
            };
            self.buf.truncate(old + n);
            if n == 0 {
                self.eof = true;
            }
        }
        Ok(true)
    }

    /// Skips to the end of the current group; codes of the new width start there.
    fn round_up(&mut self) {
        let g = u64::from(self.n_bits) << 3;
        let used = self.posbits - self.group_base;
        self.posbits = self.group_base + used.div_ceil(g) * g;
        self.group_base = self.posbits;
    }

    /// The next code, or `None` at the end of the data.
    fn next_code(&mut self) -> io::Result<Option<usize>> {
        if !self.fill(self.posbits + u64::from(self.n_bits))? {
            return Ok(None);
        }
        let rel = self.posbits - self.base_bits;
        let byte = (rel / 8) as usize;
        let shift = (rel % 8) as u32;
        let mut v = 0u32;
        for (i, b) in self.buf[byte..].iter().take(3).enumerate() {
            v |= u32::from(*b) << (8 * i);
        }
        self.posbits += u64::from(self.n_bits);
        Ok(Some(((v >> shift) & ((1 << self.n_bits) - 1)) as usize))
    }

    /// Decodes until some output is ready or the data ends.
    fn step(&mut self) -> io::Result<bool> {
        loop {
            if self.free_ent > self.maxcode && self.n_bits < self.maxbits {
                self.round_up();
                self.n_bits += 1;
                self.maxcode = if self.n_bits == self.maxbits { self.maxmaxcode } else { (1 << self.n_bits) - 1 };
            }
            let Some(code) = self.next_code()? else { return Ok(false) };
            let Some(old) = self.oldcode else {
                if code >= 256 {
                    return Err(bad("damaged data"));
                }
                self.oldcode = Some(code);
                self.finchar = code as u8;
                self.out.push(self.finchar);
                return Ok(true);
            };
            if code == CLEAR && self.block_mode {
                self.free_ent = 256;
                self.round_up();
                self.n_bits = INIT_BITS;
                self.maxcode = (1 << INIT_BITS) - 1;
                continue;
            }
            let incode = code;
            let mut code = code;
            let start = self.out.len();
            if code >= self.free_ent {
                if code > self.free_ent {
                    return Err(bad("damaged data"));
                }
                self.out.push(self.finchar);
                code = old;
            }
            let mut guard = 0usize;
            while code >= 256 {
                self.out.push(self.suffix[code]);
                code = usize::from(self.prefix[code]);
                guard += 1;
                if guard > self.maxmaxcode {
                    return Err(bad("damaged data"));
                }
            }
            self.finchar = self.suffix[code];
            self.out.push(self.finchar);
            self.out[start..].reverse();
            if self.free_ent < self.maxmaxcode {
                self.prefix[self.free_ent] = old as u16;
                self.suffix[self.free_ent] = self.finchar;
                self.free_ent += 1;
            }
            self.oldcode = Some(incode);
            return Ok(true);
        }
    }
}

impl<R: Read> Read for LzwReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.started {
            self.start()?;
        }
        if self.out_pos == self.out.len() {
            self.out.clear();
            self.out_pos = 0;
            while self.out.len() < 32 * 1024 {
                if !self.step()? {
                    break;
                }
            }
        }
        let n = (self.out.len() - self.out_pos).min(buf.len());
        buf[..n].copy_from_slice(&self.out[self.out_pos..self.out_pos + n]);
        self.out_pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "TOBEORNOTTOBEORTOBEORNOT#" compressed as `compress` does (checked with 7-Zip).
    const TOBE_Z: &[u8] = &[
        0x1f, 0x9d, 0x90, 0x54, 0x9e, 0x08, 0x29, 0xf2, 0x44, 0x8a, 0x93, 0x27, 0x54, 0x02, 0x0e, 0x2c, 0xa8, 0x90, 0xa0, 0x41, 0x84, 0x23, 0x00,
    ];

    #[test]
    fn decodes_compress_output() {
        let mut out = String::new();
        LzwReader::new(TOBE_Z).read_to_string(&mut out).unwrap();
        assert_eq!(out, "TOBEORNOTTOBEORTOBEORNOT#");
    }

    #[test]
    fn rejects_garbage() {
        let mut out = Vec::new();
        assert!(LzwReader::new(&b"\x1f\x9d\x90\xff\xff\xff"[..]).read_to_end(&mut out).is_err());
        assert!(LzwReader::new(&b"PK"[..]).read_to_end(&mut out).is_err());
    }
}
