use super::split_util;
use clap::Args;
use super_image_worker_core::LP_SECTOR_SIZE;
use std::ffi::CString;
use std::path::PathBuf;

/// Resolve the loop device node for a given number.
/// Stock Linux uses `/dev/loopN`; Android exposes loops as
/// `/dev/block/loopN` (shiba has 54 there and no `/dev/loopN` nodes).
/// Prefer whichever node exists, falling back to `/dev/block/loopN`.
fn loop_node(nr: i32) -> String {
    let a = format!("/dev/loop{nr}");
    if std::path::Path::new(&a).exists() {
        return a;
    }
    let b = format!("/dev/block/loop{nr}");
    if std::path::Path::new(&b).exists() {
        return b;
    }
    // Default to the Linux path when neither exists yet (loop-control
    // may still allocate it on demand).
    a
}

fn open_loop_node(nr: i32, flags: i32) -> i32 {
    let primary = loop_node(nr);
    let c = match CString::new(primary.as_str()) {
        Ok(c) => c,
        Err(_) => return -1,
    };
    let fd = unsafe { libc::open(c.as_ptr(), flags) };
    if fd >= 0 {
        return fd;
    }
    // Try the alternate prefix before giving up.
    let alt = if primary.starts_with("/dev/block/loop") {
        format!("/dev/loop{nr}")
    } else {
        format!("/dev/block/loop{nr}")
    };
    let c2 = match CString::new(alt.as_str()) {
        Ok(c) => c,
        Err(_) => return -1,
    };
    unsafe { libc::open(c2.as_ptr(), flags) }
}

fn backing_file_for(nr: i32) -> Option<String> {
    let path = format!("/sys/block/loop{nr}/loop/backing_file");
    std::fs::read_to_string(&path).ok().map(|s| s.trim().to_string())
}

const LOOP_CTL_GET_FREE: u64 = 0x4C82;
const LOOP_SET_FD: u64 = 0x4C00;
const LOOP_CLR_FD: u64 = 0x4C01;
const LOOP_SET_STATUS64: u64 = 0x4C04;
const LOOP_GET_STATUS64: u64 = 0x4C05;
const LO_FLAGS_PARTSCAN: u32 = 8;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct LoopInfo64 {
    lo_device: u64,
    lo_inode: u64,
    lo_rdevice: u64,
    lo_offset: u64,
    lo_sizelimit: u64,
    lo_number: u32,
    lo_encrypt_type: u32,
    lo_encrypt_key_size: u32,
    lo_flags: u32,
    lo_file_name: [u8; 64],
    lo_crypt_name: [u8; 64],
    lo_encrypt_key: [u8; 32],
    lo_init: [u64; 2],
}

#[derive(Args)]
#[command(
    about = "Connect partition as a loop block device (Linux + Android)",
    long_about = "Create a loop block device (/dev/loopN) pointing to a partition in super image.\n\n\
Works on Linux and Android (kernel 4+). Uses direct ioctl calls to\n\
/dev/loop-control and /dev/loopN - no losetup binary required. Prints the\n\
allocated device path (e.g. /dev/loop14).\n\n\
REQUIREMENTS: root (sudo/su/ksu/magisk); the partition must be a SINGLE\n\
linear extent - fragmented (multi-extent) partitions are refused with a hint\n\
to use device-mapper (`map` on Android) or plain `extract`/`read`, because one\n\
loop device cannot express scattered blocks. Empty (extent-less) partitions\n\
and non-linear extents are refused as well.\n\n\
The created device behaves like any block device:\n\
  - Mount filesystem: sudo mount /dev/loop14 /mnt\n\
  - Check filesystem: sudo fsck /dev/loop14\n\
  - Read data: sudo dd if=/dev/loop14 of=part.img\n\
Pair with `disconnect` for teardown (matched by image path + offset + size).\n\n\
NAME RESOLUTION: full name (-p odm_a) or base name plus --slot (-p system -s a).\n\n\
EXAMPLES:\n\
  sudo super-image-worker connect super.img -p odm_a\n\
  sudo super-image-worker connect super.img -p system -s a\n\
  sudo super-image-worker connect super.img -p vendor_b\n\
  sudo super-image-worker disconnect super.img -p odm_a"
)]
pub struct ConnectArgs {
    /// Path to super image (raw or sparse format)
    pub image: PathBuf,

    /// Partition name to connect as loop device
    #[arg(short, long)]
    pub partition: String,

    /// Filter by slot suffix: a, b, or all
    /// Use when partition name is ambiguous (e.g., 'system' matches system_a and system_b)
    #[arg(short, long, default_value = "all")]
    pub slot: String,
}

#[derive(Args)]
#[command(
    about = "Disconnect a loop block device (Linux + Android)",
    long_about = "Remove a loop block device created by 'connect'.\n\n\
Finds the loop device by matching the super image path plus the partition's\n\
offset and size (via LOOP_GET_STATUS64 and\n\
/sys/block/loopN/loop/backing_file), then detaches it with LOOP_CLR_FD.\n\
Single-extent partitions only (mirrors `connect`). Requires root.\n\n\
EXAMPLES:\n\
  sudo super-image-worker disconnect super.img -p odm_a\n\
  sudo super-image-worker disconnect super.img -p system -s a"
)]
pub struct DisconnectArgs {
    /// Path to super image (same as used in 'connect')
    pub image: PathBuf,

