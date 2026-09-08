use crate::error::{Error, Result};
use crate::format::*;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

pub struct LpWriter;

impl LpWriter {
    pub fn new() -> Self {
        Self
    }

    /// Align a sector candidate up to the device geometry (public for `make`,
    /// which must align the metadata reservation itself).
    ///
    /// honours both `alignment` and `alignment_offset` (both in bytes, per
    /// AOSP liblp): the allocation start `s` satisfies
    /// `(s * LP_SECTOR_SIZE) % alignment == alignment_offset % alignment`,
    /// so payload lands on the right erase-block phase even on eMMC/UFS
    /// chips with exotic geometry. When both are sector multiples (the
    /// universal real-world case) the result is exact; for exotic
    /// sub-sector granularity it degrades to the legacy sector rounding.
    pub fn align_up(v: u64, alignment: u32, alignment_offset: u32) -> Option<u64> {
        if alignment == 0 {
            return Some(v);
        }
        let alignment = alignment as u64;
        let offset = (alignment_offset as u64) % alignment;
        if alignment.is_multiple_of(LP_SECTOR_SIZE) && offset.is_multiple_of(LP_SECTOR_SIZE) {
            let phys = v.checked_mul(LP_SECTOR_SIZE)?;
            let rem = phys % alignment;
            let shift = if rem <= offset {
                offset - rem
            } else {
                alignment + offset - rem
            };
            // `shift` is a multiple of LP_SECTOR_SIZE by construction
            // (all three terms are), so the division is exact.
            v.checked_add(shift / LP_SECTOR_SIZE)
        } else {
            let sectors = alignment.saturating_add(LP_SECTOR_SIZE - 1) / LP_SECTOR_SIZE;
            if sectors == 0 {
                return Some(v);
            }
            let rem = v.checked_rem(sectors)?;
            if rem == 0 {
                Some(v)
            } else {
                v.checked_add(sectors.checked_sub(rem)?)
            }
        }
    }

    /// Sorted `(start, end)` sector ranges of linear extents on one device.
    fn collect_used(extents: &[Extent], device_index: u32) -> Result<Vec<(u64, u64)>> {
        let mut used: Vec<(u64, u64)> = Vec::new();
        for e in extents {
            if e.target_type != LP_TARGET_TYPE_LINEAR || e.target_source != device_index {
                continue;
            }
            let end = e
                .target_data
                .checked_add(e.num_sectors)
                .ok_or_else(|| Error::Invalid("extent range overflow".into()))?;
            used.push((e.target_data, end));
        }
        used.sort_by_key(|&(start, _)| start);
        Ok(used)
    }

    /// Find free sectors on a block device, honouring alignment,
    /// alignment_offset and the hard device-size limit.
    /// Returns Invalid("not enough free space...") when the allocation
    /// would exceed the device.
    pub fn find_free_sectors(
        &self,
        extents: &[Extent],
        devices: &[BlockDevice],
        device_index: u32,
        needed_sectors: u64,
    ) -> Result<u64> {
        if needed_sectors == 0 {
            return Err(Error::Invalid("requested zero sectors".into()));
        }
        let device = devices
            .get(device_index as usize)
            .ok_or_else(|| Error::Invalid("block device not found".into()))?;

        let first_sector = device.first_logical_sector;

        // Hard disk boundary: device.size is in bytes.
        let limit_sectors = device.size.checked_div(LP_SECTOR_SIZE).unwrap_or(0);

        let used = Self::collect_used(extents, device_index)?;

        let mut candidate = Self::align_up(first_sector, device.alignment, device.alignment_offset)
            .ok_or_else(|| Error::Invalid("alignment overflow".into()))?;

        for &(start, end) in &used {
            let alloc_end = candidate
                .checked_add(needed_sectors)
                .ok_or_else(|| Error::Invalid("not enough free space on block device".into()))?;
            if alloc_end <= start {
                if limit_sectors > 0 && alloc_end > limit_sectors {
                    return Err(Error::Invalid(
                        "not enough free space on block device".into(),
                    ));
                }
                return Ok(candidate);
            }
            if end > candidate {
                candidate = Self::align_up(end, device.alignment, device.alignment_offset)
                    .ok_or_else(|| Error::Invalid("alignment overflow".into()))?;
            }
        }

        let alloc_end = candidate
            .checked_add(needed_sectors)
            .ok_or_else(|| Error::Invalid("not enough free space on block device".into()))?;
        if limit_sectors > 0 && alloc_end > limit_sectors {
            return Err(Error::Invalid(
                "not enough free space on block device".into(),
            ));
        }
        Ok(candidate)
    }

