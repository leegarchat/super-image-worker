use super::split_util;
use clap::Args;
use super_image_worker_core::{LP_SECTOR_SIZE, LP_TARGET_TYPE_LINEAR, LpWriter};
use std::fs::File;
use std::path::PathBuf;

#[derive(Args)]
#[command(
    about = "Create a new empty partition (without payload)",
    long_about = "Create a new partition entry in super image LP metadata.\n\n\
Unlike 'add', writes metadata only - no payload bytes are touched, so the new\n\
partition initially exposes whatever (zeroed on `make` images) blocks were\n\
allocated. --size reserves space (rounded UP to sectors; 0 creates an\n\
extent-less placeholder, handy for the inactive slot). Missing groups can be\n\
created on the fly with --group-size. Group maximum_size is enforced unless\n\
--force. --dry-run previews. Raw images only. No root needed.\n\n\
SIZE FORMAT: plain bytes, K/M/G suffixes (upper or lower), 0 = empty.\n\
ATTRIBUTES (--attrs, comma list): readonly, slot_suffixed (default: readonly).\n\n\
SPLIT / RETROFIT: repeatable --device; first-fit may pick any bound device.\n\n\
EXAMPLES:\n\
  super-image-worker create super.img -n new_part -g qti_dynamic_partitions_a\n\
  super-image-worker create super.img -n new_part -g new_group --group-size 4G --size 1M\n\
  super-image-worker create super.img -n empty_b -g default --size 0\n\
  super-image-worker create super.img -n big_a -g qti_dynamic_partitions_a --size 2G --force"
)]
pub struct CreateArgs {
    /// Path to super image (raw format only)
    pub image: PathBuf,

    /// Partition name to create
    #[arg(short, long)]
    pub name: String,

    /// Metadata slot (-s) whose table grows: 0|a, 1|b, ... or all
    /// (default: all). With `all` exactly one valid slot must exist.
    #[arg(short = 's', long, default_value = "all")]
    pub slot: String,

    /// Group name for the partition
    #[arg(short, long)]
    pub group: String,

    /// Initial size (0 for empty partition, or with K/M/G suffix)
    #[arg(long, default_value = "0")]
    pub size: String,

    /// Partition attributes (comma-separated)
    /// Available: readonly, slot_suffixed
    #[arg(long, default_value = "readonly")]
    pub attrs: String,

    /// Create a new group with this max_size if group doesn't exist
    /// Format: same as --size (e.g., 4G, 1073741824)
    #[arg(long)]
    pub group_size: Option<String>,

    /// Bind secondary block devices for split/retrofit images.
    /// Repeatable: `--device vendor=path` or `--device path` (auto-match).
    #[arg(long = "device")]
    pub device: Vec<String>,

    /// Allow exceeding the group's maximum_size limit
    #[arg(long)]
    pub force: bool,

    /// Dry run - show what would be created without modifying image
    #[arg(long)]
    pub dry_run: bool,
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() || s == "0" {
        return Ok(0);
    }
    let (num_part, multiplier) = if let Some(rest) = s.strip_suffix(['G', 'g']) {
        (rest, 1024 * 1024 * 1024)
    } else if let Some(rest) = s.strip_suffix(['M', 'm']) {
        (rest, 1024 * 1024)
    } else if let Some(rest) = s.strip_suffix(['K', 'k']) {
        (rest, 1024)
    } else {
        (s, 1)
    };
    let num: u64 = num_part.parse().map_err(|_| format!("invalid size: {s}"))?;
    num.checked_mul(multiplier)
        .ok_or_else(|| format!("size overflow: {s}"))
}

