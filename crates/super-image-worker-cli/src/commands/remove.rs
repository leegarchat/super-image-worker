use super::split_util;
use clap::Args;
use std::fs::File;
use std::path::PathBuf;

#[derive(Args)]
#[command(
    about = "Remove a partition or group",
    long_about = "Remove a partition or group from super image LP metadata.\n\n\
PARTITION MODE (default): drops the partition entry and cuts out its extents\n\
from the extent table; first_extent_index of later partitions is shifted down.\n\
Name accepts a full name (system_a) or a base name plus --suffix\n\
(system --suffix a); -s/--slot picks the metadata copy to edit.\n\
Payload bytes stay in place (they become free space for future allocations).\n\n\
GROUP MODE (--group): drops the group entry and renumbers group_index of\n\
survivors. Refuses non-empty groups unless --force cascades: with --force,\n\
EVERY partition of the group is deleted first (their extents freed in\n\
dependency-safe descending order), then the group itself. --dry-run previews.\n\
Raw images only. No root needed.\n\n\
EXAMPLES:\n\
  super-image-worker remove super.img my_partition\n\
  super-image-worker remove super.img system --suffix a\n\
  super-image-worker remove super.img my_group --group\n\
  super-image-worker remove super.img my_group --group --force   # Cascade + extents\n\
  super-image-worker remove super.img test_a --dry-run"
)]
pub struct RemoveArgs {
    /// Path to super image (raw format only)
    pub image: PathBuf,

    /// Partition or group name to remove (base name allowed with --suffix)
    pub name: String,

    /// Metadata slot (-s) to edit: 0|a, 1|b, ... or all (default: all).
    /// With `all` the entry must resolve in exactly one slot.
    #[arg(short = 's', long, default_value = "all")]
    pub slot: String,

    /// Extra name filter for base-name resolution (long flag only):
    /// a, b, or all (default: all, partition mode only)
    #[arg(long, default_value = "all")]
    pub suffix: String,

    /// Remove a group instead of partition
    /// Group must be empty (no partitions assigned), unless --force is given
    #[arg(long)]
    pub group: bool,

    /// Force removal: with --group, also delete all partitions of the group
    #[arg(long)]
    pub force: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,

    /// Dry run - show what would be removed without modifying image
    #[arg(long)]
    pub dry_run: bool,
}

/// Select the loaded slot holding a group for --group removal.
/// An explicit `--slot` index wins; otherwise the group name must
/// occur in exactly one loaded slot. Shared with `rename --group`.
pub(crate) fn select_group_slot(
    datas: &[super_image_worker_core::SuperData],
    name: &str,
    slot: Option<u64>,
) -> std::result::Result<usize, String> {
    if let Some(idx) = slot {
        let pos = datas
            .iter()
            .position(|d| d.metadata_slot == idx)
            .ok_or_else(|| format!("metadata slot {idx} not loaded"))?;
        if datas[pos].groups.iter().any(|g| g.name == name) {
            Ok(pos)
        } else {
            Err(format!("slot {idx}: group '{name}' not found"))
        }
    } else {
        let hits: Vec<usize> = datas
            .iter()
            .enumerate()
            .filter(|(_, d)| d.groups.iter().any(|g| g.name == name))
            .map(|(p, _)| p)
            .collect();
        match hits.len() {
            0 => Err(format!("group '{name}' not found")),
            1 => Ok(hits[0]),
            _ => {
                let slots: Vec<u64> = hits.iter().map(|p| datas[*p].metadata_slot).collect();
                Err(format!(
                    "group '{name}' exists in several metadata slots {slots:?}; pass --slot <index> to select one"
                ))
            }
        }
    }
}

