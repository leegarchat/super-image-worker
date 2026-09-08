use clap::Args;
use super_image_worker_core::{
    GEOMETRY_BACKUP_OFFSET, GEOMETRY_PRIMARY_OFFSET, LP_METADATA_HEADER_SIZE,
    LP_METADATA_MAJOR_VERSION, LP_METADATA_MINOR_VERSION, LP_SECTOR_SIZE, LP_TARGET_TYPE_LINEAR,
    LpWriter, backup_offset, primary_offset,
};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

#[derive(Args)]
#[command(
    about = "Create a super image from scratch (lpmake analog)",
    long_about = "Build a new super image with fresh LP metadata (geometry + slots).\n\n\
Independent high-performance analog of AOSP lpmake with extras:\n\
  - Arbitrary target slot (--slot 0|1|a|b|all); stock lpmake only writes slot 0\n\
  - Single super.img or split/retrofit outputs (--retrofit)\n\
  - Raw or Android Sparse container output (--sparse)\n\
  - Payloads spanning several block devices (multi-extent chain) when one\n\
    device alone is too small - stock lpmake fails there\n\
  - O(1) RAM: payloads are streamed in 1 MiB chunks, never fully loaded\n\n\
PIPELINE: validate specs -> reserve aligned metadata area -> first-fit\n\
allocate extents (per-device alignment AND alignment_offset honoured) ->\n\
sized output files (sparse on FS, O(1)) -> stream payloads -> render geometry\n\
+ slot blobs with SHA-256 -> optional sparse conversion -> self-verify reload.\n\n\
LAYOUT NOTES:\n\
  - Metadata (geometry + all slots) lives in the first block device file.\n\
    Secondary retrofit files carry payload data only.\n\
  - The same partition table is replicated to every selected slot\n\
    (primary + backup copies per AOSP liblp).\n\
  - A payload that fits on one device gets ONE extent; otherwise it is carved\n\
    into a per-device extent chain in table order (visible in `info -m`).\n\
  - Group maximum_size is enforced per group unless --force.\n\n\
SPEC FORMATS (repeatable flags):\n\
  --device name:size[:alignment[:alignment_offset]]  (e.g. super:4G,\n\
           system:2G:1048576, vendor:1G:1048576:0; alignment_offset shifts\n\
           the allocation phase for exotic erase-block geometries)\n\
  --group name:max_size                              (e.g. default:4G)\n\
  --partition name:attrs:group[:payload_path]        (attrs: comma list of\n\
           readonly, slot_suffixed, updated, disabled, none)\n\
  --partition name:attrs:group@device:payload_path  pins the payload to one\n\
           split device (single extent, fails if it does not fit there)\n\
  Sizes accept K/M/G suffixes (e.g. 300M, 1G, 64K). Names are capped at 36\n\
  bytes. A partition without payload becomes an extent-less placeholder.\n\n\
OUTPUTS: single mode writes exactly -o PATH (one device required); PATH may\n\
be a regular file or a raw block device (e.g. /dev/block/by-name/super),\n\
in which case the image is built straight into the block node (the\n\
--device size must equal the probed block size, and --sparse is refused\n\
for block outputs). Retrofit mode (--retrofit, 2+ devices) writes\n\
<PREFIX>_<device>.img per device, unless --output-map <name>=<path>\n\
overrides individual outputs (files or block devices, so a split image\n\
can target several block nodes at once: super, cust, modem_a, ...).\n\
--sparse converts each raw file to an Android Sparse container with RAW +\n\
DONT_CARE chunks (staging *.raw-tmp files are removed afterwards).\n\n\
EXAMPLES:\n\
  super-image-worker make -o super.img --device super:4G --group default:4G \\\n\
      --partition system_a:readonly:default:system.img\n\
  super-image-worker make -o out/super --retrofit --device system:2G --device vendor:1G \\\n\
      --group google_dynamic_partitions_a:3G --partition system_a:readonly:google_dynamic_partitions_a:sys.img\n\
  super-image-worker make -o super.img --sparse --slot all --device super:4G \\\n\
      --group g:4G --partition system_a:readonly:g:a.img --partition system_b:readonly:g:b.img\n\
  super-image-worker make -o super.img --device super:4G --group g:4G \\\n\
      --partition sys:readonly:g:sys.img --dry-run   # Validate + print plan"
)]
pub struct MakeArgs {
    /// Output path. Single mode: exact file. Retrofit: prefix, one
    /// `<prefix>_<device>.img` per block device.
    #[arg(short, long)]
    pub output: PathBuf,

