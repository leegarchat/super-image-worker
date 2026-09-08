use super::split_util;
use clap::Args;
use super_image_worker_core::LpWriter;
use std::fs::File;
use std::path::PathBuf;

#[derive(Args)]
#[command(
    about = "Rename a partition or group",
    long_about = "Rename a partition or group in super image LP metadata.\n\n\
Only rewrites the 36-byte name field (LP_PARTITION_NAME_MAX_LENGTH) and\n\
refreshes SHA-256 checksums - extents, payload and indices are untouched.\n\
The new name must be unique in its table and at most 36 bytes. --group\n\
renames a group instead of a partition. --dry-run previews. Raw images\n\
only. No root needed.\n\n\
EXAMPLES:\n\
  super-image-worker rename super.img old_name new_name\n\
  super-image-worker rename super.img old_group new_group --group\n\
  super-image-worker rename super.img test_a test_b --dry-run"
)]
pub struct RenameArgs {
    /// Path to super image (raw format only)
    pub image: PathBuf,

    /// Current partition or group name
    pub old_name: String,

    /// New name (max 36 characters)
    pub new_name: String,

    /// Rename a group instead of partition
    #[arg(long)]
    pub group: bool,

    /// Metadata slot (-s) to edit: 0|a, 1|b, ... or all (default: all).
    /// With `all` the current name must resolve in exactly one slot.
    #[arg(short = 's', long, default_value = "all")]
    pub slot: String,

    /// Extra name filter (long flag only): a, b, or all (default: all).
    /// Only affects base-name resolution; exact names match directly.
    #[arg(long, default_value = "all")]
    pub suffix: String,

    /// Dry run - show what would change without modifying image
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run(args: RenameArgs) -> std::process::ExitCode {
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
    // Groups resolve by exact name; partitions through the resolver
    // (exactly one owning slot required).
    let slot_pos = if args.group {
        match super::remove::select_group_slot(&datas, &args.old_name, slot_idx) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        match split_util::resolve_write_slot(&datas, &args.old_name, suffix, slot_idx) {
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

    if args.new_name.len() > 36 {
        eprintln!("error: name too long (max 36 chars)");
        return std::process::ExitCode::FAILURE;
    }

    if args.group {
        if data.groups.iter().any(|g| g.name == args.new_name) {
            eprintln!("error: group '{}' already exists", args.new_name);
            return std::process::ExitCode::FAILURE;
        }
        let group = match data.groups.iter_mut().find(|g| g.name == args.old_name) {
            Some(g) => g,
            None => {
                eprintln!("error: group '{}' not found", args.old_name);
                return std::process::ExitCode::FAILURE;
            }
        };
        if args.dry_run {
            println!(
                "would rename group '{}' -> '{}'",
                args.old_name, args.new_name
            );
            return std::process::ExitCode::SUCCESS;
        }
        group.name = args.new_name.clone();
    } else {
        if data.partitions.iter().any(|p| p.name == args.new_name) {
            eprintln!("error: partition '{}' already exists", args.new_name);
            return std::process::ExitCode::FAILURE;
        }
        let part = match data.partitions.iter_mut().find(|p| p.name == args.old_name) {
            Some(p) => p,
            None => {
                eprintln!("error: partition '{}' not found", args.old_name);
                return std::process::ExitCode::FAILURE;
            }
        };
        if args.dry_run {
            println!(
                "would rename partition '{}' -> '{}'",
                args.old_name, args.new_name
            );
            return std::process::ExitCode::SUCCESS;
        }
        part.name = args.new_name.clone();
    }

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

    println!("renamed '{}' -> '{}'", args.old_name, args.new_name);
    std::process::ExitCode::SUCCESS
}