pub fn run(args: RemoveArgs) -> std::process::ExitCode {
    // --slot picks the metadata copy (index), --suffix the name letters.
    let slot_idx = match split_util::parse_slot_opt(&args.slot) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let suffix = match split_util::parse_suffix_opt(&args.suffix) {
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
    // Select the owning slot: groups match by exact name, partitions
    // through the suffix-aware resolver (exactly one hit required).
    let slot_pos = if args.group {
        match select_group_slot(&datas, &args.name, slot_idx) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        match split_util::resolve_write_slot(&datas, &args.name, suffix, slot_idx) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: {e}");
                return std::process::ExitCode::FAILURE;
            }
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

    if args.group {
        let group_idx = match data.groups.iter().position(|g| g.name == args.name) {
            Some(i) => i,
            None => {
                eprintln!("error: group '{}' not found", args.name);
                return std::process::ExitCode::FAILURE;
            }
        };

        let deps: Vec<&str> = data
            .partitions
            .iter()
            .filter(|p| p.group_index == group_idx as u32)
            .map(|p| p.name.as_str())
            .collect();

        if !deps.is_empty() && !args.force {
            eprintln!(
                "error: group '{}' has {} partition(s): {:?}",
                args.name,
                deps.len(),
                deps
            );
            eprintln!("remove partitions first or use --force");
            return std::process::ExitCode::FAILURE;
        }

        if args.dry_run {
            if args.force && !deps.is_empty() {
                println!(
                    "would remove group '{}' with {} partition(s): {:?}",
                    args.name,
                    deps.len(),
                    deps
                );
            } else {
                println!("would remove group '{}'", args.name);
            }
            return std::process::ExitCode::SUCCESS;
        }

        if args.force && !deps.is_empty() {
            // Cascade: collect extent ranges of doomed partitions (descending
            // order keeps indices valid while draining), then drop them.
            let mut doomed: Vec<(u32, u32)> = data
                .partitions
                .iter()
                .filter(|p| p.group_index == group_idx as u32)
                .map(|p| (p.first_extent_index, p.num_extents))
                .collect();
            doomed.sort_by_key(|a| std::cmp::Reverse(a.0));
            let mut freed_extents: usize = 0;
            for (first, num) in &doomed {
                let f = *first as usize;
                let n = *num as usize;
                if f.saturating_add(n) > data.extents.len() {
                    eprintln!("error: partition extent range out of bounds (corrupt metadata)");
                    return std::process::ExitCode::FAILURE;
                }
                data.extents.drain(f..f + n);
                // Shift extent indices of partitions located after the hole.
                for p in &mut data.partitions {
                    if (p.first_extent_index as usize) > f {
                        p.first_extent_index = p.first_extent_index.saturating_sub(n as u32);
                    }
                }
                // Adjust not-yet-processed (lower) ranges: unaffected since we
                // go descending. No-op by construction.
                freed_extents += n;
            }
            let before = data.partitions.len();
            data.partitions
                .retain(|p| p.group_index != group_idx as u32);
            let removed_parts = before - data.partitions.len();
            data.groups.remove(group_idx);
            for p in &mut data.partitions {
                if p.group_index > group_idx as u32 {
                    p.group_index = p.group_index.saturating_sub(1);
                }
            }
            println!(
                "removed group '{}' with {} partition(s), freed {} extent(s)",
                args.name, removed_parts, freed_extents
            );
            // Fall through to metadata write below.
        } else {
            data.groups.remove(group_idx);

            for p in &mut data.partitions {
                if p.group_index > group_idx as u32 {
                    p.group_index = p.group_index.saturating_sub(1);
                }
            }
        }
    } else {
        let part_idx = match data.resolve_partition(&args.name, suffix) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("error: {e}");
                return std::process::ExitCode::FAILURE;
            }
        };
        let resolved_name = match data.partitions.get(part_idx) {
            Some(p) => p.name.clone(),
            None => {
                eprintln!("error: partition index out of range");
                return std::process::ExitCode::FAILURE;
            }
        };

        if args.dry_run {
            println!("would remove partition '{resolved_name}'");
            return std::process::ExitCode::SUCCESS;
        }

        let removed = data.partitions.remove(part_idx);
        let first_ext = removed.first_extent_index as usize;
        let num_ext = removed.num_extents as usize;
        if first_ext.saturating_add(num_ext) > data.extents.len() {
            eprintln!("error: partition extent range out of bounds (corrupt metadata)");
            return std::process::ExitCode::FAILURE;
        }

        for _ in 0..num_ext {
            if first_ext < data.extents.len() {
                data.extents.remove(first_ext);
            } else {
                eprintln!("error: extent index out of range");
                return std::process::ExitCode::FAILURE;
            }
        }

        for p in &mut data.partitions {
            if (p.first_extent_index as usize) > first_ext {
                p.first_extent_index = p.first_extent_index.saturating_sub(num_ext as u32);
            }
        }
        println!("removed partition '{resolved_name}' ({} extents)", num_ext);
    }

    let mut file = match File::options().read(true).write(true).open(&args.image) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let writer = super_image_worker_core::LpWriter::new();
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

    if args.group {
        println!("removed group '{}'", args.name);
    }
    std::process::ExitCode::SUCCESS
}
