use super::split_util;
use clap::Args;
use super_image_worker_core::{LP_SECTOR_SIZE, LP_TARGET_TYPE_LINEAR, LpWriter};
use std::fs::File;
use std::path::PathBuf;

#[derive(Args)]
#[command(
    about = "Resize an existing partition",
    long_about = "Change the size of an existing partition in super image.\n\n\
GROW: if the sectors right after the last extent are free AND inside the\n\
device, the last extent is expanded in place; otherwise a new linear extent\n\
is allocated via free-space search and spliced into the partition's extent\n\
window (first_extent_index of later partitions is shifted) - never\n\
overwriting the neighbour's blocks. Extent-less partitions (empty slot B)\n\
get a fresh extent. SHRINK (--allow-shrink): trailing extents are truncated\n\
or dropped so the total equals the target; shrinking to 0 removes all extents.\n\
Data itself is never moved. Raw images only. No root needed.\n\n\
NAME RESOLUTION: full name (system_a) or base name plus --slot (system -s a).\n\n\
SIZE FORMAT: plain bytes (1048576), K/M/G suffixes upper or lower (512M, 2g),\n\
or sectors with trailing s (2048s). New size is rounded UP to whole sectors.\n\n\
GROUP LIMITS: the group's maximum_size is enforced (siblings + new size <= max)\n\
unless --force. Shrinking below current size additionally needs --allow-shrink.\n\
--dry-run prints old -> new without touching the image.\n\n\
SPLIT / RETROFIT: repeatable --device; growth may land on any bound device.\n\
Metadata is rewritten (primary + backup of the loaded slot) in the main file.\n\n\
EXAMPLES:\n\
  super-image-worker resize super.img system_a 2G\n\
  super-image-worker resize super.img system -s a 900M        # Base name + slot\n\
  super-image-worker resize super.img vendor_a 512M --allow-shrink\n\
  super-image-worker resize super.img system_a 4G --force     # Over group max_size\n\
  super-image-worker resize super.img system_a 1G --dry-run"
)]
pub struct ResizeArgs {
    /// Path to super image (raw format only)
    pub image: PathBuf,

    /// Partition name to resize (base name allowed with --slot)
    pub name: String,

    /// New size in bytes, or with suffix: K, M, G, s (sectors)
    pub size: String,

    /// Filter by slot suffix: a, b, or all (for base-name resolution)
    #[arg(short, long, default_value = "all")]
    pub slot: String,

    /// Bind secondary block devices for split/retrofit images.
    /// Repeatable: `--device vendor=path` or `--device path` (auto-match).
    #[arg(long = "device")]
    pub device: Vec<String>,

    /// Force resize even if it exceeds group maximum_size limit
    #[arg(long)]
    pub force: bool,

    /// Allow shrinking the partition (reducing size)
    #[arg(long)]
    pub allow_shrink: bool,

    /// Dry run - show what would change without modifying image
    #[arg(long)]
    pub dry_run: bool,
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty size".into());
    }
    let (num_part, multiplier) = if let Some(rest) = s.strip_suffix(['G', 'g']) {
        (rest, 1024u64 * 1024 * 1024)
    } else if let Some(rest) = s.strip_suffix(['M', 'm']) {
        (rest, 1024u64 * 1024)
    } else if let Some(rest) = s.strip_suffix(['K', 'k']) {
        (rest, 1024u64)
    } else if let Some(rest) = s.strip_suffix('s') {
        (rest, LP_SECTOR_SIZE)
    } else {
        (s, 1)
    };
    let num: u64 = num_part.parse().map_err(|_| format!("invalid size: {s}"))?;
    num.checked_mul(multiplier)
        .ok_or_else(|| "size overflow".to_string())
}

