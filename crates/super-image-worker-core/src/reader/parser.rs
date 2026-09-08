use super::source::Image;
use crate::error::{Error, Result};
use crate::format::*;
use sha2::{Digest, Sha256};

fn read_u16(data: &[u8], offset: usize) -> Result<u16> {
    data.get(offset..offset + 2)
        .and_then(|v| v.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| Error::Invalid(format!("missing u16 at offset {offset}")))
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    data.get(offset..offset + 4)
        .and_then(|v| v.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| Error::Invalid(format!("missing u32 at offset {offset}")))
}

fn read_u64(data: &[u8], offset: usize) -> Result<u64> {
    data.get(offset..offset + 8)
        .and_then(|v| v.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| Error::Invalid(format!("missing u64 at offset {offset}")))
}

fn read_string(data: &[u8], offset: usize, len: usize) -> String {
    let end = offset.saturating_add(len).min(data.len());
    if offset > data.len() {
        return String::new();
    }
    let slice = match data.get(offset..end) {
        Some(s) => s,
        None => return String::new(),
    };
    let null_pos = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    String::from_utf8_lossy(slice.get(..null_pos).unwrap_or(&[])).into_owned()
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

pub struct LpParser;

impl Default for LpParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LpParser {
    pub fn new() -> Self {
        Self
    }

    /// Load geometry, validating SHA-256. Falls back from primary (0x1000)
    /// to backup (0x2000) if the primary is missing or corrupt.
    #[allow(clippy::collapsible_if)]
    pub fn find_geometry(&self, image: &mut Image) -> Result<(Geometry, u64)> {
        let mut last_err: Option<Error> = None;
        for &offset in &[GEOMETRY_PRIMARY_OFFSET, GEOMETRY_BACKUP_OFFSET] {
            if offset.saturating_add(0x34) > image.size() {
                continue;
            }
            let mut magic_buf = [0u8; 4];
            if image.read_at(offset, &mut magic_buf).is_err() {
                continue;
            }
            if u32::from_le_bytes(magic_buf) != GEOMETRY_MAGIC {
                last_err = Some(Error::Invalid(format!("no geometry magic at 0x{offset:x}")));
                continue;
            }
            match self.parse_geometry_validated(image, offset) {
                Ok(g) => return Ok(g),
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }
        // Legacy scan for relocated geometries (validation still enforced).
        let search_limit = image.size().min(0x10000);
        let mut offset = GEOMETRY_PRIMARY_OFFSET;
        let mut magic_buf = [0u8; 4];
        while offset < search_limit {
            if offset != GEOMETRY_PRIMARY_OFFSET
                && offset != GEOMETRY_BACKUP_OFFSET
                && image.read_at(offset, &mut magic_buf).is_ok()
                && u32::from_le_bytes(magic_buf) == GEOMETRY_MAGIC
            {
                if let Ok(g) = self.parse_geometry_validated(image, offset) {
                    return Ok(g);
                }
            }
            offset = offset.saturating_add(0x1000);
        }
        Err(last_err.unwrap_or_else(|| Error::NotFound("LP geometry not found".into())))
    }

    fn parse_geometry_validated(&self, image: &mut Image, offset: u64) -> Result<(Geometry, u64)> {
        let mut data = vec![0u8; 0x34];
        image.read_at(offset, &mut data)?;
        let magic = read_u32(&data, 0)?;
        if magic != GEOMETRY_MAGIC {
            return Err(Error::Invalid(format!(
                "invalid geometry magic: 0x{magic:08x}"
            )));
        }
        let stored = data
            .get(8..0x28)
            .ok_or_else(|| Error::Invalid("geometry checksum truncated".into()))?;
        let mut zeroed = data.clone();
        if let Some(slot) = zeroed.get_mut(8..0x28) {
            slot.fill(0);
        } else {
            return Err(Error::Invalid("geometry checksum range error".into()));
        }
        let calc = sha256(&zeroed);
        if calc.as_slice() != stored {
            return Err(Error::Invalid(format!(
                "geometry SHA-256 mismatch at 0x{offset:x} (corrupt)"
            )));
        }
        let max_size = read_u32(&data, 0x28)?;
        let slot_count = read_u32(&data, 0x2C)?;
        let block_size = read_u32(&data, 0x30)?;
        if max_size == 0 || max_size > 16 * 1024 * 1024 {
            return Err(Error::Invalid("invalid metadata_max_size".into()));
        }
        if slot_count == 0 || slot_count > 8 {
            return Err(Error::Invalid("invalid metadata_slot_count".into()));
        }
        if block_size != 4096 && block_size != 512 {
            return Err(Error::Invalid("invalid logical_block_size".into()));
        }
        Ok((
            Geometry {
                metadata_max_size: max_size,
                metadata_slot_count: slot_count,
                logical_block_size: block_size,
            },
            offset,
        ))
    }

    /// Load metadata for one explicit slot index: primary copy first,
    /// then the backup copy of the same slot (AOSP layout).
    /// Used for `--slot a|b` selection (slot 0 <-> `_a`, 1 <-> `_b`).
    #[allow(clippy::collapsible_if)]
    pub fn find_metadata_for_slot(
        &self,
        image: &mut Image,
        geometry: &Geometry,
        slot: u64,
    ) -> Result<(MetadataHeader, u64)> {
        let max = geometry.metadata_max_size as u64;
        let count = geometry.metadata_slot_count as u64;
        if slot >= count {
            return Err(Error::NotFound(format!(
                "metadata slot {slot} out of range (slot_count {count})"
            )));
        }
        if let Some(off) = primary_offset(slot, max) {
            if off.saturating_add(0x80) <= image.size() {
                if let Ok(res) = self.parse_metadata_validated(image, off, geometry) {
                    return Ok(res);
                }
            }
        }
        if let Some(off) = backup_offset(slot, count, max) {
            if off.saturating_add(0x80) <= image.size() {
                if let Ok(res) = self.parse_metadata_validated(image, off, geometry) {
                    return Ok(res);
                }
            }
        }
        Err(Error::NotFound(format!(
            "LP metadata slot {slot} not found (primary+backup invalid)"
        )))
    }

    /// Collect every valid metadata slot (slot index, header, offset).
    /// Invalid/empty slots (e.g. unused slot 2 on 3-slot geometries)
    /// are skipped.
    pub fn find_all_metadata(
        &self,
        image: &mut Image,
        geometry: &Geometry,
    ) -> Vec<(u64, MetadataHeader, u64)> {
        let count = geometry.metadata_slot_count as u64;
        let mut out = Vec::new();
        for slot in 0..count {
            if let Ok((header, offset)) = self.find_metadata_for_slot(image, geometry, slot)
            {
                out.push((slot, header, offset));
            }
        }
        out
    }

    /// Load metadata header, validating header SHA-256 and tables SHA-256.
    /// Tries every primary slot first, then every backup slot (AOSP order).
    pub fn find_metadata(
        &self,
        image: &mut Image,
        geometry: &Geometry,
    ) -> Result<(MetadataHeader, u64)> {
        let max = geometry.metadata_max_size as u64;
        let count = geometry.metadata_slot_count as u64;
        // Primary slots.
        for slot in 0..count {
            let off = match primary_offset(slot, max) {
                Some(o) => o,
                None => continue,
            };
            if off.saturating_add(0x80) > image.size() {
                break;
            }
            if let Ok(res) = self.parse_metadata_validated(image, off, geometry) {
                return Ok(res);
            }
        }
        // Backup slots.
        for slot in 0..count {
            let off = match backup_offset(slot, count, max) {
                Some(o) => o,
                None => continue,
            };
            if off.saturating_add(0x80) > image.size() {
                break;
            }
            if let Ok(res) = self.parse_metadata_validated(image, off, geometry) {
                return Ok(res);
            }
        }
        Err(Error::NotFound(
            "LP metadata header not found (all primary+backup slots invalid)".into(),
        ))
    }

    fn parse_metadata_validated(
        &self,
        image: &mut Image,
        offset: u64,
        geometry: &Geometry,
    ) -> Result<(MetadataHeader, u64)> {
        let (header, _) = self.parse_metadata_header(image, offset)?;
        // Validate header checksum: sha256(header with 0x0C..0x2C zeroed).
        let hlen = header.header_size as u64;
        if hlen < 0x80 || hlen > geometry.metadata_max_size as u64 {
            return Err(Error::Invalid("header_size out of range".into()));
        }
        let mut hraw = vec![0u8; hlen as usize];
        image.read_at(offset, &mut hraw)?;
        let stored_hdr = hraw
            .get(0x0C..0x2C)
            .ok_or_else(|| Error::Invalid("header checksum truncated".into()))?;
        let mut zeroed = hraw.clone();
        if let Some(slot) = zeroed.get_mut(0x0C..0x2C) {
            slot.fill(0);
        } else {
            return Err(Error::Invalid("header checksum range error".into()));
        }
        let calc_hdr = sha256(&zeroed);
        if calc_hdr.as_slice() != stored_hdr {
            return Err(Error::Invalid(format!(
                "metadata header SHA-256 mismatch at 0x{offset:x}"
            )));
        }
        // Validate tables checksum.
        let tables_size = header.tables_size as u64;
        if tables_size > geometry.metadata_max_size as u64 {
            return Err(Error::Invalid(
                "tables_size exceeds metadata_max_size".into(),
            ));
        }
        let tables_off = offset
            .checked_add(hlen)
            .ok_or_else(|| Error::Invalid("tables offset overflow".into()))?;
        let mut tables = vec![0u8; tables_size as usize];
        if !tables.is_empty() {
            image.read_at(tables_off, &mut tables)?;
        }
        let stored_tables = hraw
            .get(0x30..0x50)
            .ok_or_else(|| Error::Invalid("tables checksum truncated".into()))?;
        let calc_tables = sha256(&tables);
        if calc_tables.as_slice() != stored_tables {
            return Err(Error::Invalid(format!(
                "metadata tables SHA-256 mismatch at 0x{offset:x}"
            )));
        }
        Ok((header, offset))
    }

    fn parse_metadata_header(
        &self,
        image: &mut Image,
        offset: u64,
    ) -> Result<(MetadataHeader, u64)> {
        let mut data = vec![0u8; 0x80];
        image.read_at(offset, &mut data)?;
        let magic = read_u32(&data, 0)?;
        if magic != LP_METADATA_MAGIC {
            return Err(Error::Invalid(format!(
                "invalid metadata magic: 0x{magic:08x}"
            )));
        }
        let header_size = read_u32(&data, 8)?;
        let tables_size = read_u32(&data, 0x2C)?;
        if !(0x80..=1024 * 1024).contains(&header_size) {
            return Err(Error::Invalid("header_size out of range".into()));
        }
        if tables_size > 64 * 1024 * 1024 {
            return Err(Error::Invalid("tables_size too large".into()));
        }
        Ok((
            MetadataHeader {
                major_version: read_u16(&data, 4)?,
                minor_version: read_u16(&data, 6)?,
                header_size,
                tables_size,
                partitions: TableDescriptor {
                    offset: read_u32(&data, 0x50)?,
                    num_entries: read_u32(&data, 0x54)?,
                    entry_size: read_u32(&data, 0x58)?,
                },
                extents: TableDescriptor {
                    offset: read_u32(&data, 0x5C)?,
                    num_entries: read_u32(&data, 0x60)?,
                    entry_size: read_u32(&data, 0x64)?,
                },
                groups: TableDescriptor {
                    offset: read_u32(&data, 0x68)?,
                    num_entries: read_u32(&data, 0x6C)?,
                    entry_size: read_u32(&data, 0x70)?,
                },
                block_devices: TableDescriptor {
                    offset: read_u32(&data, 0x74)?,
                    num_entries: read_u32(&data, 0x78)?,
                    entry_size: read_u32(&data, 0x7C)?,
                },
            },
            offset,
        ))
    }

    fn read_table<T, F>(
        &self,
        image: &mut Image,
        desc: &TableDescriptor,
        metadata_offset: u64,
        header_size: u32,
        parse: F,
    ) -> Result<Vec<T>>
    where
        F: Fn(&[u8]) -> Result<T>,
    {
        if desc.entry_size == 0 {
            if desc.num_entries == 0 {
                return Ok(Vec::new());
            }
            return Err(Error::Invalid("entry size is zero".into()));
        }
        if desc.entry_size > 1024 * 1024 {
            return Err(Error::Invalid("entry size too large".into()));
        }
        if desc.num_entries > 65536 {
            return Err(Error::Invalid("table entry count too large".into()));
        }
        let base = metadata_offset
            .checked_add(header_size as u64)
            .and_then(|v| v.checked_add(desc.offset as u64))
            .ok_or_else(|| Error::Invalid("table base overflow".into()))?;
        let mut result = Vec::with_capacity(desc.num_entries as usize);
        for i in 0..desc.num_entries {
            let entry_offset = base
                .checked_add(
                    (i as u64)
                        .checked_mul(desc.entry_size as u64)
                        .ok_or_else(|| Error::Invalid("table entry offset overflow".into()))?,
                )
                .ok_or_else(|| Error::Invalid("table entry offset overflow".into()))?;
            let entry_end = entry_offset
                .checked_add(desc.entry_size as u64)
                .ok_or_else(|| Error::Invalid("table entry end overflow".into()))?;
            if entry_end > image.size() {
                return Err(Error::Invalid("table entry beyond image".into()));
            }
            let mut data = vec![0u8; desc.entry_size as usize];
            image.read_at(entry_offset, &mut data)?;
            result.push(parse(&data)?);
        }
        Ok(result)
    }

    pub fn read_partitions(
        &self,
        image: &mut Image,
        header: &MetadataHeader,
        metadata_offset: u64,
    ) -> Result<Vec<Partition>> {
        self.read_table(
            image,
            &header.partitions,
            metadata_offset,
            header.header_size,
            |data| {
                Ok(Partition {
                    name: read_string(data, 0, 36),
                    attributes: read_u32(data, 0x24)?,
                    first_extent_index: read_u32(data, 0x28)?,
                    num_extents: read_u32(data, 0x2C)?,
                    group_index: read_u32(data, 0x30)?,
                })
            },
        )
    }

    pub fn read_extents(
        &self,
        image: &mut Image,
        header: &MetadataHeader,
        metadata_offset: u64,
    ) -> Result<Vec<Extent>> {
        self.read_table(
            image,
            &header.extents,
            metadata_offset,
            header.header_size,
            |data| {
                Ok(Extent {
                    num_sectors: read_u64(data, 0)?,
                    target_type: read_u32(data, 0x08)?,
                    target_data: read_u64(data, 0x0C)?,
                    target_source: read_u32(data, 0x14)?,
                })
            },
        )
    }

    pub fn read_groups(
        &self,
        image: &mut Image,
        header: &MetadataHeader,
        metadata_offset: u64,
    ) -> Result<Vec<Group>> {
        self.read_table(
            image,
            &header.groups,
            metadata_offset,
            header.header_size,
            |data| {
                Ok(Group {
                    name: read_string(data, 0, 36),
                    flags: read_u32(data, 0x24)?,
                    maximum_size: read_u64(data, 0x28)?,
                })
            },
        )
    }

    pub fn read_block_devices(
        &self,
        image: &mut Image,
        header: &MetadataHeader,
        metadata_offset: u64,
    ) -> Result<Vec<BlockDevice>> {
        self.read_table(
            image,
            &header.block_devices,
            metadata_offset,
            header.header_size,
            |data| {
                Ok(BlockDevice {
                    first_logical_sector: read_u64(data, 0)?,
                    alignment: read_u32(data, 0x08)?,
                    alignment_offset: read_u32(data, 0x0C)?,
                    size: read_u64(data, 0x10)?,
                    partition_name: read_string(data, 0x18, 36),
                })
            },
        )
    }
}
