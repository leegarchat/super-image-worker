use crate::output::{table, table::TsvOptions};
use clap;
use super_image_worker_core::load_super;
use std::path::PathBuf;

#[derive(clap::Args)]
#[command(
    about = "Display partition table, geometry, groups, extents and metadata",
    long_about = "Display complete information about an Android super partition.\n\n\
Reads LP metadata (Logical Partition), validates its SHA-256 checksums\n\
(geometry, header, tables, with automatic fallback to backup copies), then\n\
displays geometry, header, partitions, extents, groups and block devices.\n\
Accepts raw and Android-sparse images; fully read-only, no root needed.\n\n\
OUTPUT FORMATS (--format):\n\
  human  - Aligned human-readable tables with human sizes (default)\n\
  json   - Pretty JSON: image/groups/devices/partitions/extents (jq-friendly)\n\
  tsv    - Tab-separated rows for awk/cut; -H drops the header, -c picks\n\
           columns (name, base_name, suffix, group, group_index,\n\
           group_max_size, size, size_bytes, sectors, extents, attrs,\n\
           attributes, first_extent, first_phys_offset,\n\
           first_phys_offset_hex, device, readonly)\n\
  env    - `SUPER_*` shell variables for `eval $(super-image-worker info --format env)`\n\n\
SECTION FLAGS: -i/-p/-e/-g/-d/-m pick info/partitions/extents/groups/devices/\n\
mapping; -a/--all or no flags shows everything. -b prints raw bytes in TSV.\n\n\
SLOT FILTER (--slot a|b|all): keeps only partitions of that slot suffix;\n\
slotless (Virtual A/B) partitions always match.\n\n\
PRECISION EXTRACTION (--get <key>, no trailing newline, exit 2 on bad key):\n\
  partitions.<name>.{size,size_human,sectors,group,first_phys_offset_hex,...}\n\
  partitions.<name>.extent.<idx>.{type,sectors,phys_offset_hex,device,...}\n\
  group.<name>.{max_size,...}, device.<name>.{size,...},\n\
  groups, devices, available_suffixes, metadata_slot, ...\n\
  Use --list-keys for the full catalogue.\n\n\
SPLIT / RETROFIT: pass secondaries with repeatable --device:\n\
  --device vendor=path (explicit) or --device path (auto-match by file\n\
  name: vendor.img, super_vendor.img, *_vendor.img). A binding report is\n\
  printed above the selected format.\n\n\
EXAMPLES:\n\
  super-image-worker info super.img                         # Everything, human tables\n\
  super-image-worker info super.img -s a -f tsv -H -c name,size_bytes | awk '{print $1}'\n\
  super-image-worker info super.img -f json | jq '.partitions[] | .name'\n\
  eval $(super-image-worker info super.img -f env); echo $SUPER_PART_SYSTEM_A_SIZE\n\
  super-image-worker info super.img --get partitions.system_a.size\n\
  super-image-worker info super.img --get partitions.system_a.extent.0.phys_offset_hex\n\
  super-image-worker info sys.img --device vendor=vend.img  # Retrofit pair"
)]
pub struct InfoArgs {
    /// Path to super image (raw or Android-sparse format)
    pub image: PathBuf,

    /// Output format: human, json, tsv, env
    #[arg(short, long, default_value = "human")]
    pub format: String,

    /// Filter by slot suffix: a, b, or all
    #[arg(short, long, default_value = "all")]
    pub slot: String,

    /// Suppress column headers in TSV mode
    #[arg(short = 'H', long)]
    pub no_header: bool,

    /// Comma-separated column list for TSV mode
    /// Available: name, base_name, suffix, group, group_index, group_max_size,
    /// size, size_bytes, sectors, extents, attrs, attributes, first_extent,
    /// first_phys_offset, first_phys_offset_hex, device, readonly
    #[arg(short, long)]
    pub columns: Option<String>,

    /// Show sizes in raw bytes instead of human-readable format
    #[arg(short, long)]
    pub bytes: bool,

    /// Show geometry and header info (metadata version, slot count, block size)
    #[arg(short, long)]
    pub info: bool,

    /// Show partition list with names, groups, sizes and attributes
    #[arg(short, long)]
    pub partitions: bool,

    /// Show extent table (physical sectors, offsets, device mapping)
    #[arg(short, long)]
    pub extents: bool,

    /// Show partition groups with max sizes
    #[arg(short, long)]
    pub groups: bool,

    /// Show block devices (super device geometry)
    #[arg(short, long)]
    pub devices: bool,

    /// Show partition -> extent mapping (which extents belong to which partition)
    #[arg(short, long)]
    pub mapping: bool,

    /// Show everything (default if no selection flags specified)
    #[arg(short, long)]
    pub all: bool,

