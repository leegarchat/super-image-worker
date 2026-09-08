use super::split_util;
use clap::Args;
use super_image_worker_core::{LP_SECTOR_SIZE, LP_TARGET_TYPE_LINEAR, LpWriter};
use std::fs::File;
use std::path::PathBuf;

#[derive(Args)]
#[command(
    about = "Add a new partition with payload data",
    long_about = "Add a new partition to super image and write payload data.\n\n\
Creates the partition entry, allocates first-fit free sectors (honouring\n\
per-device alignment AND alignment_offset), streams the payload file in\n\
1 MiB chunks (O(1) RAM - safe for multi-GB system.img on 32-bit and ramdisks),\n\
then rewrites metadata checksums (SHA-256) into the primary AND backup copies\n\
of the loaded slot only - neighbouring slots are never touched.\n\
Raw images only (never sparse). No root needed (plain file I/O).\n\n\
GROUP SELECTION: explicit --group wins; otherwise the group carrying the\n\
partition's slot suffix (_a/_b) is picked by vendor priority\n\
qti_* > google_* > samsung_*/sec_* > mtk_* > largest maximum_size.\n\
The group's maximum_size is enforced (used + needed <= max): exceeding it\n\
fails unless --force is passed.\n\n\
ATTRIBUTES (--attrs, comma list): readonly, slot_suffixed (default: readonly).\n\n\
SPLIT / RETROFIT: repeatable --device (vendor=path or auto-matched path).\n\
Allocation is restricted to bound devices only (device 0 = main IMAGE plus\n\
every --device file), so a forgotten --device fails fast with a hint instead\n\
of writing into an unbound file. Payload lands in the allocated device's file;\n\
metadata always lives in the primary file.\n\n\
EXAMPLES:\n\
  super-image-worker add super.img -n my_part_a -p payload.img\n\
  super-image-worker add super.img -n custom -p data.bin -g my_group\n\
  super-image-worker add super.img -n test_a -p file.txt --attrs readonly,slot_suffixed\n\
  super-image-worker add super.img -n big_a -p big.img --force   # Over group max_size\n\
  super-image-worker add sys.img -n extra_a -p extra.img --device vendor=vend.img"
)]
pub struct AddArgs {
    /// Path to super image (must be raw format, not sparse)
    pub image: PathBuf,

    /// Partition name to add (e.g., my_partition_a)
    /// Suffix _a/_b determines default group selection
    #[arg(short, long)]
    pub name: String,

    /// Path to payload file (any format: img, bin, txt, etc.)
    /// File is streamed in chunks, never fully loaded into RAM
    #[arg(short, long)]
    pub payload: PathBuf,

    /// Group name for the partition (default: auto-select by slot suffix)
    #[arg(short, long)]
    pub group: Option<String>,

    /// Bind secondary block devices for split/retrofit images.
    /// Repeatable: `--device vendor=path` or `--device path` (auto-match).
    /// Payload is allocated first-fit across all bound devices.
    #[arg(long = "device")]
    pub device: Vec<String>,

    /// Partition attributes (comma-separated)
    /// Available: readonly, slot_suffixed
    #[arg(long, default_value = "readonly")]
    pub attrs: String,