    /// Largest contiguous free run (in sectors) on one block device,
    /// respecting alignment/offset and the size limit. Used to carve a
    /// payload into a multi-device extent chain.
    pub fn largest_free_run(
        &self,
        extents: &[Extent],
        devices: &[BlockDevice],
        device_index: u32,
    ) -> Result<u64> {
        let device = devices
            .get(device_index as usize)
            .ok_or_else(|| Error::Invalid("block device not found".into()))?;
        let used = Self::collect_used(extents, device_index)?;
        let limit_sectors = device.size.checked_div(LP_SECTOR_SIZE).unwrap_or(0);

        let mut candidate = Self::align_up(
            device.first_logical_sector,
            device.alignment,
            device.alignment_offset,
        )
        .ok_or_else(|| Error::Invalid("alignment overflow".into()))?;
        let mut best: u64 = 0;

        for &(start, end) in &used {
            if candidate < start {
                let gap_end = if limit_sectors > 0 {
                    start.min(limit_sectors)
                } else {
                    start
                };
                if gap_end > candidate {
                    best = best.max(gap_end - candidate);
                }
            }
            if end > candidate {
                candidate = Self::align_up(end, device.alignment, device.alignment_offset)
                    .ok_or_else(|| Error::Invalid("alignment overflow".into()))?;
                if limit_sectors > 0 && candidate >= limit_sectors {
                    break;
                }
            }
        }
        // Tail past the last extent.
        if limit_sectors == 0 {
            // Unknown device size: only inter-extent gaps are measurable.
            // (Real metadata always carries a size; this is degenerate input.)
        } else if candidate < limit_sectors {
            best = best.max(limit_sectors - candidate);
        }
        Ok(best)
    }

    /// First-fit allocation across ALL block devices (split/retrofit aware).
    /// Returns `(device_index, sector)`. Devices are tried in table order.
    pub fn find_free_sectors_any(
        &self,
        extents: &[Extent],
        devices: &[BlockDevice],
        needed_sectors: u64,
    ) -> Result<(u32, u64)> {
        if devices.is_empty() {
            return Err(Error::Invalid("no block devices in metadata".into()));
        }
        let allowed: Vec<u32> = (0..devices.len() as u32).collect();
        self.find_free_sectors_any_in(extents, devices, needed_sectors, &allowed)
    }

