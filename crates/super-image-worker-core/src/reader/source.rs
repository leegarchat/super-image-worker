use crate::error::{Error, Result};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

const SPARSE_MAGIC: u32 = 0xed26_ff3a;

pub trait BlockSource: Read + Seek {
    fn total_size(&self) -> u64;
    fn format_name(&self) -> &str;
}

#[derive(Debug)]
#[doc(hidden)]
pub struct SparseChunk {
    kind: u16,
    logical_start: u64,
    logical_len: u64,
    file_offset: u64,
    fill_value: [u8; 4],
}

#[derive(Debug)]
pub enum Image {
    Raw {
        file: File,
        size: u64,
    },
    Sparse {
        file: File,
        block_size: u64,
        size: u64,
        chunks: Vec<SparseChunk>,
    },
}

fn u16_le(b: &[u8]) -> Result<u16> {
    b.try_into()
        .map(u16::from_le_bytes)
        .map_err(|_| Error::Invalid("truncated u16 in sparse header".into()))
}

fn u32_le(b: &[u8]) -> Result<u32> {
    b.try_into()
        .map(u32::from_le_bytes)
        .map_err(|_| Error::Invalid("truncated u32 in sparse header".into()))
}

impl Image {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let file_size = file.metadata()?.len();
        let mut magic = [0u8; 4];
        if let Err(e) = file.read_exact(&mut magic) {
            return Err(Error::Io(e));
        }
        file.seek(SeekFrom::Start(0))?;
        if u32::from_le_bytes(magic) == SPARSE_MAGIC {
            Self::parse_sparse(file)
        } else {
            Ok(Self::Raw {
                file,
                size: file_size,
            })
        }
    }

    fn parse_sparse(mut file: File) -> Result<Self> {
        let mut header = [0u8; 28];
        file.read_exact(&mut header)?;
        let major = u16::from_le_bytes([header[4], header[5]]);
        let file_header_size = u16_le(&header[8..10])? as u64;
        let chunk_header_size = u16_le(&header[10..12])? as u64;
        let block_size = u32_le(&header[12..16])? as u64;
        let total_blocks = u32_le(&header[16..20])? as u64;
        let total_chunks = u32_le(&header[20..24])?;
        if major != 1 || file_header_size < 28 || chunk_header_size < 12 || block_size == 0 {
            return Err(Error::Invalid("unsupported sparse header".into()));
        }
        if total_chunks > 1_000_000 {
            return Err(Error::Invalid("sparse chunk count too large".into()));
        }
        file.seek(SeekFrom::Start(file_header_size))?;
        let capacity = (total_chunks as usize).min(65536);
        let mut chunks = Vec::with_capacity(capacity);
        let mut logical_start = 0u64;
        for index in 0..total_chunks {
            let mut chunk_header = vec![0u8; chunk_header_size as usize];
            if chunk_header.len() > 1024 {
                return Err(Error::Invalid(format!("chunk {index} header too large")));
            }
            file.read_exact(&mut chunk_header)?;
            let kind = u16_le(
                chunk_header
                    .get(0..2)
                    .ok_or_else(|| Error::Invalid(format!("chunk {index} truncated kind")))?,
            )?;
            let chunk_blocks = u32_le(
                chunk_header
                    .get(4..8)
                    .ok_or_else(|| Error::Invalid(format!("chunk {index} truncated blocks")))?,
            )? as u64;
            let total_size = u32_le(
                chunk_header
                    .get(8..12)
                    .ok_or_else(|| Error::Invalid(format!("chunk {index} truncated size")))?,
            )? as u64;
            let logical_len = chunk_blocks
                .checked_mul(block_size)
                .ok_or_else(|| Error::Invalid(format!("chunk {index} length overflow")))?;
            let data_size = total_size
                .checked_sub(chunk_header_size)
                .ok_or_else(|| Error::Invalid(format!("chunk {index} has invalid size")))?;
            let file_offset = file.stream_position()?;
            let fill_value = if kind == 0xcac2 {
                if data_size != 4 {
                    return Err(Error::Invalid(format!("chunk {index} FILL size")));
                }
                let mut v = [0u8; 4];
                file.read_exact(&mut v)?;
                v
            } else {
                [0; 4]
            };
            match kind {
                0xcac1 => {
                    if data_size != logical_len {
                        return Err(Error::Invalid(format!("chunk {index} RAW size")));
                    }
                    // Guard against seeking past EOF on corrupt headers.
                    file.seek(SeekFrom::Current(data_size as i64))?;
                }
                0xcac3 => {
                    if data_size != 0 {
                        return Err(Error::Invalid(format!("chunk {index} DONT_CARE size")));
                    }
                }
                0xcac4 => {
                    // CRC32 chunk: 4-byte checksum value follows header (ignored),
                    // or zero-length on some producers. Both are valid.
                    if data_size != 0 && data_size != 4 {
                        return Err(Error::Invalid(format!("chunk {index} CRC size")));
                    }
                    if data_size > 0 {
                        file.seek(SeekFrom::Current(data_size as i64))?;
                    }
                }
                0xcac2 => {}
                _ => return Err(Error::Invalid(format!("chunk {index} type 0x{kind:04x}"))),
            }
            chunks.push(SparseChunk {
                kind,
                logical_start,
                logical_len,
                file_offset,
                fill_value,
            });
            logical_start = logical_start
                .checked_add(logical_len)
                .ok_or_else(|| Error::Invalid("logical image size overflow".into()))?;
        }
        let expected_size = total_blocks
            .checked_mul(block_size)
            .ok_or_else(|| Error::Invalid("sparse image size overflow".into()))?;
        if logical_start != expected_size {
            return Err(Error::Invalid("chunk blocks do not match header".into()));
        }
        Ok(Self::Sparse {
            file,
            block_size,
            size: expected_size,
            chunks,
        })
    }

    pub fn size(&self) -> u64 {
        match self {
            Self::Raw { size, .. } | Self::Sparse { size, .. } => *size,
        }
    }

    pub fn format_name(&self) -> &'static str {
        match self {
            Self::Raw { .. } => "raw",
            Self::Sparse { .. } => "android-sparse",
        }
    }

    /// O(log N) chunk lookup by logical offset via binary search.
    fn find_chunk(chunks: &[SparseChunk], position: u64) -> Option<usize> {
        let mut lo = 0usize;
        let mut hi = chunks.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let c = chunks.get(mid)?;
            let end = c.logical_start.saturating_add(c.logical_len);
            if position < c.logical_start {
                hi = mid;
            } else if position >= end {
                lo = mid + 1;
            } else {
                return Some(mid);
            }
        }
        None
    }

    pub fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(buffer.len() as u64)
            .ok_or_else(|| Error::Invalid("read range overflow".into()))?;
        if end > self.size() {
            return Err(Error::Invalid(format!(
                "read range 0x{offset:x}..0x{end:x} exceeds image"
            )));
        }
        match self {
            Self::Raw { file, .. } => {
                file.seek(SeekFrom::Start(offset))?;
                file.read_exact(buffer)?;
            }
            Self::Sparse { file, chunks, .. } => {
                let mut position = offset;
                let mut written = 0usize;
                while written < buffer.len() {
                    let idx = Self::find_chunk(chunks, position).ok_or_else(|| {
                        Error::Invalid(format!("no sparse chunk for offset 0x{position:x}"))
                    })?;
                    let chunk = chunks
                        .get(idx)
                        .ok_or_else(|| Error::Invalid("sparse chunk index out of range".into()))?;
                    let chunk_end = chunk.logical_start.saturating_add(chunk.logical_len);
                    let available = chunk_end.saturating_sub(position) as usize;
                    if available == 0 {
                        return Err(Error::Invalid("zero-length sparse chunk".into()));
                    }
                    let count = available.min(buffer.len() - written);
                    let dest = buffer
                        .get_mut(written..written + count)
                        .ok_or_else(|| Error::Invalid("output buffer range error".into()))?;
                    match chunk.kind {
                        0xcac1 => {
                            let inner = position
                                .checked_sub(chunk.logical_start)
                                .ok_or_else(|| Error::Invalid("sparse offset underflow".into()))?;
                            let file_pos =
                                chunk.file_offset.checked_add(inner).ok_or_else(|| {
                                    Error::Invalid("sparse file offset overflow".into())
                                })?;
                            file.seek(SeekFrom::Start(file_pos))?;
                            file.read_exact(dest)?;
                        }
                        0xcac2 => {
                            for (i, byte) in dest.iter_mut().enumerate() {
                                let pos = position.checked_add(i as u64).ok_or_else(|| {
                                    Error::Invalid("sparse fill offset overflow".into())
                                })?;
                                let fill_idx = (pos % 4) as usize;
                                if let Some(v) = chunk.fill_value.get(fill_idx) {
                                    *byte = *v;
                                } else {
                                    return Err(Error::Invalid("fill index out of range".into()));
                                }
                            }
                        }
                        0xcac3 | 0xcac4 => dest.fill(0),
                        _ => {
                            return Err(Error::Invalid(format!(
                                "unsupported sparse chunk 0x{:04x}",
                                chunk.kind
                            )));
                        }
                    }
                    position = position.saturating_add(count as u64);
                    written = written.saturating_add(count);
                }
            }
        }
        Ok(())
    }
}
