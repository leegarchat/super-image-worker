use super::split_util;
use clap::Args;
use super_image_worker_core::{ExtentReader, Image, MultiBlockImage, SplitExtentReader};
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;

#[derive(Args)]
#[command(
    about = "Read partition data to stdout (streaming)",
    long_about = "Read raw partition data and output to stdout for piping.\n\n\
Streams partition bytes (following the full multi-extent chain across all\n\
bound block devices) directly to stdout with O(1) RAM - no intermediate\n\
files. Ideal for pipelines into file, md5sum/sha256sum, dd, xxd, simg2img.\n\
A closed pipe (`| head -c`) exits 0 via graceful BrokenPipe handling.\n\
Accepts raw and Android-sparse images; read-only, no root needed.\n\n\
PARTITION SELECTION: -p takes a full name (system_a) or a base name plus\n\
--suffix (system --suffix a). --slot <index|all> picks the metadata copy.\nNote: -s is --size here, so --suffix/--slot are long-only.\n\
Slotless partitions match any suffix.\n\n\
RANGE (--skip/--size, plain bytes or K/M/G suffix, e.g. 10M, 512K, 2G):\n\
  default streams the whole partition; --skip drops a prefix, --size caps\n\
  the length. Values beyond end-of-partition are clamped, not errors.\n\n\
SPLIT / RETROFIT: repeatable --device (vendor=path or auto-matched path).\n\
Missing bindings fail with the exact device name to pass.\n\n\
EXAMPLES:\n\
  super-image-worker read super.img -p odm_a > odm.img\n\
  super-image-worker read super.img -p system --suffix a | file -\n\
  super-image-worker read super.img -p vendor_a | sha256sum\n\
  super-image-worker read super.img -p vendor_a --skip 1M --size 10M | xxd | head\n\
  super-image-worker read super.img -p system_a --size 100M | simg2img - out.raw\n\
  super-image-worker read sys.img -p vendor_a --device vendor=vend.img > vendor.img"
)]
pub struct ReadArgs {
    /// Path to super image (raw or sparse format)
    pub image: PathBuf,

    /// Partition name to read (base name allowed with --suffix)
    #[arg(short, long)]
    pub partition: String,

    /// Metadata slot to read: 0|a, 1|b, ... or all (default: all).
    /// Selects which LP metadata copy is used, independent of names.
    /// (Long flag only: -s is taken by --size.)
    #[arg(long, default_value = "all")]
    pub slot: String,

    /// Extra name filter (long flag only): a, b, or all (default: all).
    /// Slotless partitions always match.
    #[arg(long, default_value = "all")]
    pub suffix: String,

    /// Bind secondary block devices for split/retrofit images.
    /// Repeatable: `--device vendor=path` or `--device path` (auto-match).
    #[arg(long = "device")]
    pub device: Vec<String>,

    /// Max bytes to read (default: entire partition)
    /// Format: plain bytes or with K/M/G suffix
    #[arg(short, long)]
    pub size: Option<String>,

    /// Skip bytes from the start of partition
    /// Format: plain bytes or with K/M/G suffix
    #[arg(long)]
    pub skip: Option<String>,
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(0);
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
        .ok_or_else(|| "size overflow".to_string())
}

enum ActiveReader<'a> {
    Single(ExtentReader<'a>),
    Split(SplitExtentReader<'a>),
}

impl<'a> ActiveReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> super_image_worker_core::Result<usize> {
        match self {
            Self::Single(r) => r.read(buf),
            Self::Split(r) => r.read(buf),
        }
    }
}

