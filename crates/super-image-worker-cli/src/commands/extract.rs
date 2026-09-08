use super::split_util;
use clap::Args;
use super_image_worker_core::{Image, extract_partition, extract_partition_split};
use rayon::prelude::*;
use std::fs;
use std::io::BufWriter;
use std::path::PathBuf;

#[derive(Args)]
#[command(
    about = "Extract partitions from super image to individual .img files",
    long_about = "Extract one or all partitions from super image to separate .img files.\n\n\
lpunpack analog. Follows each partition's full multi-extent chain (across all\n\
bound block devices) and writes sequentially to <partition_name>.img in the\n\
output directory (created if missing). File size always equals the partition\n\
size (sum of extents); extent-less partitions produce empty files.\n\
Accepts raw and Android-sparse images; read-only, no root needed.\n\n\
PARALLELISM: partitions are extracted concurrently with rayon; every thread\n\
opens an isolated image handle, so there are no Seek races. Errors are\n\
collected per partition: failed outputs are deleted, the rest are kept,\n\
and the exit code is still FAILURE if anything failed.\n\n\
SELECTION: -p takes a full name (system_a) or a base name plus --slot\n\
(system -s a). Without -p, --slot filters the whole table (a|b|all).\n\
--dry-run lists name + size without writing. Existing files are skipped\n\
unless --force overwrites them.\n\n\
SPLIT / RETROFIT: repeatable --device (vendor=path or auto-matched path).\n\
A partition touching an unbound device fails with the exact --device string.\n\n\
EXAMPLES:\n\
  super-image-worker extract super.img -o ./output              # Unpack everything\n\
  super-image-worker extract super.img -o ./out -p system_a     # One partition\n\
  super-image-worker extract super.img -o ./out -s a            # Slot A only\n\
  super-image-worker extract super.img -o ./out -p system -s b  # Base name + slot\n\
  super-image-worker extract sys.img -o ./out --device vendor=vend.img  # Split image\n\
  super-image-worker extract super.img --dry-run                # Names + sizes only\n\
  super-image-worker extract super.img -o ./out --force         # Overwrite outputs"
)]
pub struct ExtractArgs {
    /// Path to super image (raw or Android-sparse format)
    pub image: PathBuf,

    /// Output directory for extracted .img files (created if not exists)
    #[arg(short, long, default_value = ".")]
    pub output: PathBuf,

    /// Extract only this partition name (e.g., system_a, vendor_b).
    /// Accepts base names together with --slot (e.g., -p system -s a).
    #[arg(short, long)]
    pub partition: Option<String>,

    /// Filter by slot suffix: a, b, or all
    #[arg(short, long, default_value = "all")]
    pub slot: String,

    /// Bind secondary block devices for split/retrofit images.
    /// Repeatable: `--device vendor=path` or `--device path` (auto-match).
    #[arg(long = "device")]
    pub device: Vec<String>,

    /// Overwrite existing .img files in output directory
    #[arg(long)]
    pub force: bool,

