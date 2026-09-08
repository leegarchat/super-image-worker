use super::human_size;
use super_image_worker_core::{LP_SECTOR_SIZE, LP_TARGET_TYPE_LINEAR, SuperData};

pub struct TsvOptions {
    pub header: bool,
    pub columns: Vec<String>,
    pub bytes: bool,
}

impl Default for TsvOptions {
    fn default() -> Self {
        Self {
            header: true,
            columns: vec![
                "name".into(),
                "group".into(),
                "size".into(),
                "extents".into(),
                "attrs".into(),
            ],
            bytes: false,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn print_human(
    data: &SuperData,
    suffix: Option<&str>,
    show_info: bool,
    show_groups: bool,
    show_devices: bool,
    show_partitions: bool,
    show_extents: bool,
    show_mapping: bool,
) {
    let filtered = data.filter_by_suffix(suffix);
    let suffixes = data.available_suffixes();

    if show_info {
        println!("=== Super Partition Info ===");
        println!("format: {}", data.image_format);
        println!(
            "size: {} ({})",
            data.image_size,
            human_size(data.image_size)
        );
        println!("geometry_offset: 0x{:x}", data.geometry_offset);
        println!(
            "metadata_offset: 0x{:x} (slot {})",
            data.metadata_offset, data.metadata_slot
        );
        println!("metadata_max_size: {}", data.geometry.metadata_max_size);
        println!("metadata_slot_count: {}", data.geometry.metadata_slot_count);
        println!("logical_block_size: {}", data.geometry.logical_block_size);
        println!(
            "metadata_version: {}.{}",
            data.header.major_version, data.header.minor_version
        );
        println!("header_size: {}", data.header.header_size);
        println!("tables_size: {}", data.header.tables_size);
        println!("available_suffixes: {}", suffixes.join(","));
        println!();
    }

    if show_groups {
        println!("=== Groups ({}) ===", data.groups.len());
        for (i, g) in data.groups.iter().enumerate() {
            println!(
                "[{}] name={} flags=0x{:x} max_size={} ({})",
                i,
                g.name,
                g.flags,
                g.maximum_size,
                human_size(g.maximum_size)
            );
        }
        println!();
    }

    if show_devices {
        println!("=== Block Devices ({}) ===", data.devices.len());
        for (i, d) in data.devices.iter().enumerate() {
            println!(
                "[{}] name={} first_sector={} alignment={} size={} ({})",
                i,
                d.partition_name,
                d.first_logical_sector,
                d.alignment,
                d.size,
                human_size(d.size)
            );
        }
        println!();
    }

    if show_partitions {
        println!("=== Partitions ({}) ===", filtered.len());
        for (i, p) in filtered.iter().enumerate() {
            let group_name = data.partition_group_name(p);
            let size = data.partition_size(p);
            println!(
                "[{}] name={} group={} attrs=[{}] extents={} size={} ({})",
                i,
                p.name,
                group_name,
                SuperData::partition_attr_string(p.attributes),
                p.num_extents,
                size,
                human_size(size)
            );
        }
        println!();
    }

    if show_extents {
        println!("=== Extents ({}) ===", data.extents.len());
        for (i, e) in data.extents.iter().enumerate() {
            let size = e.num_sectors.saturating_mul(LP_SECTOR_SIZE);
            let device = data
                .devices
                .get(e.target_source as usize)
                .map(|d| d.partition_name.as_str())
                .unwrap_or("?");
            match e.target_type {
                LP_TARGET_TYPE_LINEAR => println!(
                    "[{}] type=linear sectors={} size={} phys_offset=0x{:x} device={}",
                    i,
                    e.num_sectors,
                    size,
                    e.target_data.saturating_mul(LP_SECTOR_SIZE),
                    device
                ),
                1 => println!("[{}] type=zero sectors={} size={}", i, e.num_sectors, size),
                _ => println!(
                    "[{}] type={} sectors={} size={}",
                    i, e.target_type, e.num_sectors, size
                ),
            }
        }
        println!();
    }

    if show_mapping {
        println!("=== Partition -> Extent Mapping ===");
        for p in &filtered {
            println!("\"{}\":", p.name);
            let mut idx = p.first_extent_index as u64;
            for _ in 0..p.num_extents {
                if let Some(e) = data.extents.get(idx as usize) {
                    let size = e.num_sectors.saturating_mul(LP_SECTOR_SIZE);
                    // Owning block device (relevant for split/retrofit layouts).
                    let dev = if e.target_type == LP_TARGET_TYPE_LINEAR {
                        data.devices
                            .get(e.target_source as usize)
                            .map(|d| d.partition_name.as_str())
                            .unwrap_or("?")
                    } else {
                        "-"
                    };
                    match e.target_type {
                        LP_TARGET_TYPE_LINEAR => println!(
                            "  extent[{}]: linear dev={} 0x{:x}..0x{:x} ({})",
                            idx,
                            dev,
                            e.target_data.saturating_mul(LP_SECTOR_SIZE),
                            e.target_data
                                .saturating_mul(LP_SECTOR_SIZE)
                                .saturating_add(size),
                            human_size(size)
                        ),
                        1 => println!("  extent[{}]: zero ({})", idx, human_size(size)),
                        _ => println!(
                            "  extent[{}]: type={} ({})",
                            idx,
                            e.target_type,
                            human_size(size)
                        ),
                    }
                }
                idx = idx.saturating_add(1);
            }
        }
    }
}

pub fn json_value(data: &SuperData, suffix: Option<&str>) -> serde_json::Value {
    let filtered = data.filter_by_suffix(suffix);
    let suffixes = data.available_suffixes();

    let groups: Vec<serde_json::Value> = data.groups.iter().map(|g| {
        serde_json::json!({"name": g.name, "flags": g.flags, "max_size": g.maximum_size, "max_size_human": human_size(g.maximum_size)})
    }).collect();
    let devices: Vec<serde_json::Value> = data.devices.iter().map(|d| {
        serde_json::json!({"name": d.partition_name, "first_sector": d.first_logical_sector, "alignment": d.alignment, "alignment_offset": d.alignment_offset, "size": d.size, "size_human": human_size(d.size)})
    }).collect();
    let partitions: Vec<serde_json::Value> = filtered
        .iter()
        .map(|p| {
            let size = data.partition_size(p);
            serde_json::json!({
                "name": p.name,
                "base_name": SuperData::strip_suffix(&p.name),
                "suffix": SuperData::find_suffix(&p.name).unwrap_or(""),
                "group": data.partition_group_name(p),
                "group_index": p.group_index,
                "group_max_size": data.partition_group_max_size(p),
                "attributes": p.attributes,
                "attrs": SuperData::partition_attr_string(p.attributes),
                "num_extents": p.num_extents,
                "first_extent": p.first_extent_index,
                "first_phys_offset": data.partition_first_phys_offset(p),
                "device": data.partition_device_name(p),
                "size": size,
                "size_human": human_size(size),
                "sectors": data.partition_sectors(p),
            })
        })
        .collect();
    let extents: Vec<serde_json::Value> = data.extents.iter().map(|e| {
        let size = e.num_sectors.saturating_mul(LP_SECTOR_SIZE);
        let t = match e.target_type { LP_TARGET_TYPE_LINEAR => "linear", 1 => "zero", _ => "unknown" };
        serde_json::json!({
            "type": t,
            "type_id": e.target_type,
            "sectors": e.num_sectors,
            "size": size,
            "size_human": human_size(size),
            "phys_offset": if e.target_type == LP_TARGET_TYPE_LINEAR { e.target_data.saturating_mul(LP_SECTOR_SIZE) } else { 0 },
            "target_data": e.target_data,
            "target_source": e.target_source,
            "device": data.devices.get(e.target_source as usize).map(|d| d.partition_name.as_str()).unwrap_or("?"),
        })
    }).collect();

    serde_json::json!({
        "image_format": data.image_format,
        "image_size": data.image_size,
        "image_size_human": human_size(data.image_size),
        "geometry_offset": format!("0x{:x}", data.geometry_offset),
        "metadata_offset": format!("0x{:x}", data.metadata_offset),
        "metadata_slot": data.metadata_slot,
        "metadata_max_size": data.geometry.metadata_max_size,
        "metadata_slot_count": data.geometry.metadata_slot_count,
        "logical_block_size": data.geometry.logical_block_size,
        "metadata_version": format!("{}.{}", data.header.major_version, data.header.minor_version),
        "header_size": data.header.header_size,
        "tables_size": data.header.tables_size,
        "available_suffixes": suffixes,
        "groups": groups,
        "devices": devices,
        "partitions": partitions,
        "extents": extents,
    })
}

pub fn print_json(data: &SuperData, suffix: Option<&str>) {
    let root = json_value(data, suffix);
    println!(
        "{}",
        serde_json::to_string_pretty(&root).unwrap_or_else(|_| "{}".to_string())
    );
}

/// Multi-slot JSON: single slot prints the plain object (backward
/// compatible); several slots print `{"slots": [...]}` plus a merged
/// partition count, mirroring `lpdump -a`.
pub fn print_json_slots(datas: &[SuperData], suffix: Option<&str>) {
    if datas.len() == 1 {
        if let Some(d) = datas.first() {
            print_json(d, suffix);
        }
        return;
    }
    let slots: Vec<serde_json::Value> =
        datas.iter().map(|d| json_value(d, suffix)).collect();
    let total_partitions: usize = datas.iter().map(|d| d.partitions.len()).sum();
    let first = datas.first();
    let root = serde_json::json!({
        "image_format": first.map(|d| d.image_format.clone()).unwrap_or_default(),
        "image_size": first.map(|d| d.image_size).unwrap_or(0),
        "metadata_slot_count": first.map(|d| d.geometry.metadata_slot_count).unwrap_or(0),
        "slot_count": datas.len(),
        "total_partitions": total_partitions,
        "slots": slots,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&root).unwrap_or_else(|_| "{}".to_string())
    );
}

fn tsv_column(data: &SuperData, p: &super_image_worker_core::Partition, col: &str, bytes: bool) -> String {
    let group_name = data.partition_group_name(p);
    let size = data.partition_size(p);
    let suffix = SuperData::find_suffix(&p.name).unwrap_or("");
    let base_name = SuperData::strip_suffix(&p.name);
    match col {
        "name" => p.name.clone(),
        "base_name" => base_name.to_string(),
        "suffix" => suffix.to_string(),
        "group" => group_name.to_string(),
        "group_index" => p.group_index.to_string(),
        "group_max_size" => data.partition_group_max_size(p).to_string(),
        "size" => {
            if bytes {
                size.to_string()
            } else {
                human_size(size)
            }
        }
        "size_bytes" => size.to_string(),
        "sectors" => data.partition_sectors(p).to_string(),
        "extents" => p.num_extents.to_string(),
        "attrs" => SuperData::partition_attr_string(p.attributes),
        "attributes" => p.attributes.to_string(),
        "first_extent" => p.first_extent_index.to_string(),
        "first_phys_offset" => data.partition_first_phys_offset(p).to_string(),
        "first_phys_offset_hex" => format!("0x{:x}", data.partition_first_phys_offset(p)),
        "device" => data.partition_device_name(p).to_string(),
        "readonly" => {
            (p.attributes & super_image_worker_core::LP_PARTITION_ATTR_READONLY != 0).to_string()
        }
        _ => String::new(),
    }
}

pub fn print_tsv(data: &SuperData, suffix: Option<&str>, opts: &TsvOptions) {
    let filtered = data.filter_by_suffix(suffix);

    if opts.header {
        println!("{}", opts.columns.join("\t"));
    }

    for p in &filtered {
        let values: Vec<String> = opts
            .columns
            .iter()
            .map(|c| tsv_column(data, p, c.as_str(), opts.bytes))
            .collect();
        println!("{}", values.join("\t"));
    }
}

pub fn print_env(data: &SuperData, suffix: Option<&str>) {
    let filtered = data.filter_by_suffix(suffix);
    let suffixes = data.available_suffixes();

    println!("SUPER_IMAGE_FORMAT={}", data.image_format);
    println!("SUPER_IMAGE_SIZE={}", data.image_size);
    println!("SUPER_GEOMETRY_OFFSET=0x{:x}", data.geometry_offset);
    println!("SUPER_METADATA_OFFSET=0x{:x}", data.metadata_offset);
    println!("SUPER_METADATA_SLOT={}", data.metadata_slot);
    println!(
        "SUPER_METADATA_MAX_SIZE={}",
        data.geometry.metadata_max_size
    );
    println!(
        "SUPER_METADATA_SLOT_COUNT={}",
        data.geometry.metadata_slot_count
    );
    println!(
        "SUPER_LOGICAL_BLOCK_SIZE={}",
        data.geometry.logical_block_size
    );
    println!(
        "SUPER_METADATA_VERSION={}.{}",
        data.header.major_version, data.header.minor_version
    );
    println!("SUPER_AVAILABLE_SUFFIXES={}", suffixes.join(","));
    println!("SUPER_PARTITION_COUNT={}", filtered.len());

    for (i, p) in filtered.iter().enumerate() {
        let key = p.name.to_uppercase().replace('-', "_");
        let group_name = data.partition_group_name(p);
        let size = data.partition_size(p);
        let suffix = SuperData::find_suffix(&p.name).unwrap_or("");
        let base_name = SuperData::strip_suffix(&p.name);
        println!("SUPER_PART_{i}_NAME={}", p.name);
        println!("SUPER_PART_{i}_BASE_NAME={base_name}");
        println!("SUPER_PART_{i}_SUFFIX={suffix}");
        println!("SUPER_PART_{i}_GROUP={group_name}");
        println!("SUPER_PART_{i}_SIZE={size}");
        println!("SUPER_PART_{i}_SIZE_HUMAN={}", human_size(size));
        println!("SUPER_PART_{i}_EXTENTS={}", p.num_extents);
        println!(
            "SUPER_PART_{i}_ATTRS={}",
            SuperData::partition_attr_string(p.attributes)
        );
        println!("SUPER_PART_{key}_SIZE={size}");
    }
}

pub fn print_get(data: &SuperData, key: &str) {
    let parts: Vec<&str> = key.split('.').collect();
    match parts.as_slice() {
        ["format"] => print!("{}", data.image_format),
        ["size"] => print!("{}", data.image_size),
        ["size_human"] => print!("{}", human_size(data.image_size)),
        ["geometry_offset"] => print!("0x{:x}", data.geometry_offset),
        ["metadata_offset"] => print!("0x{:x}", data.metadata_offset),
        ["metadata_slot"] => print!("{}", data.metadata_slot),
        ["metadata_max_size"] => print!("{}", data.geometry.metadata_max_size),
        ["metadata_slot_count"] => print!("{}", data.geometry.metadata_slot_count),
        ["logical_block_size"] => print!("{}", data.geometry.logical_block_size),
        ["metadata_version"] => print!(
            "{}.{}",
            data.header.major_version, data.header.minor_version
        ),
        ["header_size"] => print!("{}", data.header.header_size),
        ["tables_size"] => print!("{}", data.header.tables_size),
        ["available_suffixes"] => print!("{}", data.available_suffixes().join(",")),
        ["partition_count"] => print!("{}", data.partitions.len()),
        ["partitions", name, field] | ["partition", name, field] => {
            if let Some(p) = data.partitions.iter().find(|p| p.name == *name) {
                let group_name = data.partition_group_name(p);
                let group_max = data.partition_group_max_size(p);
                let size = data.partition_size(p);
                let suffix = SuperData::find_suffix(&p.name).unwrap_or("");
                let base_name = SuperData::strip_suffix(&p.name);
                let sectors = data.partition_sectors(p);
                let first_phys = data.partition_first_phys_offset(p);
                let first_device = data.partition_device_name(p);
                match *field {
                    "name" => print!("{}", p.name),
                    "base_name" => print!("{base_name}"),
                    "suffix" => print!("{suffix}"),
                    "group" => print!("{group_name}"),
                    "group_index" => print!("{}", p.group_index),
                    "group_max_size" => print!("{group_max}"),
                    "group_max_size_human" => print!("{}", human_size(group_max)),
                    "size" => print!("{size}"),
                    "size_human" => print!("{}", human_size(size)),
                    "sectors" => print!("{sectors}"),
                    "extents" => print!("{}", p.num_extents),
                    "attrs" => print!("{}", SuperData::partition_attr_string(p.attributes)),
                    "attributes" => print!("{}", p.attributes),
                    "first_extent" => print!("{}", p.first_extent_index),
                    "first_phys_offset" => print!("{first_phys}"),
                    "first_phys_offset_hex" => print!("0x{first_phys:x}"),
                    "device" => print!("{first_device}"),
                    "readonly" => print!(
                        "{}",
                        p.attributes & super_image_worker_core::LP_PARTITION_ATTR_READONLY != 0
                    ),
                    "slot_suffixed" => print!(
                        "{}",
                        p.attributes & super_image_worker_core::LP_PARTITION_ATTR_SLOT_SUFFIXED != 0
                    ),
                    _ => {
                        eprintln!("unknown field: {field}");
                        std::process::exit(2);
                    }
                }
            } else {
                eprintln!("partition not found: {name}");
                std::process::exit(2);
            }
        }
        ["partitions", name, "extent", idx_str, field]
        | ["partition", name, "extent", idx_str, field] => {
            if let Some(p) = data.partitions.iter().find(|p| p.name == *name) {
                let idx: u32 = match idx_str.parse() {
                    Ok(v) => v,
                    Err(_) => {
                        eprintln!("invalid extent index: {idx_str}");
                        std::process::exit(2);
                    }
                };
                if idx >= p.num_extents {
                    eprintln!("extent index {idx} out of range (0..{})", p.num_extents);
                    std::process::exit(2);
                }
                let ext_idx = (p.first_extent_index as u64).saturating_add(idx as u64);
                if let Some(e) = data.extents.get(ext_idx as usize) {
                    let size = e.num_sectors.saturating_mul(LP_SECTOR_SIZE);
                    let device = data
                        .devices
                        .get(e.target_source as usize)
                        .map(|d| d.partition_name.as_str())
                        .unwrap_or("");
                    let phys_offset = if e.target_type == LP_TARGET_TYPE_LINEAR {
                        e.target_data.saturating_mul(LP_SECTOR_SIZE)
                    } else {
                        0
                    };
                    match *field {
                        "type" => print!(
                            "{}",
                            match e.target_type {
                                LP_TARGET_TYPE_LINEAR => "linear",
                                1 => "zero",
                                _ => "unknown",
                            }
                        ),
                        "sectors" => print!("{}", e.num_sectors),
                        "size" => print!("{size}"),
                        "size_human" => print!("{}", human_size(size)),
                        "phys_offset" => print!("{phys_offset}"),
                        "phys_offset_hex" => print!("0x{phys_offset:x}"),
                        "target_data" => print!("{}", e.target_data),
                        "target_source" => print!("{}", e.target_source),
                        "device" => print!("{device}"),
                        _ => {
                            eprintln!("unknown field: {field}");
                            std::process::exit(2);
                        }
                    }
                } else {
                    eprintln!("extent not found");
                    std::process::exit(2);
                }
            } else {
                eprintln!("partition not found: {name}");
                std::process::exit(2);
            }
        }
        ["groups"] => {
            let mut first = true;
            for g in &data.groups {
                if !first {
                    print!(" ");
                }
                print!("{}", g.name);
                first = false;
            }
        }
        ["group", name, field] => {
            if let Some(g) = data.groups.iter().find(|g| g.name == *name) {
                match *field {
                    "name" => print!("{}", g.name),
                    "flags" => print!("{}", g.flags),
                    "max_size" => print!("{}", g.maximum_size),
                    "max_size_human" => print!("{}", human_size(g.maximum_size)),
                    _ => {
                        eprintln!("unknown field: {field}");
                        std::process::exit(2);
                    }
                }
            } else {
                eprintln!("group not found: {name}");
                std::process::exit(2);
            }
        }
        ["devices"] => {
            let mut first = true;
            for d in &data.devices {
                if !first {
                    print!(" ");
                }
                print!("{}", d.partition_name);
                first = false;
            }
        }
        ["device", name, field] => {
            if let Some(d) = data.devices.iter().find(|d| d.partition_name == *name) {
                match *field {
                    "name" => print!("{}", d.partition_name),
                    "first_sector" => print!("{}", d.first_logical_sector),
                    "alignment" => print!("{}", d.alignment),
                    "size" => print!("{}", d.size),
                    "size_human" => print!("{}", human_size(d.size)),
                    _ => {
                        eprintln!("unknown field: {field}");
                        std::process::exit(2);
                    }
                }
            } else {
                eprintln!("device not found: {name}");
                std::process::exit(2);
            }
        }
        _ => {
            eprintln!("unknown key: {key}");
            eprintln!("use --list-keys for available keys");
            std::process::exit(1);
        }
    }
}