pub fn run(args: ReadArgs) -> std::process::ExitCode {
    // --slot picks the metadata copy (index), --suffix the name letters.
    let slot = match split_util::parse_slot_opt(&args.slot) {
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

    let datas = match split_util::load_for_slot_filter(&args.image, slot) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let (slot_pos, part_idx) =
        match split_util::find_partition_across(&datas, &args.partition, suffix) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: {e}");
                return std::process::ExitCode::FAILURE;
            }
        };
    let data = match datas.get(slot_pos) {
        Some(d) => d,
        None => {
            eprintln!("error: metadata slot not found");
            return std::process::ExitCode::FAILURE;
        }
    };

    let extents = match split_util::partition_extents(data, part_idx) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    if extents.is_empty() {
        return std::process::ExitCode::SUCCESS;
    }

    let mut total_size: u64 = 0;
    for e in &extents {
        total_size = total_size.saturating_add(
            e.num_sectors
                .saturating_mul(super_image_worker_core::LP_SECTOR_SIZE),
        );
    }

    let skip = match args.skip {
        Some(ref s) => match parse_size(s) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: {e}");
                return std::process::ExitCode::FAILURE;
            }
        },
        None => 0,
    };
    let max_read = match args.size {
        Some(ref s) => match parse_size(s) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: {e}");
                return std::process::ExitCode::FAILURE;
            }
        },
        None => total_size,
    };
    if skip >= total_size {
        return std::process::ExitCode::SUCCESS;
    }
    let max_read = max_read.min(total_size.saturating_sub(skip));

    let split = data.devices.len() > 1 || !args.device.is_empty();
    let mut image_opt: Option<Image> = None;
    let mut mb_opt: Option<MultiBlockImage> = None;
    if split {
        match split_util::open_split(&args.image, &args.device, data) {
            Ok(mb) => mb_opt = Some(mb),
            Err(e) => {
                eprintln!("error: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        match Image::open(&args.image) {
            Ok(i) => image_opt = Some(i),
            Err(e) => {
                eprintln!("error: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    }

    let mut reader = match (image_opt.as_mut(), mb_opt.as_mut()) {
        (Some(image), _) => ActiveReader::Single(ExtentReader::new(image, extents)),
        (_, Some(mb)) => ActiveReader::Split(SplitExtentReader::new(mb, extents)),
        _ => {
            eprintln!("error: no image opened");
            return std::process::ExitCode::FAILURE;
        }
    };
    if skip > 0 {
        let mut skip_buf = vec![0u8; (skip.min(65536)) as usize];
        let mut remaining = skip;
        while remaining > 0 {
            let to_read = remaining.min(skip_buf.len() as u64) as usize;
            let chunk = match skip_buf.get_mut(..to_read) {
                Some(c) => c,
                None => {
                    eprintln!("error: skip buffer range error");
                    return std::process::ExitCode::FAILURE;
                }
            };
            match reader.read(chunk) {
                Ok(0) => break,
                Ok(n) => remaining = remaining.saturating_sub(n as u64),
                Err(e) => {
                    eprintln!("error skipping: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            }
        }
    }

    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let mut buf = vec![0u8; 1024 * 1024];
    let mut written = 0u64;

    while written < max_read {
        let remaining = max_read.saturating_sub(written) as usize;
        let to_read = remaining.min(buf.len());
        if to_read == 0 {
            break;
        }
        let chunk = match buf.get_mut(..to_read) {
            Some(c) => c,
            None => {
                eprintln!("error: read buffer range error");
                return std::process::ExitCode::FAILURE;
            }
        };
        match reader.read(chunk) {
            Ok(0) => break,
            Ok(n) => {
                let out_slice = match chunk.get(..n) {
                    Some(s) => s,
                    None => {
                        eprintln!("error: output slice range error");
                        return std::process::ExitCode::FAILURE;
                    }
                };
                if let Err(e) = out.write_all(out_slice) {
                    if e.kind() == io::ErrorKind::BrokenPipe {
                        let _ = out.flush();
                        return std::process::ExitCode::SUCCESS;
                    }
                    eprintln!("error writing: {e}");
                    return std::process::ExitCode::FAILURE;
                }
                written = written.saturating_add(n as u64);
            }
            Err(e) => {
                eprintln!("error reading: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    }

    // BrokenPipe on flush (e.g. `| head -c`) is not an error.
    match out.flush() {
        Ok(_) => std::process::ExitCode::SUCCESS,
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error flushing: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
