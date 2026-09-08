mod commands;
mod output;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "super-image-worker",
    version,
    about = "AOSP Super image parser and manager",
    long_about = "super-image-worker - standalone Rust utility for working with Android super partition images.\n\n\
Supports raw and Android-sparse image formats. Reads, modifies, extracts, and manages\n\
logical partitions defined in LP metadata (liblp format), including split/retrofit\n\
multi-device layouts, and can generate new images from scratch (see `make`).\n\n\
QUICK WORKFLOWS:\n\
  super-image-worker info super.img                    # View partition layout\n\
  super-image-worker info super.img -f json | jq .     # Machine-readable layout\n\
  super-image-worker extract super.img -o ./out        # Unpack all partitions (lpunpack analog)\n\
  super-image-worker read super.img -p system_a | file -   # Stream one partition to a pipe\n\
  super-image-worker make -o super.img --device super:4G --group g:4G \\\n\
      --partition system_a:readonly:g:system.img # Build an image (lpmake analog)\n\
  super-image-worker add super.img -n part_a -p file.img   # Append a partition + payload\n\
  sudo super-image-worker connect super.img -p part_a  # Loop-mount a partition (Linux/Android)\n\n\
SLOT vs SUFFIX (two independent selectors):\n\
  -s/--slot is the primary selector: which LP metadata copy is used.\n\
    Values: 0|a|A|_a|_A (= slot 0), 1|b|B|_b|_B (= slot 1), plain 2..7,\n\
    or all (default = every valid slot, like `lpdump -a`). Letters are\n\
    just conveniences for the index, never read from partition names.\n\
  --suffix (long flag only) is an extra name filter: keeps only\n\
    partitions with that trailing letter (a, b, or all, default all);\n\
    slotless Virtual A/B partitions always match. Plus base-name\n\
    resolution, e.g. `-p system --suffix a` matches `system_a`.\n\
  Example: metadata slot 0 holding both _a and _b entries is read with\n\
    `--slot 0 --suffix b` for the _b rows of slot 0.\n\
  `read` is the exception: -s there is --size, so both --slot and\n\
    --suffix are long-only.\n\
  `make` takes --slot for the slots to generate (same aliases); it has\n\
    no --suffix (names are created).\n\n\
SPLIT / RETROFIT IMAGES:\n\
  Retrofit super spans several files (e.g. system + vendor). Pass secondaries\n\
  with --device on info/extract/read/add/resize/create:\n\
    super-image-worker info sys.img --device vendor=vend.img\n\
  Bare paths auto-match by file name (vendor.img, super_vendor.img).\n\n\
ENVIRONMENT:\n\
  connect/disconnect - Linux and Android (loop devices, requires root)\n\
  map/unmap          - Android/Recovery only (native /dev/block/mapper/*, requires root)\n\
  make/add/resize/remove/rename/create - raw images only for writing (never sparse)\n\
  info/extract/read  - work everywhere, no root for read-only operations\n\n\
EXIT CODES: 0 success, 1 generic failure, 2 bad --get key/field (info).\n\
Every subcommand ships its own detailed manual with formats, selectors,\n\
examples and edge cases: `super-image-worker <command> --help`\n\
(e.g. `super-image-worker info --help`, `super-image-worker make --help`)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(clap::Subcommand)]
pub enum Commands {
    /// Display partition table, geometry, groups, extents and metadata
    ///
    /// Read-only inspector with human/JSON/TSV/env formats, --slot filtering,
    /// --get single-value extraction for scripts, and split-image --device bindings.
    Info(commands::info::InfoArgs),

    /// Extract partitions from super image to individual .img files
    ///
    /// Analog of lpunpack. Parallel (rayon) streaming extractor, raw + sparse
    /// + split inputs, per-partition isolated handles, --slot/--device support.
    Extract(commands::extract::ExtractArgs),

    /// Add a new partition with payload data
    ///
    /// Streams payload (O(1) RAM) into first-fit free space on bound devices,
    /// enforces group maximum_size unless --force. Raw images only.
    /// Auto-selects group by slot suffix (_a/_b) unless --group is given.
    Add(commands::add::AddArgs),