pub fn run(args: CreateArgs) -> std::process::ExitCode {
    // --slot picks the metadata table that grows (no suffix guessing).
    let slot_idx = match split_util::parse_slot_opt(&args.slot) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let datas = match split_util::load_for_slot_filter(&args.image, slot_idx) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let slot_pos = match split_util::resolve_new_slot(&datas, slot_idx) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let mut data = match datas.get(slot_pos).cloned() {
        Some(d) => d,
        None => {
            eprintln!("error: metadata slot not found");
            return std::process::ExitCode::FAILURE;
        }
    };

    if data.image_format != "raw" {
        eprintln!("error: only raw images are supported");
        return std::process::ExitCode::FAILURE;
    }

    if data.partitions.iter().any(|p| p.name == args.name) {
        eprintln!("error: partition '{}' already exists", args.name);
        return std::process::ExitCode::FAILURE;
    }

    let size = match parse_size(&args.size) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    if args.dry_run {
        println!(
            "would create partition '{}' in group '{}' size={}",
            args.name, args.group, size
        );
        return std::process::ExitCode::SUCCESS;
    }

    if let Some(ref group_max) = args.group_size {
        let max_size = match parse_size(group_max) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: {e}");
                return std::process::ExitCode::FAILURE;
            }
        };
        if !data.groups.iter().any(|g| g.name == args.group) {
            data.groups.push(super_image_worker_core::Group {
                name: args.group.clone(),
                flags: 0,
                maximum_size: max_size,
            });
            println!("created group '{}' max_size={}", args.group, max_size);
        }
    }

    let group_index = match data.groups.iter().position(|g| g.name == args.group) {
        Some(i) => i as u32,
        None => {
            eprintln!(
                "error: group '{}' not found. Use --group-size to create it",
                args.group
            );
            return std::process::ExitCode::FAILURE;
        }
    };

    let mut attributes = 0u32;
    for attr in args.attrs.split(',') {
        match attr.trim() {
            "readonly" => attributes |= super_image_worker_core::LP_PARTITION_ATTR_READONLY,
            "slot_suffixed" => attributes |= super_image_worker_core::LP_PARTITION_ATTR_SLOT_SUFFIXED,
            "" => {}
            other => {
                eprintln!("error: unknown attribute '{other}'");
                return std::process::ExitCode::FAILURE;
            }
        }
    }

    let (first_extent_index, num_extents) = if size > 0 {
        let needed_sectors = size.div_ceil(LP_SECTOR_SIZE);
        // Group maximum_size enforcement.
        if let Some(g) = data.groups.get(group_index as usize)
            && g.maximum_size > 0
        {
            let mut used: u64 = 0;
            for p in &data.partitions {
                if p.group_index == group_index {
                    used = used.saturating_add(data.partition_sectors(p));
                }
            }
            let max_sectors = g.maximum_size / LP_SECTOR_SIZE;
            if used.saturating_add(needed_sectors) > max_sectors && !args.force {
                eprintln!(
                    "error: would exceed group '{}' max_size ({} bytes, {} sectors used + {} needed > {} max). Use --force to override",
                    g.name, g.maximum_size, used, needed_sectors, max_sectors
                );
                return std::process::ExitCode::FAILURE;
            }
        }
        let writer = LpWriter::new();
        let split = data.devices.len() > 1 || !args.device.is_empty();
        if split
            && let Err(e) =
                super_image_worker_core::resolve_device_bindings(&data.devices, &args.device, &args.image)
        {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
        let (target_source, phys_sector) = if split {
            match writer.find_free_sectors_any(&data.extents, &data.devices, needed_sectors) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("error finding free space: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            }
        } else {
            match writer.find_free_sectors(&data.extents, &data.devices, 0, needed_sectors) {
                Ok(s) => (0u32, s),
                Err(e) => {
                    eprintln!("error finding free space: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            }
        };

        let ext_idx = data.extents.len() as u32;
        data.extents.push(super_image_worker_core::Extent {
            num_sectors: needed_sectors,
            target_type: LP_TARGET_TYPE_LINEAR,
            target_data: phys_sector,
            target_source,
        });
        (ext_idx, 1)
    } else {
        (data.extents.len() as u32, 0)
    };

    data.partitions.push(super_image_worker_core::Partition {
        name: args.name.clone(),
        attributes,
        first_extent_index,
        num_extents,
        group_index,
    });

    let mut file = match File::options().read(true).write(true).open(&args.image) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let writer = LpWriter::new();
    if let Err(e) = writer.write_metadata(
        &mut file,
        &data.geometry,
        data.metadata_offset,
        &data.header,
        &data.partitions,
        &data.extents,
        &data.groups,
        &data.devices,
    ) {
        eprintln!("error writing metadata: {e}");
        return std::process::ExitCode::FAILURE;
    }

    println!(
        "created partition '{}' in group '{}' size={}",
        args.name, args.group, size
    );
    std::process::ExitCode::SUCCESS
}
