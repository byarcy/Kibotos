//! Format detection by content. Extensions are ignored: a `.cbr` that is
//! really a ZIP opens as ZIP.

use std::io::{self, BufReader, Read};
use std::path::Path;

use crate::volume::Source;
use crate::{Error, Result};

/// Single-stream compressors used alone or around a tar archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    Gzip,
    Bzip2,
    Xz,
    Zstd,
    Lz4,
    /// LZMA "alone" (`.lzma`), as written by LZMA Utils and `xz --format=lzma`.
    Lzma,
    Lzip,
    /// Unix `compress` (`.Z`).
    Compress,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::Gzip => "gzip",
            Codec::Bzip2 => "bzip2",
            Codec::Xz => "xz",
            Codec::Zstd => "zstd",
            Codec::Lz4 => "lz4",
            Codec::Lzma => "lzma",
            Codec::Lzip => "lzip",
            Codec::Compress => "Z",
        }
    }

    /// File extensions this codec usually adds.
    pub(crate) fn extensions(self) -> &'static [&'static str] {
        match self {
            Codec::Gzip => &["gz", "gzip"],
            Codec::Bzip2 => &["bz2", "bzip2"],
            Codec::Xz => &["xz"],
            Codec::Zstd => &["zst", "zstd"],
            Codec::Lz4 => &["lz4"],
            Codec::Lzma => &["lzma"],
            Codec::Lzip => &["lz"],
            Codec::Compress => &["z"],
        }
    }

    /// A decoder reading a whole file: `.xz` files made of several blocks
    /// are decoded on several threads.
    pub(crate) fn file_decoder(self, file: io::BufReader<Source>) -> io::Result<Box<dyn Read>> {
        match self {
            // Nothing has been read yet, so the buffer is empty.
            Codec::Xz => crate::xzmt::decoder(file.into_inner()),
            _ => self.decoder(file),
        }
    }

    /// Wraps a reader in the matching decoder.
    pub(crate) fn decoder<'a, R: Read + 'a>(self, r: R) -> io::Result<Box<dyn Read + 'a>> {
        Ok(match self {
            Codec::Gzip => Box::new(flate2::read::MultiGzDecoder::new(r)),
            Codec::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(r)),
            Codec::Xz => Box::new(lzma_rust2::XzReader::new(r, true)),
            Codec::Zstd => Box::new(zstd::stream::read::Decoder::new(r)?),
            Codec::Lz4 => Box::new(lz4_flex::frame::FrameDecoder::new(r)),
            Codec::Lzma => Box::new(lzma_rust2::LzmaReader::new_mem_limit(r, LZMA_MEM_LIMIT_KB, None)?),
            Codec::Lzip => Box::new(lzma_rust2::LzipReader::new(r)),
            Codec::Compress => Box::new(crate::lzw::LzwReader::new(r)),
        })
    }
}

/// Largest dictionary accepted for `.lzma` files (the decoder allocates it).
const LZMA_MEM_LIMIT_KB: u32 = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Format {
    Zip,
    SevenZip,
    Rar,
    /// A tar archive, optionally compressed (`.tar.gz`, `.tgz`, ...).
    Tar(Option<Codec>),
    /// A single compressed file that is not a tar archive.
    Compressed(Codec),
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Zip => "ZIP",
            Format::SevenZip => "7z",
            Format::Rar => "RAR",
            Format::Tar(None) => "tar",
            Format::Tar(Some(Codec::Gzip)) => "tar.gz",
            Format::Tar(Some(Codec::Bzip2)) => "tar.bz2",
            Format::Tar(Some(Codec::Xz)) => "tar.xz",
            Format::Tar(Some(Codec::Zstd)) => "tar.zst",
            Format::Tar(Some(Codec::Lz4)) => "tar.lz4",
            Format::Tar(Some(Codec::Lzma)) => "tar.lzma",
            Format::Tar(Some(Codec::Lzip)) => "tar.lz",
            Format::Tar(Some(Codec::Compress)) => "tar.Z",
            Format::Compressed(c) => c.name(),
        }
    }

    /// Whether Kibotos may ever create archives of this format. RAR is
    /// permanently read-only: the unRAR license forbids building a
    /// RAR-compatible compressor.
    pub fn can_write(self) -> bool {
        !matches!(self, Format::Rar)
    }
}