    /// Resize an existing partition
    ///
    /// Grows in place when the tail is free, otherwise allocates a new extent
    /// (collision-safe); shrinking truncates/drops trailing extents.
    /// Supports K/M/G/s suffixes. Checks group limits unless --force is used.
    Resize(commands::resize::ResizeArgs),

    /// Remove a partition or group
    ///
    /// Removes partition entry and its extents, fixing up indices.
    /// Use --group for groups (must be empty unless --force cascades).
    Remove(commands::remove::RemoveArgs),

    /// Rename a partition or group
    ///
    /// Changes the name field in LP metadata. Max 36 characters.
    /// Use --group to rename a group instead of partition.
    Rename(commands::rename::RenameArgs),

    /// Create a new empty partition (without payload)
    ///
    /// Creates partition entry with optional initial size. Use --group-size to auto-create group.
    Create(commands::create::CreateArgs),

    /// Delete OTA snapshot COW partitions (lptools --clear-cow analog)
    ///
    /// Removes *-cow partitions of the `cow` group with a mapped-device
    /// safety gate. Raw images and block devices, no root needed.
    Cow(commands::cow::CowArgs),

    /// Print OTA snapshot update state (snapshotctl dump analog)
    ///
    /// Prints `Update state: <state>` from the on-disk snapshot state
    /// file, for installers to branch on. Read-only, no root needed.
    SnapshotStatus(commands::snapshot_status::SnapshotStatusArgs),

    /// Read partition data to stdout (streaming)
    ///
    /// Outputs raw partition bytes to stdout for piping to other tools.
    /// Supports --size and --skip (K/M/G) for partial reads, --slot base-name
    /// resolution and split --device inputs. Handles BrokenPipe gracefully.
    Read(commands::read::ReadArgs),

    /// Connect partition as a loop block device (Linux + Android)
    ///
    /// Creates /dev/loopN device pointing to partition's physical offset in super image.
    /// Works on both Linux and Android. Requires root.
    /// Use 'map' instead on Android for device-mapper based mapping.
    Connect(commands::connect::ConnectArgs),

    /// Disconnect a loop block device (Linux + Android)
    ///
    /// Removes loop device created by 'connect'. Finds device by image path and partition.
    /// Requires root.
    Disconnect(commands::connect::DisconnectArgs),

    /// Map partition via device-mapper (Android/Recovery only)
    ///
    /// Creates /dev/block/mapper/<name> device using DM ioctl.
    /// Android/Recovery only. Requires root.
    /// On Linux host use 'connect' for loop-device mapping.
    Map(commands::map::MapArgs),

    /// Unmap device-mapper partition (Android/Recovery only)
    ///
    /// Removes device-mapper device created by 'map'.
    /// Android/Recovery only. Requires root.
    Unmap(commands::map::UnmapArgs),

    /// Create a super image from scratch (lpmake analog)
    ///
    /// Builds fresh LP metadata (geometry + slots) with streaming payload
    /// writes. Supports arbitrary slots, single/split outputs, raw/sparse.
    Make(commands::make::MakeArgs),
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Commands::Info(args) => commands::info::run(args),
        Commands::Extract(args) => commands::extract::run(args),
        Commands::Add(args) => commands::add::run(args),
        Commands::Resize(args) => commands::resize::run(args),
        Commands::Remove(args) => commands::remove::run(args),
        Commands::Rename(args) => commands::rename::run(args),
        Commands::Create(args) => commands::create::run(args),
        Commands::Cow(args) => commands::cow::run(args),
        Commands::SnapshotStatus(args) => commands::snapshot_status::run(args),
        Commands::Read(args) => commands::read::run(args),
        Commands::Connect(args) => commands::connect::run_connect(args),
        Commands::Disconnect(args) => commands::connect::run_disconnect(args),
        Commands::Map(args) => commands::map::run_map(args),
        Commands::Unmap(args) => commands::map::run_unmap(args),
        Commands::Make(args) => commands::make::run(args),
    }
}
