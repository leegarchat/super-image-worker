pub const LP_SECTOR_SIZE: u64 = 512;
pub const GEOMETRY_MAGIC: u32 = 0x616c_4467;
pub const LP_METADATA_MAGIC: u32 = 0x414c_5030;

pub const LP_TARGET_TYPE_LINEAR: u32 = 0;
pub const LP_TARGET_TYPE_ZERO: u32 = 1;

/// LP metadata version written by `make` (matches current AOSP liblp).
pub const LP_METADATA_MAJOR_VERSION: u16 = 10;
pub const LP_METADATA_MINOR_VERSION: u16 = 2;
/// Default on-disk header size for freshly generated metadata.
pub const LP_METADATA_HEADER_SIZE: u32 = 256;

pub const LP_PARTITION_ATTR_READONLY: u32 = 1;
pub const LP_PARTITION_ATTR_SLOT_SUFFIXED: u32 = 2;
pub const LP_PARTITION_ATTR_UPDATED: u32 = 4;
pub const LP_PARTITION_ATTR_DISABLED: u32 = 8;

/// AOSP liblp on-disk layout constants.
pub const GEOMETRY_PRIMARY_OFFSET: u64 = 0x1000;
pub const GEOMETRY_BACKUP_OFFSET: u64 = 0x2000;
pub const METADATA_BASE_OFFSET: u64 = 0x3000;

/// Primary metadata offset for a given slot (AOSP liblp rule).
pub fn primary_offset(slot: u64, metadata_max_size: u64) -> Option<u64> {
    slot.checked_mul(metadata_max_size)?
        .checked_add(METADATA_BASE_OFFSET)
}

/// Backup metadata offset for a given slot (AOSP liblp rule:
/// backup area follows ALL primary slots).
pub fn backup_offset(slot: u64, slot_count: u64, metadata_max_size: u64) -> Option<u64> {
    slot_count
        .checked_add(slot)?
        .checked_mul(metadata_max_size)?
        .checked_add(METADATA_BASE_OFFSET)
}

/// Derive slot index from a metadata offset. Returns None if the offset
/// is not aligned to a primary slot base.
pub fn slot_from_primary_offset(metadata_offset: u64, metadata_max_size: u64) -> Option<u64> {
    if metadata_max_size == 0 {
        return None;
    }
    let base = metadata_offset.checked_sub(METADATA_BASE_OFFSET)?;
    if base % metadata_max_size != 0 {
        return None;
    }
    Some(base / metadata_max_size)
}

#[derive(Debug, Clone)]
pub struct TableDescriptor {
    pub offset: u32,
    pub num_entries: u32,
    pub entry_size: u32,
}

#[derive(Debug, Clone)]
pub struct Geometry {
    pub metadata_max_size: u32,
    pub metadata_slot_count: u32,
    pub logical_block_size: u32,
}

#[derive(Debug, Clone)]
pub struct MetadataHeader {
    pub major_version: u16,
    pub minor_version: u16,
    pub header_size: u32,
    pub tables_size: u32,
    pub partitions: TableDescriptor,
    pub extents: TableDescriptor,
    pub groups: TableDescriptor,
    pub block_devices: TableDescriptor,
}

#[derive(Debug, Clone)]
pub struct Partition {
    pub name: String,
    pub attributes: u32,
    pub first_extent_index: u32,
    pub num_extents: u32,
    pub group_index: u32,
}

#[derive(Debug, Clone)]
pub struct Extent {
    pub num_sectors: u64,
    pub target_type: u32,
    pub target_data: u64,
    pub target_source: u32,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub name: String,
    pub flags: u32,
    pub maximum_size: u64,
}

#[derive(Debug, Clone)]
pub struct BlockDevice {
    pub first_logical_sector: u64,
    pub alignment: u32,
    pub alignment_offset: u32,
    pub size: u64,
    pub partition_name: String,
}