    /// Block device spec name:size[:alignment[:alignment_offset]] (repeatable).
    /// Single super: one device. Retrofit: several.
    #[arg(long = "device")]
    pub device: Vec<String>,

    /// Group spec name:max_size (repeatable).
    #[arg(long = "group")]
    pub group: Vec<String>,

    /// Partition spec name:attrs:group[:payload_path] (repeatable).
    /// attrs: comma list of readonly, slot_suffixed, updated, disabled, none.
    #[arg(long = "partition")]
    pub partition: Vec<String>,

    /// Target metadata slot(s): 0, 1, a, b, or all (default: 0).
    #[arg(long, default_value = "0")]
    pub slot: String,

    /// LP metadata slot area size in bytes (default 65536).
    #[arg(long, default_value = "65536")]
    pub metadata_size: String,

    /// Number of metadata slots, 1..=8 (usually 2 or 3; default 3).
    #[arg(long, default_value = "3")]
    pub metadata_slots: u32,

    /// Logical block size: 512 or 4096 (default 4096).
    #[arg(long, default_value = "4096")]
    pub block_size: u32,

    /// Default device alignment in bytes when a --device omits it.
    #[arg(long, default_value = "1048576")]
    pub alignment: String,

    /// Split/retrofit mode: emit one file per block device.
    #[arg(long)]
    pub retrofit: bool,

    /// Override the output path of one block device (repeatable).
    /// Form: `--output-map <device-name>=<path>`; path may be a regular
    /// file or a raw block device. Unmapped devices fall back to
    /// `<prefix>_<device>.img` (retrofit) or `-o` (single mode).
    #[arg(long = "output-map")]
    pub output_map: Vec<String>,

    /// Emit Android Sparse container(s) instead of raw.
    #[arg(long)]
    pub sparse: bool,

    /// Allow partitions to exceed their group's maximum_size.
    #[arg(long)]
    pub force: bool,

    /// Dry run - validate and print the plan without writing files.
    #[arg(long)]
    pub dry_run: bool,
}

struct DeviceSpec {
    name: String,
    size: u64,
    alignment: u32,
    alignment_offset: u32,
}

struct PartitionSpec {
    name: String,
    attrs: u32,
    group: String,
    /// Optional split-device pin from `group@device` syntax.
    pin: Option<String>,
    payload: Option<PathBuf>,
    payload_len: u64,
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
    } else {
        (s, 1)
    };
    let num: u64 = num_part.parse().map_err(|_| format!("invalid size: {s}"))?;
    num.checked_mul(multiplier)
        .ok_or_else(|| format!("size overflow: {s}"))
}

fn parse_attrs(s: &str) -> Result<u32, String> {
    let mut attrs = 0u32;
    for a in s.split(',') {
        match a.trim() {
            "readonly" => attrs |= super_image_worker_core::LP_PARTITION_ATTR_READONLY,
            "slot_suffixed" => attrs |= super_image_worker_core::LP_PARTITION_ATTR_SLOT_SUFFIXED,
            "updated" => attrs |= super_image_worker_core::LP_PARTITION_ATTR_UPDATED,
            "disabled" => attrs |= super_image_worker_core::LP_PARTITION_ATTR_DISABLED,
            "none" | "" => {}
            other => return Err(format!("unknown attribute '{other}'")),
        }
    }
    Ok(attrs)
}