    /// Get a single value by key (use --list-keys for available keys)
    /// Supports nested access: partitions.<name>.<field>, partitions.<name>.extent.<idx>.<field>
    #[arg(long)]
    pub get: Option<String>,

    /// Bind secondary block devices for split/retrofit images.
    /// Repeatable. Forms: `--device vendor=path` (explicit) or `--device path`
    /// (auto-match by file name, e.g. vendor.img / super_vendor.img).
    #[arg(long = "device")]
    pub device: Vec<String>,

    /// List all available --get keys with descriptions
    #[arg(long)]
    pub list_keys: bool,
}

pub fn run(args: InfoArgs) -> std::process::ExitCode {
    if args.list_keys {
        print_keys();
        return std::process::ExitCode::SUCCESS;
    }

    let data = match load_super(&args.image) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let slot = match args.slot.as_str() {
        "a" | "A" => Some("a"),
        "b" | "B" => Some("b"),
        "all" => None,
        other => {
            eprintln!("unknown slot: {other} (use a, b, all)");
            return std::process::ExitCode::FAILURE;
        }
    };

    if let Some(ref key) = args.get {
        table::print_get(&data, key);
        return std::process::ExitCode::SUCCESS;
    }

    let show_all = args.all
        || (!args.info
            && !args.partitions
            && !args.extents
            && !args.groups
            && !args.devices
            && !args.mapping);

    // Split/retrofit binding report (validates --device specs).
    let bindings = super::split_util::binding_report(&data, &args.device, &args.image);
    if !bindings.is_empty() {
        println!("=== Device Bindings (split/retrofit) ===");
        for line in &bindings {
            println!("{line}");
        }
        println!();
    }

    match args.format.as_str() {
        "human" => table::print_human(
            &data,
            slot,
            show_all || args.info,
            show_all || args.groups,
            show_all || args.devices,
            show_all || args.partitions,
            show_all || args.extents,
            show_all || args.mapping,
        ),
        "json" => table::print_json(&data, slot),
        "tsv" => {
            let columns = args
                .columns
                .map(|c| c.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_else(|| {
                    vec![
                        "name".into(),
                        "group".into(),
                        "size".into(),
                        "extents".into(),
                        "attrs".into(),
                    ]
                });
            let opts = TsvOptions {
                header: !args.no_header,
                columns,
                bytes: args.bytes,
            };
            table::print_tsv(&data, slot, &opts);
        }
        "env" => table::print_env(&data, slot),
        other => {
            eprintln!("unknown format: {other}");
            return std::process::ExitCode::FAILURE;
        }
    }

    std::process::ExitCode::SUCCESS
}

fn print_keys() {
    println!("Available --get keys:");
    println!("  Scalars:");
    println!("    format, size, size_human, geometry_offset, metadata_max_size");
    println!("    metadata_slot_count, logical_block_size, metadata_version");
    println!("    header_size, tables_size, available_suffixes, partition_count");
    println!("  Partition fields:");
    println!("    partitions.<name>.name");
    println!("    partitions.<name>.base_name");
    println!("    partitions.<name>.suffix");
    println!("    partitions.<name>.group");
    println!("    partitions.<name>.group_index");
    println!("    partitions.<name>.group_max_size");
    println!("    partitions.<name>.group_max_size_human");
    println!("    partitions.<name>.size");
    println!("    partitions.<name>.size_human");
    println!("    partitions.<name>.sectors");
    println!("    partitions.<name>.extents");
    println!("    partitions.<name>.attrs");
    println!("    partitions.<name>.attributes");
    println!("    partitions.<name>.first_extent");
    println!("    partitions.<name>.first_phys_offset");
    println!("    partitions.<name>.first_phys_offset_hex");
    println!("    partitions.<name>.device");
    println!("    partitions.<name>.readonly");
    println!("    partitions.<name>.slot_suffixed");
    println!("  Extent fields (per partition):");
    println!("    partitions.<name>.extent.<idx>.type");
    println!("    partitions.<name>.extent.<idx>.sectors");
    println!("    partitions.<name>.extent.<idx>.size");
    println!("    partitions.<name>.extent.<idx>.size_human");
    println!("    partitions.<name>.extent.<idx>.phys_offset");
    println!("    partitions.<name>.extent.<idx>.phys_offset_hex");
    println!("    partitions.<name>.extent.<idx>.target_data");
    println!("    partitions.<name>.extent.<idx>.target_source");
    println!("    partitions.<name>.extent.<idx>.device");
    println!("  Group fields:");
    println!("    group.<name>.name, flags, max_size, max_size_human");
    println!("  Device fields:");
    println!("    device.<name>.name, first_sector, alignment, size, size_human");
    println!("  Lists:");
    println!("    groups, devices");
}