#[allow(clippy::collapsible_if)]
pub fn run(args: ResizeArgs) -> std::process::ExitCode {
    let slot_opt = match args.slot.as_str() {
        "a" | "A" => Some("a"),
        "b" | "B" => Some("b"),
        "all" => None,
        other => {
            eprintln!("unknown slot: {other} (use a, b, all)");
            return std::process::ExitCode::FAILURE;
        }
    };
    // Slot-aware: `system_b` (or `-s b`) edits metadata slot 1,
    // so dual-slot images no longer hit "not found" via slot 0.
    let mut data = match split_util::load_for_write(&args.image, &args.name, slot_opt) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    if data.image_format != "raw" {
        eprintln!("error: only raw images are supported");
        return std::process::ExitCode::FAILURE;
    }

    let new_size = match parse_size(&args.size) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let slot = slot_opt;

    let part_idx = match data.resolve_partition(&args.name, slot) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let part_name = match data.partitions.get(part_idx) {
        Some(p) => p.name.clone(),
        None => {
            eprintln!("error: partition index out of range");
            return std::process::ExitCode::FAILURE;
        }
    };

    let current_size = match data.partitions.get(part_idx) {
        Some(p) => data.partition_size(p),
        None => {
            eprintln!("error: partition index out of range");
            return std::process::ExitCode::FAILURE;
        }
    };
    if new_size < current_size && !args.allow_shrink {
        eprintln!(
            "error: new size ({new_size}) < current size ({current_size}). Use --allow-shrink"
        );
        return std::process::ExitCode::FAILURE;
    }
    if new_size == current_size {
        println!("{part_name}: size unchanged ({current_size} bytes)");
        return std::process::ExitCode::SUCCESS;
    }

    let (group_index, group_name, group_max) = match data.partitions.get(part_idx) {
        Some(p) => match data.groups.get(p.group_index as usize) {
            Some(g) => (p.group_index, g.name.clone(), g.maximum_size),
            None => {
                eprintln!("error: partition group index out of range");
                return std::process::ExitCode::FAILURE;
            }
        },
        None => {
            eprintln!("error: partition index out of range");
            return std::process::ExitCode::FAILURE;
        }
    };

    let new_sectors = new_size.div_ceil(LP_SECTOR_SIZE);

    if !args.force && group_max > 0 {
        let mut other_sectors: u64 = 0;
        for other in &data.partitions {
            if other.group_index == group_index && other.name != part_name {
                other_sectors = other_sectors.saturating_add(data.partition_sectors(other));
            }
        }
        let group_max_sectors = group_max / LP_SECTOR_SIZE;
        if other_sectors.saturating_add(new_sectors) > group_max_sectors {
            eprintln!(
                "error: would exceed group '{group_name}' max_size ({group_max}). Use --force to override"
            );
            return std::process::ExitCode::FAILURE;
        }
    }

    if args.dry_run {
        println!("resize {part_name}: {current_size} -> {new_size} bytes ({new_sectors} sectors)");
        return std::process::ExitCode::SUCCESS;
    }

    let writer = LpWriter::new();

    // Snapshot partition extent window.
    let (first, num) = match data.partitions.get(part_idx) {
        Some(p) => (p.first_extent_index as u64, p.num_extents as u64),
        None => {
            eprintln!("error: partition index out of range");
            return std::process::ExitCode::FAILURE;
        }
    };
    let current_sectors = match data.partitions.get(part_idx) {
        Some(p) => data.partition_sectors(p),
        None => 0,
    };

    if new_sectors > current_sectors {
        // GROW.
        let extra = new_sectors - current_sectors;
        // Validate split bindings (metadata is primary-only, but allocation
        // may target any device).
        if data.devices.len() > 1 || !args.device.is_empty() {
            if let Err(e) =
                super_image_worker_core::resolve_device_bindings(&data.devices, &args.device, &args.image)
            {
                eprintln!("error: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
        if num == 0 {
            // Extent-less partition (e.g. empty slot B): allocate fresh extent.
            let (src, phys) =
                match writer.find_free_sectors_any(&data.extents, &data.devices, new_sectors) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("error finding free space: {e}");
                        return std::process::ExitCode::FAILURE;
                    }
                };
            let insert_at = data.extents.len() as u64;
            data.extents.push(super_image_worker_core::Extent {
                num_sectors: new_sectors,
                target_type: LP_TARGET_TYPE_LINEAR,
                target_data: phys,
                target_source: src,
            });
            if let Some(p) = data.partitions.get_mut(part_idx) {
                p.first_extent_index = insert_at as u32;
                p.num_extents = 1;
            }
            println!(
                "{part_name}: allocated new extent ({new_sectors} sectors at {phys} on device {src})"
            );
        } else {
            // Try in-place expansion of the last extent.
            let last_idx = first + num - 1;
            let last = match data.extents.get(last_idx as usize).cloned() {
                Some(e) => e,
                None => {
                    eprintln!("error: extent index out of range");
                    return std::process::ExitCode::FAILURE;
                }
            };
            if last.target_type != LP_TARGET_TYPE_LINEAR {
                eprintln!("error: last extent is not linear, cannot grow in place");
                return std::process::ExitCode::FAILURE;
            }
            let grow_start = match last.target_data.checked_add(last.num_sectors) {
                Some(v) => v,
                None => {
                    eprintln!("error: extent end overflow");
                    return std::process::ExitCode::FAILURE;
                }
            };
            let free_in_place = writer
                .is_range_free(
                    &data.extents,
                    &data.devices,
                    last.target_source,
                    grow_start,
                    extra,
                )
                .unwrap_or(false);
            if free_in_place {
                if let Some(e) = data.extents.get_mut(last_idx as usize) {
                    e.num_sectors = e.num_sectors.saturating_add(extra);
                }
                println!("{part_name}: expanded last extent in place by {extra} sectors");
            } else {
                // Allocate a new linear extent and insert it right after the
                // partition's window; shift later partitions.
                let phys = match writer.find_free_sectors(
                    &data.extents,
                    &data.devices,
                    last.target_source,
                    extra,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("error finding free space: {e}");
                        return std::process::ExitCode::FAILURE;
                    }
                };
                let insert_at = (first + num) as usize;
                let new_ext = super_image_worker_core::Extent {
                    num_sectors: extra,
                    target_type: LP_TARGET_TYPE_LINEAR,
                    target_data: phys,
                    target_source: last.target_source,
                };
                if insert_at > data.extents.len() {
                    eprintln!("error: extent insert position out of range");
                    return std::process::ExitCode::FAILURE;
                }
                data.extents.insert(insert_at, new_ext);
                // Update self + shift subsequent partitions.
                let nparts = data.partitions.len();
                for i in 0..nparts {
                    if i == part_idx {
                        if let Some(p) = data.partitions.get_mut(i) {
                            p.num_extents = p.num_extents.saturating_add(1);
                        }
                    } else if let Some(p_first) = data
                        .partitions
                        .get(i)
                        .map(|p| p.first_extent_index as usize)
                    {
                        if p_first >= insert_at {
                            if let Some(p) = data.partitions.get_mut(i) {
                                p.first_extent_index = p.first_extent_index.saturating_add(1);
                            }
                        }
                    }
                }
                println!(
                    "{part_name}: allocated new extent ({extra} sectors at {phys}), collision avoided"
                );
            }
        }
    } else {
        // SHRINK: truncate trailing extents to exactly new_sectors.
        if new_sectors == 0 {
            // Drop all extents of this partition.
            let rm_start = first as usize;
            let rm_count = num as usize;
            if rm_start.saturating_add(rm_count) > data.extents.len() {
                eprintln!("error: extent range out of bounds");
                return std::process::ExitCode::FAILURE;
            }
            data.extents.drain(rm_start..rm_start + rm_count);
            if let Some(p) = data.partitions.get_mut(part_idx) {
                p.first_extent_index = rm_start as u32;
                p.num_extents = 0;
            }
            let nparts = data.partitions.len();
            for i in 0..nparts {
                if i == part_idx {
                    continue;
                }
                if let Some(p_first) = data
                    .partitions
                    .get(i)
                    .map(|p| p.first_extent_index as usize)
                {
                    if p_first > rm_start {
                        if let Some(p) = data.partitions.get_mut(i) {
                            p.first_extent_index =
                                p.first_extent_index.saturating_sub(rm_count as u32);
                        }
                    }
                }
            }
            println!("{part_name}: shrunk to zero (extents removed)");
        } else if num == 0 {
            eprintln!("error: partition has no extents");
            return std::process::ExitCode::FAILURE;
        } else {
            // Walk extents, keep prefix totalling new_sectors.
            let mut keep: u64 = new_sectors;
            let mut new_num: u64 = 0;
            let mut idx = first;
            for _ in 0..num {
                let e_sectors = match data.extents.get(idx as usize) {
                    Some(e) => e.num_sectors,
                    None => break,
                };
                if keep == 0 {
                    break;
                }
                if e_sectors <= keep {
                    keep -= e_sectors;
                    new_num += 1;
                    idx += 1;
                } else {
                    // Truncate this extent and drop the rest.
                    if let Some(e) = data.extents.get_mut(idx as usize) {
                        e.num_sectors = keep;
                    }
                    new_num += 1;
                    break;
                }
            }
            // Remove surplus trailing extents within the window.
            let surplus = num - new_num;
            if surplus > 0 {
                let rm_start = (first + new_num) as usize;
                let rm_end = (first + num) as usize;
                if rm_end > data.extents.len() || rm_start > rm_end {
                    eprintln!("error: extent range out of bounds");
                    return std::process::ExitCode::FAILURE;
                }
                data.extents.drain(rm_start..rm_end);
                if let Some(p) = data.partitions.get_mut(part_idx) {
                    p.num_extents = new_num as u32;
                }
                let nparts = data.partitions.len();
                for i in 0..nparts {
                    if i == part_idx {
                        continue;
                    }
                    if let Some(p_first) = data
                        .partitions
                        .get(i)
                        .map(|p| p.first_extent_index as usize)
                    {
                        if p_first >= rm_end {
                            if let Some(p) = data.partitions.get_mut(i) {
                                p.first_extent_index =
                                    p.first_extent_index.saturating_sub(surplus as u32);
                            }
                        }
                    }
                }
            } else if let Some(p) = data.partitions.get_mut(part_idx) {
                p.num_extents = new_num as u32;
            }
            println!("{part_name}: shrunk to {new_size} bytes ({new_sectors} sectors)");
        }
    }

    let mut file = match File::options().read(true).write(true).open(&args.image) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

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

    println!("resized {part_name}: {current_size} -> {new_size} bytes");
    std::process::ExitCode::SUCCESS
}