const ZIP_LOCAL: &[u8] = b"PK\x03\x04";
const ZIP_EMPTY: &[u8] = b"PK\x05\x06";
const ZIP_SPANNED: &[u8] = b"PK\x07\x08";
const SEVENZ: &[u8] = b"7z\xBC\xAF\x27\x1C";
const RAR4: &[u8] = b"Rar!\x1A\x07\x00";
const RAR5: &[u8] = b"Rar!\x1A\x07\x01\x00";
const GZIP: &[u8] = b"\x1F\x8B";
const BZIP2: &[u8] = b"BZh";
const XZ: &[u8] = b"\xFD7zXZ\x00";
const ZSTD: &[u8] = b"\x28\xB5\x2F\xFD";
const LZ4: &[u8] = b"\x04\x22\x4D\x18";
const LZIP: &[u8] = b"LZIP";
const COMPRESS: &[u8] = b"\x1F\x9D";

/// `.lzma` files have no signature. Like 7-Zip, accept a valid properties
/// byte, a dictionary size an encoder would write (2^n or 3 * 2^(n-1)) and
/// a plausible size; detection then also requires the data to decode.
fn looks_like_lzma(h: &[u8]) -> bool {
    // The range coder's first byte is always 0.
    if h.len() < 14 || h[0] >= 9 * 5 * 5 || h[13] != 0 {
        return false;
    }
    let dict = u32::from_le_bytes([h[1], h[2], h[3], h[4]]);
    let size = u64::from_le_bytes(h[5..13].try_into().unwrap());
    let dict_ok = (12..=30).any(|n| dict == 1 << n || dict == 3 << (n - 1));
    dict_ok && (size == u64::MAX || size < 1 << 48)
}

/// Detects the format of a file from its first bytes.
pub fn detect_path(path: &Path) -> Result<Format> {
    let mut file = Source::open(path)?;
    let mut head = [0u8; 512];
    let n = read_full(&mut file, &mut head)?;
    let head = &head[..n];

    let codec = if head.starts_with(GZIP) {
        Codec::Gzip
    } else if head.starts_with(BZIP2) && head.len() > 3 && (b'1'..=b'9').contains(&head[3]) {
        Codec::Bzip2
    } else if head.starts_with(XZ) {
        Codec::Xz
    } else if head.starts_with(ZSTD) {
        Codec::Zstd
    } else if head.starts_with(LZ4) {
        Codec::Lz4
    } else if head.starts_with(LZIP) && head.get(4) == Some(&1) {
        Codec::Lzip
    } else if head.starts_with(COMPRESS) {
        Codec::Compress
    } else if let Ok(f) = detect_container(head) {
        return Ok(f);
    } else if looks_like_lzma(head) {
        Codec::Lzma
    } else {
        return Err(Error::UnknownFormat);
    };

    // Peek at the start of the decompressed stream to tell tar.* from a
    // single compressed file.
    let file = Source::open(path)?;
    let mut dec = codec.decoder(BufReader::with_capacity(64 * 1024, file))?;
    let mut inner = [0u8; 512];
    let n = match read_full(&mut dec, &mut inner) {
        Ok(n) => n,
        // Without a signature, data that does not decode is not LZMA.
        Err(_) if codec == Codec::Lzma => return Err(Error::UnknownFormat),
        Err(_) => 0,
    };
    if codec == Codec::Lzma && n == 0 {
        return Err(Error::UnknownFormat);
    }
    if n == 512 && is_tar_header(&inner) {
        Ok(Format::Tar(Some(codec)))
    } else {
        Ok(Format::Compressed(codec))
    }
}

fn detect_container(head: &[u8]) -> Result<Format> {
    if head.starts_with(ZIP_LOCAL) || head.starts_with(ZIP_EMPTY) || head.starts_with(ZIP_SPANNED) {
        Ok(Format::Zip)
    } else if head.starts_with(SEVENZ) {
        Ok(Format::SevenZip)
    } else if head.starts_with(RAR4) || head.starts_with(RAR5) {
        Ok(Format::Rar)
    } else if head.len() == 512 && is_tar_header(head) {
        Ok(Format::Tar(None))
    } else {
        Err(Error::UnknownFormat)
    }
}

/// A tar header has "ustar" at offset 257 (POSIX/GNU) or, for old v7
/// archives, a valid header checksum.
pub(crate) fn is_tar_header(h: &[u8]) -> bool {
    if h.len() < 512 {
        return false;
    }
    if &h[257..262] == b"ustar" {
        return true;
    }
    let field = &h[148..156];
    let digits: String = field
        .iter()
        .skip_while(|&&b| b == b' ')
        .take_while(|&&b| b != 0 && b != b' ')
        .map(|&b| b as char)
        .collect();
    let Ok(stored) = u32::from_str_radix(&digits, 8) else {
        return false;
    };
    let sum: u32 = h[..512]
        .iter()
        .enumerate()
        .map(|(i, &b)| if (148..156).contains(&i) { b' ' as u32 } else { b as u32 })
        .sum();
    // An all-zero block sums to 256 and is not a header.
    stored == sum && sum != 256 && h[0] != 0
}

fn read_full(r: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}