fn parse_device_spec(s: &str, default_alignment: u32) -> Result<DeviceSpec, String> {
    let mut parts = s.splitn(4, ':');
    let name = parts.next().unwrap_or("").trim().to_string();
    let size_s = parts.next().unwrap_or("").trim();
    if name.is_empty() || size_s.is_empty() {
        return Err(format!(
            "invalid --device '{s}' (want name:size[:alignment[:alignment_offset]])"
        ));
    }
    if name.len() > 36 {
        return Err(format!("device name too long: '{name}'"));
    }
    let size = parse_size(size_s)?;
    if size == 0 {
        return Err(format!("device '{name}' has zero size"));
    }
    let alignment: u32 = match parts.next() {
        Some(a) if !a.trim().is_empty() => parse_size(a.trim())?
            .try_into()
            .map_err(|_| format!("alignment too large in '{s}'"))?,
        _ => default_alignment,
    };
    let alignment_offset: u32 = match parts.next() {
        Some(a) if !a.trim().is_empty() => a
            .trim()
            .parse()
            .map_err(|_| format!("invalid alignment_offset in '{s}'"))?,
        _ => 0,
    };
    Ok(DeviceSpec {
        name,
        size,
        alignment,
        alignment_offset,
    })
}

fn parse_group_spec(s: &str) -> Result<(String, u64), String> {
    let (name, size_s) = s
        .split_once(':')
        .ok_or_else(|| format!("invalid --group '{s}' (want name:max_size)"))?;
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(format!("invalid --group '{s}'"));
    }
    if name.len() > 36 {
        return Err(format!("group name too long: '{name}'"));
    }
    Ok((name, parse_size(size_s.trim())?))
}

fn parse_partition_spec(s: &str) -> Result<PartitionSpec, String> {
    let mut parts = s.splitn(4, ':');
    let name = parts.next().unwrap_or("").trim().to_string();
    let attrs_s = parts
        .next()
        .ok_or_else(|| format!("invalid --partition '{s}' (want name:attrs:group[:payload])"))?;
    let group = parts
        .next()
        .ok_or_else(|| format!("invalid --partition '{s}' (want name:attrs:group[:payload])"))?
        .trim()
        .to_string();
    if name.is_empty() || group.is_empty() {
        return Err(format!("invalid --partition '{s}'"));
    }
    if name.len() > 36 {
        return Err(format!("partition name too long: '{name}'"));
    }
    let attrs = parse_attrs(attrs_s.trim())?;
    // Optional `@device` pin on the group field (split layouts).
    let (group, pin) = match group.split_once('@') {
        Some((g, d)) => {
            let g = g.trim().to_string();
            let d = d.trim().to_string();
            if g.is_empty() || d.is_empty() {
                return Err(format!("invalid --partition '{s}' (want group[@device])"));
            }
            if d.len() > 36 {
                return Err(format!("device name too long in '{s}'"));
            }
            (g, Some(d))
        }
        None => (group, None),
    };
    let (payload, payload_len) = match parts.next() {
        Some(p) if !p.trim().is_empty() => {
            let pb = PathBuf::from(p.trim());
            let len = pb
                .metadata()
                .map_err(|e| format!("payload '{}': {e}", pb.display()))?
                .len();
            (Some(pb), len)
        }
        _ => (None, 0),
    };
    Ok(PartitionSpec {
        name,
        attrs,
        group,
        pin,
        payload,
        payload_len,
    })
}

/// True when `path` resolves to a raw block device (symlinks followed,
/// so /dev/block/by-name/* entries count).
fn is_block_device_path(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.file_type().is_block_device())
        .unwrap_or(false)
}

/// Probe the byte size of a block device via SEEK_END (metadata reports
/// 0 for block nodes). Restores the cursor to 0.
fn probe_block_size(path: &Path) -> Result<u64, String> {
    let mut f =
        File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let end = f
        .seek(SeekFrom::End(0))
        .map_err(|e| format!("seek end {}: {e}", path.display()))?;
    f.seek(SeekFrom::Start(0))
        .map_err(|e| format!("seek start {}: {e}", path.display()))?;
    if end == 0 {
        return Err("probed size is 0".into());
    }
    Ok(end)
}

