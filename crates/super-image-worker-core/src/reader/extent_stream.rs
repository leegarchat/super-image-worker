use super::source::Image;
use crate::error::{Error, Result};
use crate::format::*;

pub struct ExtentReader<'a> {
    image: &'a mut Image,
    extents: Vec<Extent>,
    current_extent: usize,
    current_offset: u64,
    total_size: u64,
    position: u64,
}

impl<'a> ExtentReader<'a> {
    pub fn new(image: &'a mut Image, extents: Vec<Extent>) -> Self {
        let mut total_size: u64 = 0;
        for e in &extents {
            total_size = total_size.saturating_add(e.num_sectors.saturating_mul(LP_SECTOR_SIZE));
        }
        Self {
            image,
            extents,
            current_extent: 0,
            current_offset: 0,
            total_size,
            position: 0,
        }
    }

    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() || self.position >= self.total_size {
            return Ok(0);
        }

        let mut written = 0;
        while written < buf.len() && self.current_extent < self.extents.len() {
            let extent = match self.extents.get(self.current_extent) {
                Some(e) => e.clone(),
                None => break,
            };
            let extent_size = extent.num_sectors.saturating_mul(LP_SECTOR_SIZE);

            if extent_size == 0 {
                self.current_extent = self.current_extent.saturating_add(1);
                self.current_offset = 0;
                continue;
            }

            let remaining_in_extent = extent_size.saturating_sub(self.current_offset);
            if remaining_in_extent == 0 {
                self.current_extent = self.current_extent.saturating_add(1);
                self.current_offset = 0;
                continue;
            }

            let remaining_in_buf = (buf.len() - written) as u64;
            let to_read = remaining_in_extent.min(remaining_in_buf) as usize;
            if to_read == 0 {
                break;
            }

            let dest = buf
                .get_mut(written..written + to_read)
                .ok_or_else(|| Error::Invalid("extent reader buffer range error".into()))?;

            match extent.target_type {
                LP_TARGET_TYPE_LINEAR => {
                    let phys_offset = extent
                        .target_data
                        .checked_mul(LP_SECTOR_SIZE)
                        .and_then(|v| v.checked_add(self.current_offset))
                        .ok_or_else(|| Error::Invalid("physical offset overflow".into()))?;
                    self.image.read_at(phys_offset, dest)?;
                }
                LP_TARGET_TYPE_ZERO => {
                    dest.fill(0);
                }
                _ => {
                    return Err(Error::Invalid(format!(
                        "unsupported extent type: {}",
                        extent.target_type
                    )));
                }
            }

            written += to_read;
            self.current_offset = self.current_offset.saturating_add(to_read as u64);
            self.position = self.position.saturating_add(to_read as u64);

            if self.current_offset >= extent_size {
                self.current_extent = self.current_extent.saturating_add(1);
                self.current_offset = 0;
            }
        }

        Ok(written)
    }
}

pub fn extract_partition(
    image: &mut Image,
    extents: &[Extent],
    output: &mut dyn std::io::Write,
) -> Result<u64> {
    let reader_extents: Vec<Extent> = extents.to_vec();
    let mut reader = ExtentReader::new(image, reader_extents);
    let total = reader.total_size();

    // O(1) RAM: fixed 1 MiB streaming buffer.
    let mut buf = vec![0u8; 1024 * 1024];
    let mut written = 0u64;
    while written < total {
        let remaining = total.saturating_sub(written) as usize;
        let to_read = remaining.min(buf.len());
        if to_read == 0 {
            break;
        }
        let chunk = buf
            .get_mut(..to_read)
            .ok_or_else(|| Error::Invalid("extract buffer range error".into()))?;
        let n = reader.read(chunk)?;
        if n == 0 {
            break;
        }
        output
            .write_all(
                buf.get(..n)
                    .ok_or_else(|| Error::Invalid("extract output range error".into()))?,
            )
            .map_err(Error::Io)?;
        written = written.saturating_add(n as u64);
    }

    Ok(written)
}
