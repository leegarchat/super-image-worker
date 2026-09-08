pub mod extent_stream;
pub mod multiblock;
mod parser;
mod source;

pub use extent_stream::{ExtentReader, extract_partition};
pub use multiblock::{
    MultiBlockImage, SplitExtentReader, extract_partition_split, open_multiblock,
    resolve_device_bindings,
};
pub use parser::LpParser;
pub use source::{BlockSource, Image};

use crate::error::{Error, Result};
use crate::format::*;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct SuperData {
    pub image_format: String,
    pub image_size: u64,
    pub geometry: Geometry,
    pub geometry_offset: u64,
    pub header: MetadataHeader,
    pub metadata_offset: u64,
    pub metadata_slot: u64,
    pub partitions: Vec<Partition>,
    pub extents: Vec<Extent>,
    pub groups: Vec<Group>,
    pub devices: Vec<BlockDevice>,
}

/// Map a partition suffix (`a`/`b`) to a metadata slot index
/// (AOSP convention: slot 0 <-> `_a`, slot 1 <-> `_b`).
pub fn suffix_to_slot(suffix: &str) -> Option<u64> {
    match suffix {
        "a" | "A" => Some(0),
        "b" | "B" => Some(1),
        _ => None,
    }
}

/// Map a metadata slot index back to its conventional suffix.
pub fn slot_to_suffix(slot: u64) -> Option<&'static str> {
    match slot {
        0 => Some("a"),
        1 => Some("b"),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_super_data(
    image_format: String,
    image_size: u64,
    geometry: Geometry,
    geometry_offset: u64,
    header: MetadataHeader,
    metadata_offset: u64,
    metadata_slot: u64,
    parser: &LpParser,
    image: &mut Image,
) -> Result<SuperData> {
    let partitions = parser.read_partitions(image, &header, metadata_offset)?;
    let extents = parser.read_extents(image, &header, metadata_offset)?;
    let groups = parser.read_groups(image, &header, metadata_offset)?;
    let devices = parser.read_block_devices(image, &header, metadata_offset)?;
    Ok(SuperData {
        image_format,
        image_size,
        geometry,
        geometry_offset,
        header,
        metadata_offset,
        metadata_slot,
        partitions,
        extents,
        groups,
        devices,
    })
}

/// Load one explicit metadata slot (primary then backup of that slot).
pub fn load_super_in_slot(path: &Path, slot: u64) -> Result<SuperData> {
    let mut image = Image::open(path)?;
    let format_name = image.format_name().to_string();
    let image_size = image.size();
    let parser = LpParser::new();
    let (geometry, geometry_offset) = parser.find_geometry(&mut image)?;
    let (header, metadata_offset) = parser.find_metadata_for_slot(&mut image, &geometry, slot)?;
    build_super_data(
        format_name,
        image_size,
        geometry,
        geometry_offset,
        header,
        metadata_offset,
        slot,
        &parser,
        &mut image,
    )
}

/// Load every valid metadata slot (e.g. slot 0 `_a` + slot 1 `_b` on
/// shiba). Returns slots in index order. Errors only when no slot
/// validates at all.
pub fn load_super_all(path: &Path) -> Result<Vec<SuperData>> {
    let mut image = Image::open(path)?;
    let format_name = image.format_name().to_string();
    let image_size = image.size();
    let parser = LpParser::new();
    let (geometry, geometry_offset) = parser.find_geometry(&mut image)?;
    let found = parser.find_all_metadata(&mut image, &geometry);
    if found.is_empty() {
        return Err(Error::NotFound(
            "LP metadata header not found (all primary+backup slots invalid)".into(),
        ));
    }
    let mut out = Vec::with_capacity(found.len());
    for (slot, header, metadata_offset) in found {
        let partitions = parser.read_partitions(&mut image, &header, metadata_offset)?;
        let extents = parser.read_extents(&mut image, &header, metadata_offset)?;
        let groups = parser.read_groups(&mut image, &header, metadata_offset)?;
        let devices = parser.read_block_devices(&mut image, &header, metadata_offset)?;
        out.push(SuperData {
            image_format: format_name.clone(),
            image_size,
            geometry: geometry.clone(),
            geometry_offset,
            header,
            metadata_offset,
            metadata_slot: slot,
            partitions,
            extents,
            groups,
            devices,
        });
    }
    Ok(out)
}

pub fn load_super(path: &Path) -> Result<SuperData> {
    let mut image = Image::open(path)?;
    let format_name = image.format_name().to_string();
    let image_size = image.size();

    let parser = LpParser::new();
    let (geometry, geometry_offset) = parser.find_geometry(&mut image)?;
    let (header, metadata_offset) = parser.find_metadata(&mut image, &geometry)?;
    let metadata_slot =
        slot_from_primary_offset(metadata_offset, geometry.metadata_max_size as u64)
            .or_else(|| {
                // Backup slot: offset = base + (count + slot) * max.
                let max = geometry.metadata_max_size as u64;
                let count = geometry.metadata_slot_count as u64;
                if max == 0 {
                    return None;
                }
                let rel = metadata_offset.checked_sub(METADATA_BASE_OFFSET)?;
                if rel % max != 0 {
                    return None;
                }
                let idx = rel / max;
                if idx >= count {
                    idx.checked_sub(count)
                } else {
                    Some(idx)
                }
            })
            .unwrap_or(0);

    let partitions = parser.read_partitions(&mut image, &header, metadata_offset)?;
    let extents = parser.read_extents(&mut image, &header, metadata_offset)?;
    let groups = parser.read_groups(&mut image, &header, metadata_offset)?;
    let devices = parser.read_block_devices(&mut image, &header, metadata_offset)?;

    Ok(SuperData {
        image_format: format_name,
        image_size,
        geometry,
        geometry_offset,
        header,
        metadata_offset,
        metadata_slot,
        partitions,
        extents,
        groups,
        devices,
    })
}

impl SuperData {
    pub fn partition_size(&self, p: &Partition) -> u64 {
        let mut total: u64 = 0;
        let mut idx = p.first_extent_index as u64;
        for _ in 0..p.num_extents {
            let e = match self.extents.get(idx as usize) {
                Some(e) => e,
                None => break,
            };
            total = total.saturating_add(e.num_sectors.saturating_mul(LP_SECTOR_SIZE));
            idx = idx.saturating_add(1);
        }
        total
    }

    pub fn partition_sectors(&self, p: &Partition) -> u64 {
        let mut total: u64 = 0;
        let mut idx = p.first_extent_index as u64;
        for _ in 0..p.num_extents {
            let e = match self.extents.get(idx as usize) {
                Some(e) => e,
                None => break,
            };
            total = total.saturating_add(e.num_sectors);
            idx = idx.saturating_add(1);
        }
        total
    }

    pub fn partition_group_name(&self, p: &Partition) -> &str {
        self.groups
            .get(p.group_index as usize)
            .map(|g| g.name.as_str())
            .unwrap_or("")
    }

    pub fn partition_group_max_size(&self, p: &Partition) -> u64 {
        self.groups
            .get(p.group_index as usize)
            .map(|g| g.maximum_size)
            .unwrap_or(0)
    }

    pub fn partition_first_phys_offset(&self, p: &Partition) -> u64 {
        if p.num_extents == 0 {
            return 0;
        }
        self.extents
            .get(p.first_extent_index as usize)
            .filter(|e| e.target_type == LP_TARGET_TYPE_LINEAR)
            .map(|e| e.target_data.saturating_mul(LP_SECTOR_SIZE))
            .unwrap_or(0)
    }

    pub fn partition_device_name(&self, p: &Partition) -> &str {
        if p.num_extents == 0 {
            return "";
        }
        self.extents
            .get(p.first_extent_index as usize)
            .and_then(|e| self.devices.get(e.target_source as usize))
            .map(|d| d.partition_name.as_str())
            .unwrap_or("")
    }

    pub fn find_suffix(name: &str) -> Option<&str> {
        if name.ends_with("_a") {
            Some("a")
        } else if name.ends_with("_b") {
            Some("b")
        } else {
            None
        }
    }

    pub fn strip_suffix(name: &str) -> &str {
        if let Some(base) = name.strip_suffix("_a") {
            base
        } else if let Some(base) = name.strip_suffix("_b") {
            base
        } else {
            name
        }
    }

    pub fn available_suffixes(&self) -> Vec<String> {
        let mut suffixes: Vec<String> = self
            .partitions
            .iter()
            .filter_map(|p| Self::find_suffix(&p.name).map(|s| s.to_string()))
            .collect();
        suffixes.sort();
        suffixes.dedup();
        suffixes
    }

    pub fn filter_by_slot(&self, suffix: Option<&str>) -> Vec<&Partition> {
        match suffix {
            None => self.partitions.iter().collect(),
            Some(sfx) => self
                .partitions
                .iter()
                .filter(|p| Self::find_suffix(&p.name).is_none_or(|s| s == sfx))
                .collect(),
        }
    }

    /// Resolve a partition by full name or base name + slot filter.
    /// `slot` is None (all), Some("a") or Some("b").
    /// Slot-suffixed lookup: "system" + a -> "system_a".
    /// Slotless partitions match regardless of filter.
    pub fn resolve_partition(
        &self,
        name: &str,
        slot: Option<&str>,
    ) -> std::result::Result<usize, Error> {
        // Exact match first.
        if let Some(idx) = self.partitions.iter().position(|p| p.name == name) {
            return Ok(idx);
        }
        // Base-name + slot match (handles Virtual A/B slotless + A/B).
        let mut candidates: Vec<usize> = Vec::new();
        for (idx, p) in self.partitions.iter().enumerate() {
            if Self::strip_suffix(&p.name) != name {
                continue;
            }
            match slot {
                Some(sfx) => {
                    match Self::find_suffix(&p.name) {
                        Some(s) if s == sfx => candidates.push(idx),
                        None => candidates.push(idx), // slotless matches any slot
                        _ => {}
                    }
                }
                None => candidates.push(idx),
            }
        }
        match candidates.len() {
            0 => Err(Error::NotFound(format!("partition '{name}' not found"))),
            1 => Ok(candidates[0]),
            _ => {
                let names: Vec<&str> = candidates
                    .iter()
                    .filter_map(|i| self.partitions.get(*i))
                    .map(|p| p.name.as_str())
                    .collect();
                Err(Error::Invalid(format!(
                    "ambiguous name '{name}', matches: {names:?}. Use full name or --slot"
                )))
            }
        }
    }

    /// Intelligent group selection for a new partition.
    /// Prefers existing groups carrying the partition's slot suffix,
    /// ranked by known vendor prefixes, else largest group.
    pub fn select_group_for_name(&self, part_name: &str) -> Option<u32> {
        let suffix = Self::find_suffix(part_name);
        if let Some(sfx) = suffix {
            let tail = format!("_{sfx}");
            let mut cands: Vec<(usize, &Group)> = self
                .groups
                .iter()
                .enumerate()
                .filter(|(_, g)| g.name.ends_with(tail.as_str()))
                .collect();
            if !cands.is_empty() {
                let rank = |n: &str| -> u32 {
                    if n.starts_with("qti_dynamic_partitions") {
                        0
                    } else if n.starts_with("google_dynamic_partitions") {
                        1
                    } else if n.starts_with("samsung_dynamic_partitions")
                        || n.starts_with("sec_dynamic_partitions")
                    {
                        2
                    } else if n.starts_with("mtk_dynamic_partitions") {
                        3
                    } else {
                        4
                    }
                };
                cands.sort_by_key(|(_, g)| {
                    (rank(g.name.as_str()), std::cmp::Reverse(g.maximum_size))
                });
                return cands.first().map(|(i, _)| *i as u32);
            }
        }
        // Fallback: group with the largest maximum_size (skip empty "default" if possible).
        self.groups
            .iter()
            .enumerate()
            .max_by_key(|(_, g)| g.maximum_size)
            .map(|(i, _)| i as u32)
    }

    pub fn partition_attr_string(attrs: u32) -> String {
        let mut flags = Vec::new();
        if attrs & LP_PARTITION_ATTR_READONLY != 0 {
            flags.push("readonly");
        }
        if attrs & LP_PARTITION_ATTR_SLOT_SUFFIXED != 0 {
            flags.push("slot_suffixed");
        }
        if attrs & LP_PARTITION_ATTR_UPDATED != 0 {
            flags.push("updated");
        }
        if attrs & LP_PARTITION_ATTR_DISABLED != 0 {
            flags.push("disabled");
        }
        if flags.is_empty() {
            "none".to_string()
        } else {
            flags.join("|")
        }
    }
}