/// Zero-fill `size` bytes of an open block node in 4 MiB chunks, so a
/// direct-to-block build starts from the same blank slate as a fresh
/// output file (no stale payload/metadata in gaps or free space).
fn zero_fill(handle: &mut File, size: u64) -> Result<(), String> {
    use std::io::Seek as _;
    handle
        .seek(SeekFrom::Start(0))
        .map_err(|e| format!("seek start for zero-fill: {e}"))?;
    let zeros = vec![0u8; 4 * 1024 * 1024];
    let mut remaining = size;
    while remaining > 0 {
        let n = remaining.min(zeros.len() as u64) as usize;
        handle
            .write_all(&zeros[..n])
            .map_err(|e| format!("zero-fill write: {e}"))?;
        remaining -= n as u64;
    }
    handle.flush().map_err(|e| format!("zero-fill flush: {e}"))?;
    Ok(())
}

fn parse_slots(s: &str, slot_count: u32) -> Result<Vec<u64>, String> {
    let all: Vec<u64> = (0..slot_count as u64).collect();
    match s.trim() {
        "0" | "a" | "A" => Ok(vec![0]),
        "1" | "b" | "B" => {
            if slot_count < 2 {
                return Err("slot 1 requested but metadata has only 1 slot".into());
            }
            Ok(vec![1])
        }
        "all" => Ok(all),
        other => Err(format!("invalid --slot '{other}' (want 0|1|a|b|all)")),
    }
}

