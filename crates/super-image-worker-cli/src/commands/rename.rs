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

    /// Target metadata slot for suffix-less names (a, b; default: auto
    /// by `_a`/`_b` suffix of the current name). Needed to address a
    /// specific slot on multi-slot images.
    #[arg(short, long, default_value = "all")]
    pub slot: String,

    /// Dry run - show what would change without modifying image
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run(args: RenameArgs) -> std::process::ExitCode {
    let slot_opt: Option<&str> = match args.slot.as_str() {
        "a" | "A" => Some("a"),
        "b" | "B" => Some("b"),
        "all" => None,
        other => {
            eprintln!("unknown slot: {other} (use a, b, all)");
            return std::process::ExitCode::FAILURE;
        }
    };
    let mut data = match split_util::load_for_write(&args.image, &args.old_name, slot_opt) {
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