    /// Dry run - show partitions and sizes without extracting
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run(args: ExtractArgs) -> std::process::ExitCode {
    let slot = match args.slot.as_str() {
        "a" | "A" => Some("a"),
        "b" | "B" => Some("b"),
        "all" => None,
        other => {
            eprintln!("unknown slot: {other} (use a, b, all)");
            return std::process::ExitCode::FAILURE;
        }
    };

    // Slot-aware loading: `-s b` extracts from metadata slot 1,
    // `-s all` from every valid slot (dual-slot images).
    let datas = match split_util::load_for_slot_filter(&args.image, slot) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // Resolve partition list (supports base name + slot, across slots).
    // Each entry tracks its owning metadata slot for correct extents.
    let mut names: Vec<(usize, String)> = Vec::new();
    if let Some(ref name) = args.partition {
        match split_util::find_partition_across(&datas, name, slot) {
            Ok((di, pi)) => {
                if let Some(p) = datas.get(di).and_then(|d| d.partitions.get(pi)) {
                    names.push((di, p.name.clone()));
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        for (di, data) in datas.iter().enumerate() {
            // Suffix filter still applies inside each slot (mixed slots
            // hold both `_a` and empty `_b` placeholders).
            for p in data.filter_by_slot(slot) {
                names.push((di, p.name.clone()));
            }
        }
    }

    if names.is_empty() {
        eprintln!("no partitions to extract");
        return std::process::ExitCode::FAILURE;
    }

    if !args.dry_run
        && let Err(e) = fs::create_dir_all(&args.output)
    {
        eprintln!("error creating output directory: {e}");
        return std::process::ExitCode::FAILURE;
    }

    for (di, name) in &names {
        let data = match datas.get(*di) {
            Some(d) => d,
            None => continue,
        };
        if let Some(p) = data.partitions.iter().find(|p| p.name == *name) {
            let size = data.partition_size(p);
            if args.dry_run {
                println!("{} {} ({})", p.name, size, crate::output::human_size(size));
            } else {
                let out_path = args.output.join(format!("{}.img", p.name));
                if out_path.exists() && !args.force {
                    eprintln!(
                        "skipping {}: file exists (use --force to overwrite)",
                        p.name
                    );
                }
            }
        }
    }
    if args.dry_run {
        return std::process::ExitCode::SUCCESS;
    }

    // Split detection across all loaded slots (any multi-device slot
    // forces the multiblock path; bindings resolve against slot 0 layout).
    let split = datas.iter().any(|d| d.devices.len() > 1) || !args.device.is_empty();
    // Pre-resolve split bindings once; rayon threads only open files.
    let mut bindings: Vec<(u32, PathBuf)> = Vec::new();
    if split {
        let ref_data = &datas[0];
        match super_image_worker_core::resolve_device_bindings(&ref_data.devices, &args.device, &args.image) {
            Ok(map) => {
                bindings = map.into_iter().collect();
                bindings.sort_by_key(|(i, _)| *i);
            }
            Err(e) => {
                eprintln!("error: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    }

    // Snapshot extent vectors per partition so rayon threads share no state.
    struct Job {
        name: String,
        out_path: PathBuf,
        extents: Vec<super_image_worker_core::Extent>,
    }
    let mut jobs: Vec<Job> = Vec::new();
    for (di, name) in &names {
        let data = match datas.get(*di) {
            Some(d) => d,
            None => continue,
        };
        let part_idx = match data.partitions.iter().position(|x| x.name == *name) {
            Some(i) => i,
            None => continue,
        };
        let pname = match data.partitions.get(part_idx) {
            Some(p) => p.name.clone(),
            None => continue,
        };
        let out_path = args.output.join(format!("{pname}.img"));
        if out_path.exists() && !args.force {
            continue;
        }
        let extents = match split_util::partition_extents(data, part_idx) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("error: {e}");
                return std::process::ExitCode::FAILURE;
            }
        };
        if extents.is_empty() {
            // Create empty file for extent-less partitions.
            match fs::write(&out_path, b"") {
                Ok(_) => println!("{pname}: no extents, created empty file"),
                Err(e) => eprintln!("error writing {pname}: {e}"),
            }
            continue;
        }
        jobs.push(Job {
            name: pname,
            out_path,
            extents,
        });
    }

    let image_path = args.image.clone();
    let results: Vec<(String, std::result::Result<u64, String>)> = jobs
        .into_par_iter()
        .map(|job| {
            let file = match fs::File::create(&job.out_path) {
                Ok(f) => f,
                Err(e) => return (job.name, Err(format!("create failed: {e}"))),
            };
            let mut writer = BufWriter::new(file);
            if split {
                // Isolated multiblock handle per thread: no Seek races.
                let mut mb = match super_image_worker_core::MultiBlockImage::open_primary(&image_path) {
                    Ok(m) => m,
                    Err(e) => return (job.name, Err(format!("reopen failed: {e}"))),
                };
                for (idx, path) in &bindings {
                    if let Err(e) = mb.bind(*idx, path) {
                        return (job.name, Err(format!("bind failed: {e}")));
                    }
                }
                match extract_partition_split(&mut mb, &job.extents, &mut writer) {
                    Ok(n) => (job.name, Ok(n)),
                    Err(e) => {
                        let _ = fs::remove_file(&job.out_path);
                        (job.name, Err(format!("{e}")))
                    }
                }
            } else {
                // Isolated image handle per thread: no Seek races.
                let mut image = match Image::open(&image_path) {
                    Ok(i) => i,
                    Err(e) => return (job.name, Err(format!("reopen failed: {e}"))),
                };
                match extract_partition(&mut image, &job.extents, &mut writer) {
                    Ok(n) => (job.name, Ok(n)),
                    Err(e) => {
                        let _ = fs::remove_file(&job.out_path);
                        (job.name, Err(format!("{e}")))
                    }
                }
            }
        })
        .collect();

    let mut failed = false;
    let mut ordered = results;
    ordered.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, res) in ordered {
        match res {
            Ok(n) => println!("{name}: extracted {n} bytes"),
            Err(e) => {
                eprintln!("error extracting {name}: {e}");
                failed = true;
            }
        }
    }

    if failed {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}