pub fn run(args: MakeArgs) -> std::process::ExitCode {
    if let Err(e) = run_inner(args) {
        eprintln!("error: {e}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

fn run_inner(args: MakeArgs) -> std::result::Result<(), String> {
    if args.device.is_empty() {
        return Err("at least one --device is required".into());
    }
    if !(1..=8).contains(&args.metadata_slots) {
        return Err("--metadata-slots must be 1..=8".into());
    }
    if args.block_size != 512 && args.block_size != 4096 {
        return Err("--block-size must be 512 or 4096".into());
    }
    let metadata_max_size: u32 = parse_size(&args.metadata_size)?
        .try_into()
        .map_err(|_| "metadata size too large".to_string())?;
    if metadata_max_size == 0 || metadata_max_size > 16 * 1024 * 1024 {
        return Err("invalid --metadata-size".into());
    }
    let default_alignment: u32 = parse_size(&args.alignment)?
        .try_into()
        .map_err(|_| "alignment too large".to_string())?;

    let target_slots = parse_slots(&args.slot, args.metadata_slots)?;

    // Parse devices / groups / partitions.
    let mut dev_specs = Vec::new();
    for d in &args.device {
        dev_specs.push(parse_device_spec(d, default_alignment)?);
    }
    // Duplicate device names are ambiguous for retrofit file mapping.
    {
        let mut seen = std::collections::HashSet::new();
        for d in &dev_specs {
            if !seen.insert(d.name.clone()) {
                return Err(format!("duplicate device name '{}'", d.name));
            }
        }
    }
    let mut groups: Vec<(String, u64)> = Vec::new();
    for g in &args.group {
        groups.push(parse_group_spec(g)?);
    }
    let mut part_specs = Vec::new();
    for p in &args.partition {
        part_specs.push(parse_partition_spec(p)?);
    }
    {
        let mut seen = std::collections::HashSet::new();
        for p in &part_specs {
            if !seen.insert(p.name.clone()) {
                return Err(format!("duplicate partition name '{}'", p.name));
            }
        }
    }

    let split_out = args.retrofit || dev_specs.len() > 1;
    if dev_specs.len() > 1 && !args.retrofit {
        return Err(
            "multiple --device entries require --retrofit (output becomes a per-device prefix)"
                .into(),
        );
    }

    // Metadata footprint reservation: geometry + all primaries + all backups,
    // aligned up per device (each device's own alignment AND alignment_offset
    // are honoured, so first_logical_sector already sits on the right phase).
    let max64 = metadata_max_size as u64;
    let count64 = args.metadata_slots as u64;
    let meta_end = 0x3000u64.saturating_add(2u64.saturating_mul(count64).saturating_mul(max64));
    let align64 = (default_alignment as u64).max(LP_SECTOR_SIZE);
    let reserve_bytes = meta_end.div_ceil(align64).saturating_mul(align64);
    let reserve_sectors = reserve_bytes / LP_SECTOR_SIZE;

    for d in &dev_specs {
        if d.size < reserve_bytes.saturating_add(LP_SECTOR_SIZE) {
            return Err(format!(
                "device '{}' too small ({} < reserved {} + 1 sector)",
                d.name, d.size, reserve_bytes
            ));
        }
        if d.size % LP_SECTOR_SIZE != 0 {
            return Err(format!("device '{}' size is not sector-aligned", d.name));
        }
    }

    // Build LP tables with incremental first-fit allocation (O(1) RAM).
    let writer = LpWriter::new();
    let mut devices: Vec<super_image_worker_core::BlockDevice> = Vec::new();
    for d in &dev_specs {
        let first_logical_sector =
            LpWriter::align_up(reserve_sectors, d.alignment, d.alignment_offset)
                .ok_or_else(|| format!("device '{}': alignment overflow", d.name))?;
        devices.push(super_image_worker_core::BlockDevice {
            first_logical_sector,
            alignment: d.alignment,
            alignment_offset: d.alignment_offset,
            size: d.size,
            partition_name: d.name.clone(),
        });
    }
    let groups_lp: Vec<super_image_worker_core::Group> = groups
        .iter()
        .map(|(n, m)| super_image_worker_core::Group {
            name: n.clone(),
            flags: 0,
            maximum_size: *m,
        })
        .collect();
    let group_index_of = |name: &str| -> Option<u32> {
        groups_lp
            .iter()
            .position(|g| g.name == name)
            .map(|i| i as u32)
    };

    let mut partitions: Vec<super_image_worker_core::Partition> = Vec::new();
    let mut extents: Vec<super_image_worker_core::Extent> = Vec::new();
    // One segment per extent: (partition_index, payload_path, input_offset,
    // segment_len, target_source, phys_sector). Multi-extent partitions
    // (payload spanning several block devices) produce several segments.
    let mut payload_jobs: Vec<(usize, PathBuf, u64, u64, u32, u64)> = Vec::new();
    let mut group_used: HashMap<String, u64> = HashMap::new();

    for spec in &part_specs {
        let gi = group_index_of(&spec.group).ok_or_else(|| {
            format!(
                "partition '{}': group '{}' not defined (use --group)",
                spec.name, spec.group
            )
        })?;
        let jobs_base = payload_jobs.len();
        let (first_extent_index, num_extents) = if spec.payload.is_some() && spec.payload_len > 0 {
            let needed = spec.payload_len.div_ceil(LP_SECTOR_SIZE);
            let used = group_used.get(&spec.group).copied().unwrap_or(0);
            let max = groups_lp
                .get(gi as usize)
                .map(|g| g.maximum_size)
                .unwrap_or(0);
            if max > 0 && used.saturating_add(needed) > max / LP_SECTOR_SIZE && !args.force {
                return Err(format!(
                    "partition '{}' would exceed group '{}' max_size ({} + {} > {} sectors). Use --force",
                    spec.name,
                    spec.group,
                    used,
                    needed,
                    max / LP_SECTOR_SIZE
                ));
            }
            // Pinned partitions go to one explicit device as a single
            // extent (fails if they do not fit there); unpinned ones
            // use the spanning plan (single extent on one device when
            // it fits, otherwise a per-device extent chain).
            let plan = match spec.pin.as_deref() {
                Some(pin_name) => {
                    let pinned = devices
                        .iter()
                        .position(|d| d.partition_name == pin_name)
                        .ok_or_else(|| {
                            let known: Vec<&str> = devices
                                .iter()
                                .map(|d| d.partition_name.as_str())
                                .collect();
                            format!(
                                "partition '{}': pinned device '{pin_name}' is not a --device (known: {known:?})",
                                spec.name
                            )
                        })? as u32;
                    let sector = writer
                        .find_free_sectors(&extents, &devices, pinned, needed)
                        .map_err(|e| {
                            format!(
                                "partition '{}': pinned to '{pin_name}': {e}",
                                spec.name
                            )
                        })?;
                    println!(
                        "  partition '{}': pinned to device '{pin_name}' ({needed} sectors at {sector})",
                        spec.name,
                    );
                    vec![(pinned, sector, needed)]
                }
                None => writer
                    .plan_spanning_allocation(&extents, &devices, needed)
                    .map_err(|e| format!("partition '{}': {e}", spec.name))?,
            };
            if plan.len() > 1 {
                println!(
                    "  partition '{}': payload spans {} devices ({} extents)",
                    spec.name,
                    plan.iter()
                        .map(|(src, _, _)| src)
                        .collect::<std::collections::HashSet<_>>()
                        .len(),
                    plan.len()
                );
            }
            let idx = extents.len() as u32;
            let mut input_offset: u64 = 0;
            for (src, sector, take) in &plan {
                extents.push(super_image_worker_core::Extent {
                    num_sectors: *take,
                    target_type: LP_TARGET_TYPE_LINEAR,
                    target_data: *sector,
                    target_source: *src,
                });
                if let Some(payload) = spec.payload.clone() {
                    // Last segment may be shorter (unpadded payload tail).
                    let seg_len = (*take)
                        .saturating_mul(LP_SECTOR_SIZE)
                        .min(spec.payload_len.saturating_sub(input_offset));
                    payload_jobs.push((0, payload, input_offset, seg_len, *src, *sector));
                    input_offset = input_offset.saturating_add(seg_len);
                }
            }
            group_used.insert(spec.group.clone(), used.saturating_add(needed));
            (idx, plan.len() as u32)
        } else {
            if spec.payload.is_some() && spec.payload_len == 0 {
                eprintln!(
                    "warning: payload for '{}' is empty, creating extent-less entry",
                    spec.name
                );
            }
            (extents.len() as u32, 0)
        };
        let pi = partitions.len();
        partitions.push(super_image_worker_core::Partition {
            name: spec.name.clone(),
            attributes: spec.attrs,
            first_extent_index,
            num_extents,
            group_index: gi,
        });
        // Back-patch the real partition index into this partition's segments.
        if num_extents > 0 {
            for job in payload_jobs.iter_mut().skip(jobs_base) {
                job.0 = pi;
            }
        }
    }

    // Explicit per-device output overrides: --output-map name=path.
    let mut output_overrides: HashMap<String, PathBuf> = HashMap::new();
    for spec in &args.output_map {
        let (name, path_str) = spec
            .split_once('=')
            .ok_or_else(|| format!("invalid --output-map '{spec}' (want name=path)"))?;
        let name = name.trim().to_string();
        let path_str = path_str.trim();
        if name.is_empty() || path_str.is_empty() {
            return Err(format!("invalid --output-map '{spec}' (want name=path)"));
        }
        if !dev_specs.iter().any(|d| d.name == name) {
            let known: Vec<&str> = dev_specs.iter().map(|d| d.name.as_str()).collect();
            return Err(format!(
                "--output-map device '{name}' is not a --device (known: {known:?})"
            ));
        }
        if output_overrides.insert(name.clone(), PathBuf::from(path_str)).is_some() {
            return Err(format!("--output-map device '{name}' given twice"));
        }
    }

    // Output file plan (one path per block device, table order).
    let out_files: Vec<PathBuf> = if split_out {
        dev_specs
            .iter()
            .map(|d| {
                if let Some(p) = output_overrides.get(&d.name) {
                    return p.clone();
                }
                let prefix = args.output.to_string_lossy().into_owned();
                PathBuf::from(format!("{prefix}_{}.img", d.name))
            })
            .collect()
    } else {
        vec![output_overrides
            .values()
            .next()
            .cloned()
            .unwrap_or_else(|| args.output.clone())]
    };
    if !split_out && output_overrides.len() > 1 {
        return Err("--output-map with several devices requires --retrofit".into());
    }
    // Raw staging paths (final when !sparse).
    let raw_files: Vec<PathBuf> = if args.sparse {
        out_files
            .iter()
            .map(|p| {
                let s = p.to_string_lossy().into_owned();
                PathBuf::from(format!("{s}.raw-tmp"))
            })
            .collect()
    } else {
        out_files.clone()
    };

    // Detect block-device outputs and validate them BEFORE the dry-run
    // exit, so bad plans fail fast: sparse is refused for blocks, and
    // every --device size must equal the probed block size exactly.
    let mut is_block_out: Vec<bool> = Vec::with_capacity(out_files.len());
    for path in out_files.iter() {
        is_block_out.push(is_block_device_path(path));
    }
    if args.sparse && is_block_out.iter().any(|b| *b) {
        return Err("sparse output to a block device is not supported (build raw directly)".into());
    }
    for (i, dev) in dev_specs.iter().enumerate() {
        if !is_block_out[i] {
            continue;
        }
        let probed = probe_block_size(&out_files[i])
            .map_err(|e| format!("probe {}: {e}", out_files[i].display()))?;
        if dev.size != probed {
            return Err(format!(
                "device '{}' size mismatch: spec {} != block {} {} (pass the exact block size)",
                dev.name,
                dev.size,
                probed,
                out_files[i].display()
            ));
        }
    }

    // Dry run: print the plan.
    if args.dry_run {
        println!(
            "make plan: {} device(s), {} group(s), {} partition(s), slots {target_slots:?}, {}",
            devices.len(),
            groups_lp.len(),
            partitions.len(),
            if args.sparse { "sparse" } else { "raw" }
        );
        for (d, f) in dev_specs.iter().zip(out_files.iter()) {
            let kind = if is_block_device_path(f) {
                let probed = probe_block_size(f)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|e| format!("ERR:{e}"));
                format!("block (probed {probed})")
            } else {
                "file".to_string()
            };
            println!("  device {} size={} -> {} [{kind}]", d.name, d.size, f.display());
        }
        for (pi, payload, in_off, len, src, sector) in &payload_jobs {
            let pname = partitions.get(*pi).map(|p| p.name.as_str()).unwrap_or("?");
            println!(
                "  partition {pname} payload={} (bytes {in_off}..{} of input) -> device {src} sector {sector}",
                payload.display(),
                in_off.saturating_add(*len),
            );
        }
        return Ok(());
    }

    // Create + size output files (is_block_out validated above).
    // NOTE: --sparse uses .raw-tmp staging files, never block nodes
    // (refused above), so raw_files == out_files whenever any output
    // is a block device.
    if let Some(parent) = args.output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| format!("create output dir: {e}"))?;
    }
    // Per-file parent dirs for --output-map paths living elsewhere.
    for (path, is_block) in raw_files.iter().zip(is_block_out.iter()) {
        if *is_block {
            continue;
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| format!("create output dir: {e}"))?;
        }
    }
    let mut handles: Vec<File> = Vec::new();
    for (i, dev) in dev_specs.iter().enumerate() {
        let path = &raw_files[i];
        let is_block = is_block_out[i];
        if is_block {
            // Direct-to-block build (spec == probed size validated above):
            // open read-write without truncating, then zero-fill to
            // emulate a fresh output file.
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map_err(|e| format!("open {}: {e}", path.display()))?;
            handles.push(f);
            println!("  zeroing block {} ({} bytes)...", path.display(), dev.size);
            zero_fill(handles.last_mut().ok_or("no output files")?, dev.size)?;
        } else {
            let f = super_image_worker_core::sparse::create_sized_file(path, dev.size)
                .map_err(|e| format!("create {}: {e}", path.display()))?;
            handles.push(f);
        }
    }

    // Geometry into device-0 file (primary + backup).
    let geom = LpWriter::render_geometry(metadata_max_size, args.metadata_slots, args.block_size)
        .map_err(|e| format!("{e}"))?;
    let primary_handle = handles
        .get_mut(0)
        .ok_or_else(|| "no output files".to_string())?;
    LpWriter::write_geometry_at(primary_handle, GEOMETRY_PRIMARY_OFFSET, &geom)
        .map_err(|e| format!("{e}"))?;
    LpWriter::write_geometry_at(primary_handle, GEOMETRY_BACKUP_OFFSET, &geom)
        .map_err(|e| format!("{e}"))?;

    // Stream payload segments to their device files (O(1) RAM each).
    // Segments of one partition are contiguous slices of its input file.
    // Output files were zero-sized at creation, so extent tail slack
    // (sector padding) is already zero — no explicit pad needed.
    for (pi, payload_path, in_off, seg_len, src, sector) in &payload_jobs {
        let pname = partitions
            .get(*pi)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let phys = sector.saturating_mul(LP_SECTOR_SIZE);
        let handle = handles
            .get_mut(*src as usize)
            .ok_or_else(|| format!("device index {src} out of range"))?;
        let mut input = File::open(payload_path)
            .map_err(|e| format!("open payload {}: {e}", payload_path.display()))?;
        writer
            .copy_payload_segment(handle, phys, &mut input, *in_off, *seg_len)
            .map_err(|e| format!("write payload for '{pname}': {e}"))?;
        println!(
            "  wrote {pname}[{in_off}..{}]: {seg_len} bytes -> device {src} offset 0x{phys:x}",
            in_off.saturating_add(*seg_len),
        );
    }

    // Render slot blob and write to every target slot (primary + backup).
    let header = super_image_worker_core::MetadataHeader {
        major_version: LP_METADATA_MAJOR_VERSION,
        minor_version: LP_METADATA_MINOR_VERSION,
        header_size: LP_METADATA_HEADER_SIZE,
        tables_size: 0, // recomputed by render_slot
        partitions: super_image_worker_core::TableDescriptor {
            offset: 0,
            num_entries: 0,
            entry_size: 0,
        },
        extents: super_image_worker_core::TableDescriptor {
            offset: 0,
            num_entries: 0,
            entry_size: 0,
        },
        groups: super_image_worker_core::TableDescriptor {
            offset: 0,
            num_entries: 0,
            entry_size: 0,
        },
        block_devices: super_image_worker_core::TableDescriptor {
            offset: 0,
            num_entries: 0,
            entry_size: 0,
        },
    };
    let slot_blob = LpWriter::render_slot(
        &header,
        &partitions,
        &extents,
        &groups_lp,
        &devices,
        metadata_max_size,
    )
    .map_err(|e| format!("render metadata: {e}"))?;
    {
        let primary_handle = handles
            .get_mut(0)
            .ok_or_else(|| "no output files".to_string())?;
        for s in &target_slots {
            let po =
                primary_offset(*s, max64).ok_or_else(|| "primary offset overflow".to_string())?;
            let bo = backup_offset(*s, count64, max64)
                .ok_or_else(|| "backup offset overflow".to_string())?;
            LpWriter::write_slot_at(primary_handle, po, &slot_blob).map_err(|e| format!("{e}"))?;
            LpWriter::write_slot_at(primary_handle, bo, &slot_blob).map_err(|e| format!("{e}"))?;
            println!("  metadata written to slot {s} (primary 0x{po:x}, backup 0x{bo:x})");
        }
        primary_handle.flush().map_err(|e| format!("flush: {e}"))?;
    }
    drop(handles);

    // Optional sparse conversion (raw staging -> final sparse, then cleanup).
    if args.sparse {
        for (raw, sparse) in raw_files.iter().zip(out_files.iter()) {
            super_image_worker_core::sparse::raw_to_sparse(raw, sparse, args.block_size)
                .map_err(|e| format!("sparse {}: {e}", sparse.display()))?;
            std::fs::remove_file(raw).map_err(|e| format!("remove staging raw: {e}"))?;
            println!("  sparse: {}", sparse.display());
        }
    }

    // Self-verify: reload primary and report.
    let verify_path: &Path = if args.sparse {
        &out_files[0]
    } else {
        &raw_files[0]
    };
    let data =
        super_image_worker_core::load_super(verify_path).map_err(|e| format!("verify reload: {e}"))?;
    println!(
        "done: {} partition(s), {} extent(s), {} group(s), {} device(s) -> {}",
        data.partitions.len(),
        data.extents.len(),
        data.groups.len(),
        data.devices.len(),
        out_files
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}