    /// Allow exceeding the group's maximum_size limit
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: AddArgs) -> std::process::ExitCode {
    let mut data = match split_util::load_for_write(&args.image, &args.name, None) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    if data.image_format != "raw" {
        eprintln!("error: only raw images are supported for writing");
        return std::process::ExitCode::FAILURE;
    }

    if data.partitions.iter().any(|p| p.name == args.name) {
        eprintln!("error: partition '{}' already exists", args.name);
        return std::process::ExitCode::FAILURE;
    }

    let mut payload_file = match File::open(&args.payload) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error opening payload: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let payload_len = match payload_file.metadata() {
        Ok(m) => m.len(),
        Err(e) => {
            eprintln!("error stating payload: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    if payload_len == 0 {
        eprintln!("error: payload is empty");
        return std::process::ExitCode::FAILURE;
    }

    let needed_sectors = payload_len.div_ceil(LP_SECTOR_SIZE);

    // Intelligent group selection.
    let group_index: u32 = if let Some(ref gname) = args.group {
        match data.groups.iter().position(|g| g.name == *gname) {
            Some(idx) => idx as u32,
            None => {
                eprintln!("error: group '{gname}' not found");
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        match data.select_group_for_name(&args.name) {
            Some(idx) => {
                if let Some(g) = data.groups.get(idx as usize) {
                    println!("auto-selected group '{}' for '{}'", g.name, args.name);
                }
                idx
            }
            None => {
                eprintln!("error: no groups available in metadata");
                return std::process::ExitCode::FAILURE;
            }
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
    // Validate split bindings up-front (also used to locate the target file).
    let bindings = if split {
        match super_image_worker_core::resolve_device_bindings(&data.devices, &args.device, &args.image) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        std::collections::HashMap::new()
    };
    // Allocate only on devices actually bound on the command line (device 0
    // is always bound: it is the main IMAGE argument). This fails fast with
    // a `--device` hint instead of landing payload on an unbound file.
    let (target_source, phys_sector) = if split {
        let mut allowed: Vec<u32> = vec![0];
        let mut extra: Vec<u32> = bindings.keys().copied().collect();
        extra.sort_unstable();
        allowed.extend(extra);
        match writer.find_free_sectors_any_in(
            &data.extents,
            &data.devices,
            needed_sectors,
            &allowed,
        ) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error finding free space: {e}");
                if data.devices.len() > 1 {
                    eprintln!(
                        "hint: bind more block devices with --device <name>=<path> (see `info --device` for names)"
                    );
                }
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

    let phys_offset = match phys_sector.checked_mul(LP_SECTOR_SIZE) {
        Some(o) => o,
        None => {
            eprintln!("error: physical offset overflow");
            return std::process::ExitCode::FAILURE;
        }
    };
    let dev_name = data
        .devices
        .get(target_source as usize)
        .map(|d| d.partition_name.as_str())
        .unwrap_or("?");
    println!(
        "partition '{}' -> device={} phys_sector={} phys_offset=0x{:x} size={} bytes",
        args.name, dev_name, phys_sector, phys_offset, payload_len
    );

    let new_extent = super_image_worker_core::Extent {
        num_sectors: needed_sectors,
        target_type: LP_TARGET_TYPE_LINEAR,
        target_data: phys_sector,
        target_source,
    };

    let new_partition = super_image_worker_core::Partition {
        name: args.name.clone(),
        attributes,
        first_extent_index: data.extents.len() as u32,
        num_extents: 1,
        group_index,
    };

    data.partitions.push(new_partition);
    data.extents.push(new_extent);

    // Payload goes to the file backing the chosen device; metadata always
    // lives in the primary file (retrofit layout).
    let payload_path: PathBuf = if target_source == 0 {
        args.image.clone()
    } else {
        match bindings.get(&target_source) {
            Some(p) => p.clone(),
            None => {
                eprintln!(
                    "error: block device index {target_source} is not bound; pass --device <name>=<path>"
                );
                return std::process::ExitCode::FAILURE;
            }
        }
    };
    let mut payload_file_handle = match File::options().read(true).write(true).open(&payload_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error opening target file for writing: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let mut file = match File::options().read(true).write(true).open(&args.image) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error opening image for writing: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // O(1) RAM streaming write (1 MiB chunks + sector padding).
    match writer.copy_payload_stream(
        &mut payload_file_handle,
        phys_offset,
        &mut payload_file,
        payload_len,
    ) {
        Ok(n) => println!("payload written: {n} bytes at offset 0x{phys_offset:x}"),
        Err(e) => {
            eprintln!("error writing payload: {e}");
            return std::process::ExitCode::FAILURE;
        }
    }

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
    println!("metadata updated");

    println!("done. partition '{}' added to super image", args.name);
    std::process::ExitCode::SUCCESS
}