    /// First-fit allocation restricted to `allowed` device indices (tried in
    /// table order). Lets callers allocate only on files actually bound on
    /// the command line, so a missing `--device` fails fast with a clear
    /// hint instead of landing payload on an unbound device.
    pub fn find_free_sectors_any_in(
        &self,
        extents: &[Extent],
        devices: &[BlockDevice],
        needed_sectors: u64,
        allowed: &[u32],
    ) -> Result<(u32, u64)> {
        if devices.is_empty() {
            return Err(Error::Invalid("no block devices in metadata".into()));
        }
        if allowed.is_empty() {
            return Err(Error::Invalid(
                "no block devices bound (pass --device <name>=<path> for secondary devices)".into(),
            ));
        }
        let mut last_err: Option<Error> = None;
        // Table order for a stable layout, skipping disallowed indices.
        for idx in 0..devices.len() {
            let idx = idx as u32;
            if !allowed.contains(&idx) {
                continue;
            }
            match self.find_free_sectors(extents, devices, idx, needed_sectors) {
                Ok(sector) => return Ok((idx, sector)),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err
            .unwrap_or_else(|| Error::Invalid("not enough free space on block device".into())))
    }

    /// Plan an allocation of `needed_sectors`, possibly spanning several
    /// block devices. Returns `(device_index, start_sector, length)` runs in
    /// allocation order.
    ///
    /// Fast path: the whole payload fits on one device (previous behaviour,
    /// single extent). Slow path: the payload is carved into per-device
    /// chunks (table order, each `largest_free_run`-capped), so a partition
    /// larger than any single device still packs as long as the total free
    /// space suffices.
    pub fn plan_spanning_allocation(
        &self,
        extents: &[Extent],
        devices: &[BlockDevice],
        needed_sectors: u64,
    ) -> Result<Vec<(u32, u64, u64)>> {
        if needed_sectors == 0 {
            return Err(Error::Invalid("requested zero sectors".into()));
        }
        if devices.is_empty() {
            return Err(Error::Invalid("no block devices in metadata".into()));
        }
        if let Ok((src, sector)) = self.find_free_sectors_any(extents, devices, needed_sectors) {
            return Ok(vec![(src, sector, needed_sectors)]);
        }
        let mut plan: Vec<(u32, u64, u64)> = Vec::new();
        let mut working: Vec<Extent> = extents.to_vec();
        let mut remaining = needed_sectors;
        while remaining > 0 {
            let mut progress = false;
            for idx in 0..devices.len() {
                if remaining == 0 {
                    break;
                }
                let idx = idx as u32;
                let run = self.largest_free_run(&working, devices, idx)?;
                if run == 0 {
                    continue;
                }
                let take = remaining.min(run);
                let sector = self.find_free_sectors(&working, devices, idx, take)?;
                plan.push((idx, sector, take));
                working.push(Extent {
                    num_sectors: take,
                    target_type: LP_TARGET_TYPE_LINEAR,
                    target_data: sector,
                    target_source: idx,
                });
                remaining = remaining.saturating_sub(take);
                progress = true;
            }
            if !progress {
                return Err(Error::Invalid(format!(
                    "not enough free space on block device(s): need {needed_sectors} sectors, {} short",
                    remaining
                )));
            }
        }
        Ok(plan)
    }

    /// Check whether [sector, sector+len) is free on the device
    /// (no overlap with existing linear extents) and inside the disk.
    pub fn is_range_free(
        &self,
        extents: &[Extent],
        devices: &[BlockDevice],
        device_index: u32,
        sector: u64,
        len: u64,
    ) -> Result<bool> {
        let device = devices
            .get(device_index as usize)
            .ok_or_else(|| Error::Invalid("block device not found".into()))?;
        let limit = device.size.checked_div(LP_SECTOR_SIZE).unwrap_or(0);
        let end = sector
            .checked_add(len)
            .ok_or_else(|| Error::Invalid("sector range overflow".into()))?;
        if limit > 0 && end > limit {
            return Ok(false);
        }
        for e in extents {
            if e.target_type != LP_TARGET_TYPE_LINEAR || e.target_source != device_index {
                continue;
            }
            let e_end = e.target_data.saturating_add(e.num_sectors);
            if sector < e_end && e.target_data < end {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn serialize_partition(p: &Partition) -> Result<Vec<u8>> {
        let mut data = vec![0u8; 52];
        let name_bytes = p.name.as_bytes();
        if name_bytes.len() > 36 {
            return Err(Error::Invalid(format!(
                "partition name too long: {}",
                p.name
            )));
        }
        let copy_len = name_bytes.len().min(36);
        if let Some(dst) = data.get_mut(..copy_len) {
            dst.copy_from_slice(
                name_bytes
                    .get(..copy_len)
                    .ok_or_else(|| Error::Invalid("partition name range error".into()))?,
            );
        } else {
            return Err(Error::Invalid("partition buffer range error".into()));
        }
        data.get_mut(0x24..0x28)
            .ok_or_else(|| Error::Invalid("partition field range".into()))?
            .copy_from_slice(&p.attributes.to_le_bytes());
        data.get_mut(0x28..0x2C)
            .ok_or_else(|| Error::Invalid("partition field range".into()))?
            .copy_from_slice(&p.first_extent_index.to_le_bytes());
        data.get_mut(0x2C..0x30)
            .ok_or_else(|| Error::Invalid("partition field range".into()))?
            .copy_from_slice(&p.num_extents.to_le_bytes());
        data.get_mut(0x30..0x34)
            .ok_or_else(|| Error::Invalid("partition field range".into()))?
            .copy_from_slice(&p.group_index.to_le_bytes());
        Ok(data)
    }

    fn serialize_extent(e: &Extent) -> Vec<u8> {
        let mut data = vec![0u8; 24];
        data[0x00..0x08].copy_from_slice(&e.num_sectors.to_le_bytes());
        data[0x08..0x0C].copy_from_slice(&e.target_type.to_le_bytes());
        data[0x0C..0x14].copy_from_slice(&e.target_data.to_le_bytes());
        data[0x14..0x18].copy_from_slice(&e.target_source.to_le_bytes());
        data
    }

    fn serialize_group(g: &Group) -> Result<Vec<u8>> {
        let mut data = vec![0u8; 48];
        let name_bytes = g.name.as_bytes();
        if name_bytes.len() > 36 {
            return Err(Error::Invalid(format!("group name too long: {}", g.name)));
        }
        let copy_len = name_bytes.len().min(36);
        if let Some(dst) = data.get_mut(..copy_len) {
            dst.copy_from_slice(
                name_bytes
                    .get(..copy_len)
                    .ok_or_else(|| Error::Invalid("group name range error".into()))?,
            );
        }
        data.get_mut(0x24..0x28)
            .ok_or_else(|| Error::Invalid("group field range".into()))?
            .copy_from_slice(&g.flags.to_le_bytes());
        data.get_mut(0x28..0x30)
            .ok_or_else(|| Error::Invalid("group field range".into()))?
            .copy_from_slice(&g.maximum_size.to_le_bytes());
        Ok(data)
    }

    fn serialize_device(d: &BlockDevice) -> Result<Vec<u8>> {
        let mut data = vec![0u8; 64];
        data.get_mut(0x00..0x08)
            .ok_or_else(|| Error::Invalid("device field range".into()))?
            .copy_from_slice(&d.first_logical_sector.to_le_bytes());
        data.get_mut(0x08..0x0C)
            .ok_or_else(|| Error::Invalid("device field range".into()))?
            .copy_from_slice(&d.alignment.to_le_bytes());
        data.get_mut(0x0C..0x10)
            .ok_or_else(|| Error::Invalid("device field range".into()))?
            .copy_from_slice(&d.alignment_offset.to_le_bytes());
        data.get_mut(0x10..0x18)
            .ok_or_else(|| Error::Invalid("device field range".into()))?
            .copy_from_slice(&d.size.to_le_bytes());
        let name_bytes = d.partition_name.as_bytes();
        if name_bytes.len() > 36 {
            return Err(Error::Invalid("device name too long".into()));
        }
        let copy_len = name_bytes.len().min(36);
        if let Some(dst) = data.get_mut(0x18..0x18 + copy_len) {
            dst.copy_from_slice(
                name_bytes
                    .get(..copy_len)
                    .ok_or_else(|| Error::Invalid("device name range error".into()))?,
            );
        }
        Ok(data)
    }

    fn compute_sha256(data: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hasher.finalize().into()
    }

    fn build_tables_blob(
        partitions: &[Partition],
        extents: &[Extent],
        groups: &[Group],
        devices: &[BlockDevice],
    ) -> Result<(
        Vec<u8>,
        TableDescriptor,
        TableDescriptor,
        TableDescriptor,
        TableDescriptor,
    )> {
        if partitions.len() > 65536
            || extents.len() > 65536
            || groups.len() > 65536
            || devices.len() > 16
        {
            return Err(Error::Invalid("table too large".into()));
        }
        let mut blob = Vec::new();
        let mut offset: u64 = 0;

        let part_desc = TableDescriptor {
            offset: offset as u32,
            num_entries: partitions.len() as u32,
            entry_size: 52,
        };
        for p in partitions {
            blob.extend_from_slice(&Self::serialize_partition(p)?);
        }
        offset = offset
            .checked_add(
                (partitions.len() as u64)
                    .checked_mul(52)
                    .ok_or_else(|| Error::Invalid("partitions blob overflow".into()))?,
            )
            .ok_or_else(|| Error::Invalid("tables offset overflow".into()))?;

        let ext_desc = TableDescriptor {
            offset: offset as u32,
            num_entries: extents.len() as u32,
            entry_size: 24,
        };
        for e in extents {
            blob.extend_from_slice(&Self::serialize_extent(e));
        }
        offset = offset
            .checked_add(
                (extents.len() as u64)
                    .checked_mul(24)
                    .ok_or_else(|| Error::Invalid("extents blob overflow".into()))?,
            )
            .ok_or_else(|| Error::Invalid("tables offset overflow".into()))?;

        let grp_desc = TableDescriptor {
            offset: offset as u32,
            num_entries: groups.len() as u32,
            entry_size: 48,
        };
        for g in groups {
            blob.extend_from_slice(&Self::serialize_group(g)?);
        }
        offset = offset
            .checked_add(
                (groups.len() as u64)
                    .checked_mul(48)
                    .ok_or_else(|| Error::Invalid("groups blob overflow".into()))?,
            )
            .ok_or_else(|| Error::Invalid("tables offset overflow".into()))?;

        let dev_desc = TableDescriptor {
            offset: offset as u32,
            num_entries: devices.len() as u32,
            entry_size: 64,
        };
        for d in devices {
            blob.extend_from_slice(&Self::serialize_device(d)?);
        }

        Ok((blob, part_desc, ext_desc, grp_desc, dev_desc))
    }

    fn serialize_header(header: &MetadataHeader, tables_checksum: [u8; 32]) -> Result<Vec<u8>> {
        if header.header_size < 0x80 || header.header_size > 1024 * 1024 {
            return Err(Error::Invalid("invalid header_size".into()));
        }
        let mut data = vec![0u8; header.header_size as usize];
        let set = |data: &mut [u8], range: std::ops::Range<usize>, src: &[u8]| -> Result<()> {
            let dst = data
                .get_mut(range)
                .ok_or_else(|| Error::Invalid("header field out of range".into()))?;
            dst.copy_from_slice(src);
            Ok(())
        };
        set(&mut data, 0x00..0x04, &LP_METADATA_MAGIC.to_le_bytes())?;
        set(&mut data, 0x04..0x06, &header.major_version.to_le_bytes())?;
        set(&mut data, 0x06..0x08, &header.minor_version.to_le_bytes())?;
        set(&mut data, 0x08..0x0C, &header.header_size.to_le_bytes())?;
        set(&mut data, 0x2C..0x30, &header.tables_size.to_le_bytes())?;
        set(&mut data, 0x30..0x50, &tables_checksum)?;

        let write_desc = |data: &mut [u8], base: usize, desc: &TableDescriptor| -> Result<()> {
            set(data, base..base + 4, &desc.offset.to_le_bytes())?;
            set(data, base + 4..base + 8, &desc.num_entries.to_le_bytes())?;
            set(data, base + 8..base + 12, &desc.entry_size.to_le_bytes())
        };

        write_desc(&mut data, 0x50, &header.partitions)?;
        write_desc(&mut data, 0x5C, &header.extents)?;
        write_desc(&mut data, 0x68, &header.groups)?;
        write_desc(&mut data, 0x74, &header.block_devices)?;

        if let Some(slot) = data.get_mut(0x0C..0x2C) {
            slot.fill(0);
        } else {
            return Err(Error::Invalid("header checksum range".into()));
        }
        let header_hash = Self::compute_sha256(&data);
        set(&mut data, 0x0C..0x2C, &header_hash)?;

        Ok(data)
    }

    /// Render one full metadata slot blob (header + tables, zero-padded to
    /// `metadata_max_size`), with SHA-256 checksums. Shared by
    /// `write_metadata` (updates) and `make` (fresh generation).
    pub fn render_slot(
        header: &MetadataHeader,
        partitions: &[Partition],
        extents: &[Extent],
        groups: &[Group],
        devices: &[BlockDevice],
        metadata_max_size: u32,
    ) -> Result<Vec<u8>> {
        if metadata_max_size == 0 {
            return Err(Error::Invalid("invalid metadata_max_size".into()));
        }
        let max = metadata_max_size as u64;
        let (tables_blob, part_desc, ext_desc, grp_desc, dev_desc) =
            Self::build_tables_blob(partitions, extents, groups, devices)?;

        let header_len = header.header_size as u64;
        let total_needed = header_len
            .checked_add(tables_blob.len() as u64)
            .ok_or_else(|| Error::Invalid("metadata size overflow".into()))?;
        if total_needed > max {
            return Err(Error::Invalid(format!(
                "metadata too large ({} > max {})",
                total_needed, max
            )));
        }

        let tables_checksum = Self::compute_sha256(&tables_blob);

        let new_header = MetadataHeader {
            major_version: header.major_version,
            minor_version: header.minor_version,
            header_size: header.header_size,
            tables_size: tables_blob.len() as u32,
            partitions: part_desc,
            extents: ext_desc,
            groups: grp_desc,
            block_devices: dev_desc,
        };

        let header_data = Self::serialize_header(&new_header, tables_checksum)?;

        let mut slot_data = vec![0u8; metadata_max_size as usize];
        let header_end = header_data.len().min(slot_data.len());
        if let Some(dst) = slot_data.get_mut(..header_end) {
            if let Some(src) = header_data.get(..header_end) {
                dst.copy_from_slice(src);
            } else {
                return Err(Error::Invalid("header range error".into()));
            }
        } else {
            return Err(Error::Invalid("slot buffer range error".into()));
        }

        let tables_start = header_data.len();
        let tables_len = tables_blob
            .len()
            .min(slot_data.len().saturating_sub(tables_start));
        if tables_len != tables_blob.len() {
            return Err(Error::Invalid("metadata does not fit in slot".into()));
        }
        if let Some(dst) = slot_data.get_mut(tables_start..tables_start + tables_len) {
            if let Some(src) = tables_blob.get(..tables_len) {
                dst.copy_from_slice(src);
            } else {
                return Err(Error::Invalid("tables range error".into()));
            }
        } else {
            return Err(Error::Invalid("slot tables range error".into()));
        }
        Ok(slot_data)
    }

    /// Render a 0x34-byte LP geometry blob with SHA-256 checksum
    /// (magic + struct_size + checksum + max_size + slot_count + block_size).
    pub fn render_geometry(
        metadata_max_size: u32,
        metadata_slot_count: u32,
        logical_block_size: u32,
    ) -> Result<Vec<u8>> {
        if metadata_max_size == 0 || metadata_max_size > 16 * 1024 * 1024 {
            return Err(Error::Invalid("invalid metadata_max_size".into()));
        }
        if metadata_slot_count == 0 || metadata_slot_count > 8 {
            return Err(Error::Invalid("invalid metadata_slot_count".into()));
        }
        if logical_block_size != 4096 && logical_block_size != 512 {
            return Err(Error::Invalid("invalid logical_block_size".into()));
        }
        let mut data = vec![0u8; 0x34];
        let set = |data: &mut [u8], range: std::ops::Range<usize>, src: &[u8]| -> Result<()> {
            let dst = data
                .get_mut(range)
                .ok_or_else(|| Error::Invalid("geometry field out of range".into()))?;
            dst.copy_from_slice(src);
            Ok(())
        };
        set(&mut data, 0x00..0x04, &GEOMETRY_MAGIC.to_le_bytes())?;
        set(&mut data, 0x04..0x08, &0x34u32.to_le_bytes())?;
        // checksum field 0x08..0x28 stays zero while hashing.
        set(&mut data, 0x28..0x2C, &metadata_max_size.to_le_bytes())?;
        set(&mut data, 0x2C..0x30, &metadata_slot_count.to_le_bytes())?;
        set(&mut data, 0x30..0x34, &logical_block_size.to_le_bytes())?;
        let digest = Self::compute_sha256(&data);
        set(&mut data, 0x08..0x28, &digest)?;
        Ok(data)
    }

    /// Write a geometry blob at an absolute file offset.
    pub fn write_geometry_at(file: &mut File, offset: u64, blob: &[u8]) -> Result<()> {
        file.seek(SeekFrom::Start(offset)).map_err(Error::Io)?;
        file.write_all(blob).map_err(Error::Io)?;
        Ok(())
    }

    /// Write a full slot blob at an absolute file offset.
    pub fn write_slot_at(file: &mut File, offset: u64, slot_data: &[u8]) -> Result<()> {
        file.seek(SeekFrom::Start(offset)).map_err(Error::Io)?;
        file.write_all(slot_data).map_err(Error::Io)?;
        Ok(())
    }
    /// Write updated metadata to BOTH the primary and backup copies of the
    /// slot identified by `metadata_offset` (AOSP liblp layout), without
    /// touching neighbouring slots.
    #[allow(clippy::too_many_arguments)]
    pub fn write_metadata(
        &self,
        file: &mut File,
        geometry: &Geometry,
        metadata_offset: u64,
        header: &MetadataHeader,
        partitions: &[Partition],
        extents: &[Extent],
        groups: &[Group],
        devices: &[BlockDevice],
    ) -> Result<()> {
        if geometry.metadata_max_size == 0 {
            return Err(Error::Invalid("invalid metadata_max_size".into()));
        }
        let max = geometry.metadata_max_size as u64;
        let slot_count = geometry.metadata_slot_count as u64;

        // Resolve slot index from the loaded primary offset; fall back to
        // backup-derived index if a backup copy was loaded.
        let slot: u64 = slot_from_primary_offset(metadata_offset, max)
            .or_else(|| {
                let rel = metadata_offset.checked_sub(METADATA_BASE_OFFSET)?;
                if rel % max != 0 {
                    return None;
                }
                let idx = rel / max;
                if idx >= slot_count {
                    idx.checked_sub(slot_count)
                } else {
                    Some(idx)
                }
            })
            .ok_or_else(|| Error::Invalid("metadata_offset is not a valid slot base".into()))?;
        if slot >= slot_count {
            return Err(Error::Invalid("metadata slot out of range".into()));
        }

        let primary = primary_offset(slot, max)
            .ok_or_else(|| Error::Invalid("primary offset overflow".into()))?;
        let backup = backup_offset(slot, slot_count, max)
            .ok_or_else(|| Error::Invalid("backup offset overflow".into()))?;

        let slot_data = Self::render_slot(
            header,
            partitions,
            extents,
            groups,
            devices,
            geometry.metadata_max_size,
        )?;

        Self::write_slot_at(file, primary, &slot_data)?;
        Self::write_slot_at(file, backup, &slot_data)?;
        file.flush().map_err(Error::Io)?;

        Ok(())
    }

    pub fn write_payload(&self, file: &mut File, phys_offset: u64, payload: &[u8]) -> Result<()> {
        file.seek(SeekFrom::Start(phys_offset)).map_err(Error::Io)?;
        file.write_all(payload).map_err(Error::Io)?;
        file.flush().map_err(Error::Io)?;
        Ok(())
    }

    /// Stream payload from `input` to `phys_offset` in fixed-size chunks
    /// (O(1) RAM). Returns total bytes written (sector-padded).
    pub fn copy_payload_stream(
        &self,
        file: &mut File,
        phys_offset: u64,
        input: &mut File,
        payload_len: u64,
    ) -> Result<u64> {
        let written = self.copy_payload_segment(file, phys_offset, input, 0, payload_len)?;
        // Zero-pad to sector boundary.
        let pad = (LP_SECTOR_SIZE.saturating_sub(written % LP_SECTOR_SIZE)) % LP_SECTOR_SIZE;
        if pad > 0 {
            let zeros = vec![0u8; pad as usize];
            file.write_all(&zeros).map_err(Error::Io)?;
            file.flush().map_err(Error::Io)?;
            return written
                .checked_add(pad)
                .ok_or_else(|| Error::Invalid("payload size overflow".into()));
        }
        file.flush().map_err(Error::Io)?;
        Ok(written)
    }

    /// Stream one `len`-byte segment of `input` (starting at `input_offset`)
    /// to `phys_offset` in 1 MiB chunks (O(1) RAM, no padding).
    /// Building block for multi-extent (spanning) payload writes.
    pub fn copy_payload_segment(
        &self,
        file: &mut File,
        phys_offset: u64,
        input: &mut File,
        input_offset: u64,
        len: u64,
    ) -> Result<u64> {
        input
            .seek(SeekFrom::Start(input_offset))
            .map_err(Error::Io)?;
        file.seek(SeekFrom::Start(phys_offset)).map_err(Error::Io)?;
        // 1 MiB streaming buffer: O(1) RAM regardless of payload size.
        let mut buf = vec![0u8; 1024 * 1024];
        let mut remaining = len;
        let mut written: u64 = 0;
        while remaining > 0 {
            let want = (remaining.min(buf.len() as u64)) as usize;
            let chunk = buf
                .get_mut(..want)
                .ok_or_else(|| Error::Invalid("stream buffer range error".into()))?;
            input.read_exact(chunk).map_err(Error::Io)?;
            file.write_all(chunk).map_err(Error::Io)?;
            remaining = remaining.saturating_sub(want as u64);
            written = written.saturating_add(want as u64);
        }
        Ok(written)
    }
}

impl Default for LpWriter {
    fn default() -> Self {
        Self::new()
    }
}