    /// Partition name to disconnect
    #[arg(short, long)]
    pub partition: String,

    /// Filter by slot suffix: a, b, or all
    #[arg(short, long, default_value = "all")]
    pub slot: String,
}

fn check_root() -> Result<(), String> {
    unsafe {
        if libc::geteuid() != 0 {
            return Err("connect/disconnect requires root privileges".into());
        }
    }
    Ok(())
}

fn get_free_loop() -> Result<i32, String> {
    let ctl_path = CString::new("/dev/loop-control").unwrap();
    let ctl_fd = unsafe { libc::open(ctl_path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if ctl_fd < 0 {
        return Err(format!(
            "failed to open /dev/loop-control: {}",
            std::io::Error::last_os_error()
        ));
    }

    let dev_nr = unsafe { libc::ioctl(ctl_fd, LOOP_CTL_GET_FREE as _) };
    unsafe { libc::close(ctl_fd) };

    if dev_nr < 0 {
        return Err(format!(
            "LOOP_CTL_GET_FREE failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let dev_nr = dev_nr as i32;

    // LOOP_CTL_GET_FREE may return an index with no device node yet
    // (shiba pre-creates only /dev/block/loop0..53; GET_FREE returned 54
    // with no node). Prefer the hint when its node exists, otherwise
    // scan existing nodes for a free one.
    let hint_node = loop_node(dev_nr);
    if std::path::Path::new(&hint_node).exists() {
        return Ok(dev_nr);
    }
    for nr in 0..256 {
        let node = loop_node(nr);
        if !std::path::Path::new(&node).exists() {
            continue;
        }
        // Free when no backing file (or status query fails).
        let bf = format!("/sys/block/loop{nr}/loop/backing_file");
        match std::fs::read_to_string(&bf) {
            Ok(content) if !content.trim().is_empty() => continue,
            _ => return Ok(nr),
        }
    }
    // No existing free node; return the hint and let the caller report
    // the missing node clearly.
    Ok(dev_nr)
}

fn setup_loop(dev_nr: i32, image_fd: i32, offset: u64, size: u64) -> Result<(), String> {
    let loop_fd = open_loop_node(dev_nr, libc::O_RDWR | libc::O_CLOEXEC);
    if loop_fd < 0 {
        return Err(format!(
            "failed to open {}: {}",
            loop_node(dev_nr),
            std::io::Error::last_os_error()
        ));
    }

    if unsafe { libc::ioctl(loop_fd, LOOP_SET_FD as _, image_fd) } < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(loop_fd) };
        return Err(format!("LOOP_SET_FD failed: {err}"));
    }

    let mut info: LoopInfo64 = unsafe { std::mem::zeroed() };
    info.lo_offset = offset;
    info.lo_sizelimit = size;
    info.lo_flags = LO_FLAGS_PARTSCAN;

    if unsafe { libc::ioctl(loop_fd, LOOP_SET_STATUS64 as _, &info) } < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::ioctl(loop_fd, LOOP_CLR_FD as _, 0) };
        unsafe { libc::close(loop_fd) };
        return Err(format!("LOOP_SET_STATUS64 failed: {err}"));
    }

    unsafe { libc::close(loop_fd) };
    Ok(())
}

#[allow(clippy::collapsible_if)]
fn find_loop_for_partition(image_path: &str, offset: u64, size: u64) -> Result<i32, String> {
    let abs_path =
        std::fs::canonicalize(image_path).map_err(|e| format!("failed to resolve path: {e}"))?;
    let abs_str = abs_path.to_string_lossy().to_string();
    let abs_deleted = format!("{abs_str} (deleted)");

    for nr in 0..256 {
        let loop_fd = open_loop_node(nr, libc::O_RDONLY | libc::O_CLOEXEC);
        if loop_fd < 0 {
            continue;
        }

        let mut info: LoopInfo64 = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::ioctl(loop_fd, LOOP_GET_STATUS64 as _, &mut info) };

        if ret < 0 {
            unsafe { libc::close(loop_fd) };
            continue;
        }

        if info.lo_offset == offset && info.lo_sizelimit == size {
            if let Some(backing_path) = backing_file_for(nr) {
                if backing_path == abs_str || backing_path == abs_deleted {
                    return Ok(loop_fd);
                }
            }
        }

        unsafe { libc::close(loop_fd) };
    }

    Err(format!(
        "no loop device found for {image_path} offset=0x{offset:x} size={size}"
    ))
}

fn parse_slot_filter(slot: &str) -> Result<Option<&str>, String> {
    match slot {
        "a" | "A" => Ok(Some("a")),
        "b" | "B" => Ok(Some("b")),
        "all" => Ok(None),
        other => Err(format!("unknown slot: {other} (use a, b, all)")),
    }
}

