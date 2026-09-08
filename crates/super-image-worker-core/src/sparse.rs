use crate::error::{Error, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const SPARSE_MAGIC: u32 = 0xed26_ff3a;
const CHUNK_RAW: u16 = 0xcac1;
const CHUNK_DONT_CARE: u16 = 0xcac3;

/// Create (or truncate) a file with the exact `size`, O(1) time/RAM.
/// Uses `set_len`; the filesystem is free to keep it sparse.
pub fn create_sized_file(path: &Path, size: u64) -> Result<File> {
    let file = File::create(path)?;
    file.set_len(size)?;
    Ok(file)
}

fn write_u16_le(out: &mut File, v: u16) -> Result<()> {
    out.write_all(&v.to_le_bytes()).map_err(Error::Io)
}

fn write_u32_le(out: &mut File, v: u32) -> Result<()> {
    out.write_all(&v.to_le_bytes()).map_err(Error::Io)
}

fn write_chunk_header(out: &mut File, kind: u16, chunk_blocks: u32, total_sz: u32) -> Result<()> {
    write_u16_le(out, kind)?; // chunk_type
    write_u16_le(out, 0)?; // reserved
    write_u32_le(out, chunk_blocks)?; // chunk_sz (in blocks)
    write_u32_le(out, total_sz)?; // total_sz (header + data)
    Ok(())
}

/// Convert a flat raw image to an Android Sparse container, streaming with
/// O(1) RAM. Zero blocks become `DONT_CARE` chunks, the rest `RAW` chunks.
/// `block_size` is typically 4096 (logical block size).
pub fn raw_to_sparse(raw_path: &Path, sparse_path: &Path, block_size: u32) -> Result<()> {
    if !(512..=1024 * 1024).contains(&block_size) {
        return Err(Error::Invalid("invalid sparse block size".into()));
    }
    let bs = block_size as u64;
    let mut raw = File::open(raw_path)?;
    let raw_len = raw.metadata()?.len();
    if raw_len == 0 {
        return Err(Error::Invalid("raw image is empty".into()));
    }
    if raw_len % bs != 0 {
        return Err(Error::Invalid(format!(
            "raw size {raw_len} is not a multiple of block size {block_size}"
        )));
    }
    let total_blocks = raw_len / bs;
    if total_blocks > u32::MAX as u64 {
        return Err(Error::Invalid("image too large for sparse format".into()));
    }

    let mut out = File::create(sparse_path)?;
    // Header with total_chunks patched at the end.
    out.write_all(&SPARSE_MAGIC.to_le_bytes())
        .map_err(Error::Io)?;
    write_u16_le(&mut out, 1)?; // major
    write_u16_le(&mut out, 0)?; // minor
    write_u16_le(&mut out, 28)?; // file_hdr_sz
    write_u16_le(&mut out, 12)?; // chunk_hdr_sz
    write_u32_le(&mut out, block_size)?; // blk_sz
    write_u32_le(&mut out, total_blocks as u32)?; // total_blks
    write_u32_le(&mut out, 0)?; // total_chunks (patched later)
    write_u32_le(&mut out, 0)?; // image_checksum (crc32, unused -> 0)

    // Stream in 4 MiB windows; classify per block.
    let window_blocks = (4 * 1024 * 1024u64 / bs).clamp(1, 1 << 20);
    let window_bytes = window_blocks.saturating_mul(bs) as usize;
    let mut window = vec![0u8; window_bytes];
    // Accumulate one RAW run (capped to keep RAM O(1)).
    const MAX_RAW_RUN: usize = 8 * 1024 * 1024;
    let mut raw_run: Vec<u8> = Vec::new();
    let mut raw_run_blocks: u64 = 0;
    let mut care_run_blocks: u64 = 0;
    let mut total_chunks: u32 = 0;

    let flush_raw = |out: &mut File,
                     raw_run: &mut Vec<u8>,
                     raw_run_blocks: &mut u64,
                     total_chunks: &mut u32|
     -> Result<()> {
        if *raw_run_blocks == 0 {
            return Ok(());
        }
        if *raw_run_blocks > u32::MAX as u64 {
            return Err(Error::Invalid("sparse raw run too large".into()));
        }
        let data_len = raw_run.len();
        let total_sz = (12u64).saturating_add(data_len as u64);
        if total_sz > u32::MAX as u64 {
            return Err(Error::Invalid("sparse chunk too large".into()));
        }
        write_chunk_header(out, CHUNK_RAW, *raw_run_blocks as u32, total_sz as u32)?;
        out.write_all(raw_run).map_err(Error::Io)?;
        *total_chunks = total_chunks.saturating_add(1);
        raw_run.clear();
        *raw_run_blocks = 0;
        Ok(())
    };

    let flush_care =
        |out: &mut File, care_run_blocks: &mut u64, total_chunks: &mut u32| -> Result<()> {
            let mut left = *care_run_blocks;
            while left > 0 {
                let n = left.min(u32::MAX as u64) as u32;
                write_chunk_header(out, CHUNK_DONT_CARE, n, 12)?;
                *total_chunks = total_chunks.saturating_add(1);
                left -= n as u64;
            }
            *care_run_blocks = 0;
            Ok(())
        };

    let mut remaining = raw_len;
    while remaining > 0 {
        let want = (remaining.min(window.len() as u64)) as usize;
        let slot = window
            .get_mut(..want)
            .ok_or_else(|| Error::Invalid("sparse window range error".into()))?;
        raw.read_exact(slot).map_err(Error::Io)?;
        remaining -= want as u64;
        let blocks_here = want as u64 / bs;
        for b in 0..blocks_here {
            let start = (b.saturating_mul(bs)) as usize;
            let end = start.saturating_add(bs as usize);
            let block = slot
                .get(start..end)
                .ok_or_else(|| Error::Invalid("sparse block range error".into()))?;
            let is_zero = block.iter().all(|x| *x == 0);
            if is_zero {
                if raw_run_blocks > 0 {
                    flush_raw(
                        &mut out,
                        &mut raw_run,
                        &mut raw_run_blocks,
                        &mut total_chunks,
                    )?;
                }
                care_run_blocks += 1;
            } else {
                if care_run_blocks > 0 {
                    flush_care(&mut out, &mut care_run_blocks, &mut total_chunks)?;
                }
                raw_run.extend_from_slice(block);
                raw_run_blocks += 1;
                if raw_run.len() >= MAX_RAW_RUN {
                    flush_raw(
                        &mut out,
                        &mut raw_run,
                        &mut raw_run_blocks,
                        &mut total_chunks,
                    )?;
                }
            }
        }
    }
    flush_raw(
        &mut out,
        &mut raw_run,
        &mut raw_run_blocks,
        &mut total_chunks,
    )?;
    flush_care(&mut out, &mut care_run_blocks, &mut total_chunks)?;

    // Patch total_chunks into the header.
    out.seek(SeekFrom::Start(20)).map_err(Error::Io)?;
    write_u32_le(&mut out, total_chunks)?;
    out.flush().map_err(Error::Io)?;
    Ok(())
}
