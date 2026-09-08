use super::split_util;
use clap::Args;
use std::collections::HashSet;
use std::fs::File;
use std::path::PathBuf;

#[derive(Args)]
#[command(
    about = "Delete OTA snapshot COW partitions (lptools --clear-cow analog)",
    long_about = "Remove Virtual A/B OTA snapshot leftovers from LP metadata.\n\n\
Mirrors `lptools_new --clear-cow`: in every loaded metadata slot it finds\n\
the group literally named `cow` and deletes member partitions whose names\n\
end in `-cow` (e.g. `system_a-cow`), then rewrites that slot's primary +\n\
backup copies (neighbouring slots untouched). Raw images and raw block\n\
devices only. No root needed for the metadata edit itself.\n\n\
SAFETY GATE (merge-in-progress protection): lptools queries the\n\
bootcontrol HAL for snapshot merge status, which a static binary cannot\n\
reach. Instead this command refuses to run while any `-cow` device is\n\
currently mapped under /dev/block/mapper (a live snapuserd/dm-snapshot\n\
means an update or merge is active) unless --force is passed. On hosts\n\
without /dev/block/mapper the gate trivially passes.\n\n\
SELECTION: --slot <index|all> (default all) picks the metadata copies;\n\
-s/--slot accepts 0|a, 1|b, ... aliases. --dry-run lists what would be\n\
deleted. Exit 0 also when there is nothing to delete.\n\n\
EXAMPLES:\n\
  super-image-worker clear-cow super.img\n\
  super-image-worker clear-cow /dev/block/by-name/super -s 0\n\
  super-image-worker clear-cow super.img --dry-run\n\
  super-image-worker clear-cow super.img --force   # Ignore mapped -cow devices"
)]
pub struct CowArgs {
    /// Path to super image (raw format) or block device
    pub image: PathBuf,

    /// Metadata slot(s) to clean: 0|a, 1|b, ... or all (default: all)
    #[arg(short = 's', long, default_value = "all")]
    pub slot: String,

    /// List what would be deleted without modifying anything
    #[arg(long)]
    pub dry_run: bool,

    /// Delete even while -cow devices are mapped (live merge danger)
    #[arg(long)]
    pub force: bool,
}

/// Names of currently mapped `-cow` dm devices, if any.
/// Absent /dev/block/mapper (host Linux) yields an empty list.
fn mapped_cow_devices() -> Vec<String> {
    let mut out = Vec::new();
    let dir = match std::fs::read_dir("/dev/block/mapper") {
        Ok(d) => d,
        Err(_) => return out,
    };
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with("-cow") {
            out.push(name);
        }
    }
    out.sort();
    out
}

pub fn run(args: CowArgs) -> std::process::ExitCode {
    let slot_idx = match split_util::parse_slot_opt(&args.slot) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let mut datas = match split_util::load_for_slot_filter(&args.image, slot_idx) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // Per-slot COW inventory: (slot_position, partition_names).
    let mut plan: Vec<(usize, Vec<String>)> = Vec::new();
    for (pos, data) in datas.iter().enumerate() {
        if data.image_format != "raw" {
            eprintln!("error: only raw images are supported");
            return std::process::ExitCode::FAILURE;
        }
        let in_cow_group = |p: &super_image_worker_core::Partition| {
            data.groups
                .get(p.group_index as usize)
                .is_some_and(|g| g.name == "cow")
        };
        let doomed: Vec<String> = data
            .partitions
            .iter()
            .filter(|p| in_cow_group(p) && p.name.ends_with("-cow"))
            .map(|p| p.name.clone())
            .collect();
        if !doomed.is_empty() {
            plan.push((pos, doomed));
        }
    }

    if plan.is_empty() {
        println!("no COW partitions found (group `cow` with `*-cow` entries)");
        return std::process::ExitCode::SUCCESS;
    }

    // Merge-in-progress gate: refuse while -cow devices are mapped.
    let mapped = mapped_cow_devices();
    if !mapped.is_empty() && !args.force {
        eprintln!("error: refusing to clear COW while snapshot devices are mapped (merge/update may be active): {mapped:?}");
        eprintln!("unmap them first or pass --force (dangerous during a live merge)");
        return std::process::ExitCode::FAILURE;
    }

    if args.dry_run {
        for (pos, names) in &plan {
            let slot = datas
                .get(*pos)
                .map(|d| d.metadata_slot)
                .unwrap_or(u64::MAX);
            for n in names {
                println!("would delete '{n}' from metadata slot {slot}");
            }
        }
        return std::process::ExitCode::SUCCESS;
    }

    let mut file = match File::options().read(true).write(true).open(&args.image) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let writer = super_image_worker_core::LpWriter::new();

    for (pos, names) in &plan {
        let doomed: HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
        let data = match datas.get_mut(*pos) {
            Some(d) => d,
            None => {
                eprintln!("error: metadata slot not found");
                return std::process::ExitCode::FAILURE;
            }
        };
        // Descending-first-extent drain keeps indices valid (same scheme
        // as group-cascade removal).
        let mut ranges: Vec<(u32, u32)> = data
            .partitions
            .iter()
            .filter(|p| doomed.contains(p.name.as_str()))
            .map(|p| (p.first_extent_index, p.num_extents))
            .collect();
        ranges.sort_by_key(|r| std::cmp::Reverse(r.0));
        let mut freed: usize = 0;
        for (first, num) in &ranges {
            let f = *first as usize;
            let n = *num as usize;
            if f.saturating_add(n) > data.extents.len() {
                eprintln!("error: partition extent range out of bounds (corrupt metadata)");
                return std::process::ExitCode::FAILURE;
            }
            data.extents.drain(f..f + n);
            for p in data.partitions.iter_mut() {
                if (p.first_extent_index as usize) > f {
                    p.first_extent_index = p.first_extent_index.saturating_sub(n as u32);
                }
            }
            freed += n;
        }
        let before = data.partitions.len();
        data.partitions.retain(|p| !doomed.contains(p.name.as_str()));
        let removed = before - data.partitions.len();
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
            "slot {}: removed {removed} COW partition(s), freed {freed} extent(s): {:?}",
            data.metadata_slot, names
        );
    }

    std::process::ExitCode::SUCCESS
}