pub fn run_connect(args: ConnectArgs) -> std::process::ExitCode {
    if let Err(e) = check_root() {
        eprintln!("error: {e}");
        return std::process::ExitCode::FAILURE;
    }

    let slot_filter = match parse_slot_filter(&args.slot) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let datas = match split_util::load_for_slot_filter(&args.image, slot_filter) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let (slot_pos, part_idx) =
        match split_util::find_partition_across(&datas, &args.partition, slot_filter) {
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
    let p = match data.partitions.get(part_idx).cloned() {
        Some(p) => p,
        None => {
            eprintln!("error: invalid partition index");
            return std::process::ExitCode::FAILURE;
        }
    };

    if p.num_extents == 0 {
        eprintln!("error: partition '{}' has no data", p.name);
        return std::process::ExitCode::FAILURE;
    }

    if p.num_extents > 1 {
        eprintln!(
            "error: partition '{}' is fragmented ({} extents) and cannot be mapped as a single loop device (would capture foreign blocks)",
            p.name, p.num_extents
        );
        eprintln!(
            "hint: use 'map' (device-mapper) for multi-extent partitions, or 'extract'/'read' for data access"
        );
        return std::process::ExitCode::FAILURE;
    }

    let first_extent = match data.extents.get(p.first_extent_index as usize) {
        Some(e) => e,
        None => {
            eprintln!("error: invalid extent index");
            return std::process::ExitCode::FAILURE;
        }
    };

    if first_extent.target_type != super_image_worker_core::LP_TARGET_TYPE_LINEAR {
        eprintln!("error: only linear extents are supported");
        return std::process::ExitCode::FAILURE;
    }

    let offset = match first_extent.target_data.checked_mul(LP_SECTOR_SIZE) {
        Some(o) => o,
        None => {
            eprintln!("error: physical offset overflow");
            return std::process::ExitCode::FAILURE;
        }
    };
    let size = data.partition_size(&p);

    let abs_image = match std::fs::canonicalize(&args.image) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let image_path = match CString::new(abs_image.to_string_lossy().as_bytes()) {
        Ok(p) => p,
        Err(_) => {
            eprintln!("error: invalid path");
            return std::process::ExitCode::FAILURE;
        }
    };

    unsafe {
        let image_fd = libc::open(image_path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
        if image_fd < 0 {
            eprintln!(
                "error: failed to open image: {}",
                std::io::Error::last_os_error()
            );
            return std::process::ExitCode::FAILURE;
        }

        let dev_nr = match get_free_loop() {
            Ok(n) => n,
            Err(e) => {
                eprintln!("error: {e}");
                libc::close(image_fd);
                return std::process::ExitCode::FAILURE;
            }
        };

        if let Err(e) = setup_loop(dev_nr, image_fd, offset, size) {
            eprintln!("error: {e}");
            libc::close(image_fd);
            return std::process::ExitCode::FAILURE;
        }

        libc::close(image_fd);
        println!("{}", loop_node(dev_nr));
    }

    std::process::ExitCode::SUCCESS
}

pub fn run_disconnect(args: DisconnectArgs) -> std::process::ExitCode {
    if let Err(e) = check_root() {
        eprintln!("error: {e}");
        return std::process::ExitCode::FAILURE;
    }

    let slot_filter = match parse_slot_filter(&args.slot) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let datas = match split_util::load_for_slot_filter(&args.image, slot_filter) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let (slot_pos, part_idx) =
        match split_util::find_partition_across(&datas, &args.partition, slot_filter) {
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
    let p = match data.partitions.get(part_idx).cloned() {
        Some(p) => p,
        None => {
            eprintln!("error: invalid partition index");
            return std::process::ExitCode::FAILURE;
        }
    };

    if p.num_extents == 0 {
        eprintln!("error: partition '{}' has no data", p.name);
        return std::process::ExitCode::FAILURE;
    }

    if p.num_extents > 1 {
        eprintln!(
            "error: partition '{}' is fragmented ({} extents); disconnect requires single-extent loop mapping",
            p.name, p.num_extents
        );
        return std::process::ExitCode::FAILURE;
    }

    let first_extent = match data.extents.get(p.first_extent_index as usize) {
        Some(e) => e,
        None => {
            eprintln!("error: invalid extent index");
            return std::process::ExitCode::FAILURE;
        }
    };

    let offset = match first_extent.target_data.checked_mul(LP_SECTOR_SIZE) {
        Some(o) => o,
        None => {
            eprintln!("error: physical offset overflow");
            return std::process::ExitCode::FAILURE;
        }
    };
    let size = data.partition_size(&p);

    let image_str = args.image.to_string_lossy().to_string();

    let loop_fd = match find_loop_for_partition(&image_str, offset, size) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    if unsafe { libc::ioctl(loop_fd, LOOP_CLR_FD as _, 0) } < 0 {
        let e = std::io::Error::last_os_error();
        unsafe { libc::close(loop_fd) };
        eprintln!("error: LOOP_CLR_FD failed: {e}");
        return std::process::ExitCode::FAILURE;
    }

    unsafe { libc::close(loop_fd) };

    println!("disconnected loop device");
    std::process::ExitCode::SUCCESS
}
