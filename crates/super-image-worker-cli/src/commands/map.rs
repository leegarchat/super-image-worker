use super::split_util;
use clap::Args;
use super_image_worker_core::{LP_TARGET_TYPE_LINEAR, LP_TARGET_TYPE_ZERO};
use std::ffi::CString;
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;

const DM_IOCTL_VERSION: u32 = 4;
const DM_NAME_LEN: usize = 128;
const DM_UUID_LEN: usize = 129;

const DM_DEV_CREATE: u64 = 0xc138fd03;
const DM_DEV_REMOVE: u64 = 0xc138fd04;
const DM_TABLE_LOAD: u64 = 0xc138fd09;
const DM_DEV_SUSPEND: u64 = 0xc138fd06;
const DM_DEV_STATUS: u64 = 0xc138fd07;

const DM_READONLY_FLAG: u32 = 1;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct DmIoctl {
    version: [u32; 3],
    data_size: u32,
    data_start: u32,
    target_count: u32,
    open_count: i32,
    flags: u32,
    event_nr: u32,
    padding: u32,
    dev: u64,
    name: [u8; DM_NAME_LEN],
    uuid: [u8; DM_UUID_LEN],
    data: [u8; 7],
}

impl DmIoctl {
    fn new(name: &str) -> Self {
        let mut io: DmIoctl = unsafe { std::mem::zeroed() };
        io.version[0] = DM_IOCTL_VERSION;
        io.version[1] = 0;
        io.version[2] = 0;
        io.data_size = std::mem::size_of::<DmIoctl>() as u32;
        io.data_start = 0;
        io.target_count = 0;
        io.open_count = 0;
        io.flags = 0;
        io.event_nr = 0;
        io.padding = 0;
        io.dev = 0;

        let name_bytes = name.as_bytes();
        let copy_len = name_bytes.len().min(DM_NAME_LEN - 1);
        io.name[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

        io
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct DmTargetSpec {
    sector_start: u64,
    length: u64,
    status: i32,
    next: u32,
    target_type: [u8; 16],
}

fn is_android_environment() -> bool {
    // Native Android / recovery markers. All of these are absent on a
    // standard Linux host distribution.
    std::path::Path::new("/system/build.prop").exists()
        || std::path::Path::new("/sbin/recovery").exists()
        || std::path::Path::new("/system/bin/getprop").exists()
        || std::path::Path::new("/dev/block/mapper").exists()
}

fn check_android_environment() -> Result<(), String> {
    // 'map'/'unmap' drive /dev/block/mapper/* native block devices and are
    // designed exclusively for Android OS / Recovery (TWRP/OrangeFox/AOSP).
    // On a host Linux distribution use 'connect'/'disconnect' (loop devices).
    if !is_android_environment() {
        return Err("'map'/'unmap' is designed for Android/Recovery block devices. On Linux host, use 'connect' and 'disconnect' instead.".into());
    }
    if !std::path::Path::new("/dev/device-mapper").exists() {
        return Err("device-mapper not available: /dev/device-mapper missing".into());
    }
    Ok(())
}

fn check_root() -> Result<(), String> {
    unsafe {
        if libc::geteuid() != 0 {
            return Err("map/unmap requires root privileges".into());
        }
    }
    Ok(())
}

fn dm_ioctl(fd: i32, request: u64, io: &mut DmIoctl) -> Result<(), String> {
    let ret = unsafe { libc::ioctl(fd, request as _, io as *mut DmIoctl) };
    if ret < 0 {
        return Err(format!("ioctl failed: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

fn open_dm() -> Result<i32, String> {
    let path = CString::new("/dev/device-mapper").unwrap();
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(format!(
            "failed to open /dev/device-mapper: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(fd)
}

fn dm_create_device(fd: i32, name: &str) -> Result<(), String> {
    let mut io = DmIoctl::new(name);
    dm_ioctl(fd, DM_DEV_CREATE, &mut io)
}

fn dm_remove_device(fd: i32, name: &str) -> Result<(), String> {
    let mut io = DmIoctl::new(name);
    dm_ioctl(fd, DM_DEV_REMOVE, &mut io)
}

fn dm_get_state(fd: i32, name: &str) -> Result<u32, String> {
    let mut io = DmIoctl::new(name);
    dm_ioctl(fd, DM_DEV_STATUS, &mut io)?;
    Ok(io.flags)
}

fn dm_table_load(
    fd: i32,
    name: &str,
    table_data: &[u8],
    target_count: u32,
    readonly: bool,
) -> Result<(), String> {
    let ioctl_size = std::mem::size_of::<DmIoctl>();
    let buffer_size = ioctl_size + table_data.len();
    let mut buffer = vec![0u8; buffer_size];

    let mut io = DmIoctl::new(name);
    io.data_size = buffer_size as u32;
    io.data_start = ioctl_size as u32;
    io.target_count = target_count;
    if readonly {
        io.flags |= DM_READONLY_FLAG;
    }

    buffer[..ioctl_size].copy_from_slice(unsafe {
        std::slice::from_raw_parts(&io as *const DmIoctl as *const u8, ioctl_size)
    });
    buffer[ioctl_size..].copy_from_slice(table_data);

    let ret = unsafe { libc::ioctl(fd, DM_TABLE_LOAD as _, buffer.as_mut_ptr()) };
    if ret < 0 {
        return Err(format!(
            "DM_TABLE_LOAD failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn dm_resume(fd: i32, name: &str) -> Result<(), String> {
    let mut io = DmIoctl::new(name);
    io.flags = 0;
    dm_ioctl(fd, DM_DEV_SUSPEND, &mut io)
}

fn build_linear_target(
    sector_start: u64,
    num_sectors: u64,
    device: &str,
    physical_sector: u64,
) -> Vec<u8> {
    let params = format!("{device} {physical_sector}\0");
    let spec_size = std::mem::size_of::<DmTargetSpec>();
    let padding = (8 - ((spec_size + params.len()) % 8)) % 8;

    let mut spec = DmTargetSpec {
        sector_start,
        length: num_sectors,
        status: 0,
        next: (spec_size + params.len() + padding) as u32,
        target_type: [0; 16],
    };
    spec.target_type[..6].copy_from_slice(b"linear");

    let mut data = Vec::with_capacity(spec.next as usize);
    data.extend_from_slice(unsafe {
        std::slice::from_raw_parts(&spec as *const DmTargetSpec as *const u8, spec_size)
    });
    data.extend_from_slice(params.as_bytes());
    data.extend(std::iter::repeat_n(0u8, padding));

    data
}

fn build_zero_target(sector_start: u64, num_sectors: u64) -> Vec<u8> {
    let spec_size = std::mem::size_of::<DmTargetSpec>();
    let mut spec = DmTargetSpec {
        sector_start,
        length: num_sectors,
        status: 0,
        next: spec_size as u32,
        target_type: [0; 16],
    };
    spec.target_type[..4].copy_from_slice(b"zero");

    let mut data = vec![0u8; spec_size];
    data.copy_from_slice(unsafe {
        std::slice::from_raw_parts(&spec as *const DmTargetSpec as *const u8, spec_size)
    });

    data
}

#[derive(Args)]
#[command(
    about = "Map partition via device-mapper (Android/Recovery only)",
    long_about = "Create a device-mapper device for a partition in super image.\n\n\
Works ONLY on Android or recovery (TWRP/OrangeFox/AOSP) with native block\n\
devices. Uses direct ioctl calls to /dev/device-mapper (DM_DEV_CREATE,\n\
DM_TABLE_LOAD, DM_DEV_SUSPEND). Unlike `connect`, expresses the FULL extent\n\
chain (linear + zero targets), so fragmented multi-extent partitions map\n\
correctly.\n\n\
Creates /dev/block/mapper/<partition_name> device that can be used for:\n\
  - Mounting filesystems\n\
  - Flashing with dd\n\
  - Direct block access\n\n\
Requires root privileges. The readonly flag follows the partition attributes\n\
unless --force-writable is passed. Prints the mapper path on success.\n\
On a Linux host use 'connect'/'disconnect' (loop devices) instead.\n\n\
NAME RESOLUTION: full name (-p system_a) or base name plus --suffix\n\
(-p system --suffix b); -s/--slot <0|a|1|b|...|all> picks the metadata copy.\n\n\
EXAMPLES:\n\
  super-image-worker map super.img -p system_a\n\
  super-image-worker map super.img -p vendor --suffix a\n\
  super-image-worker map super.img -p system_a --force-writable"
)]
pub struct MapArgs {
    /// Path to super image or block device (e.g., /dev/block/by-name/super)
    pub image: PathBuf,

    /// Partition name to map as device-mapper device
    #[arg(short, long)]
    pub partition: String,

    /// Metadata slot (-s): 0|a, 1|b, ... or all (default: all).
    /// Selects which LP metadata copy is used, independent of names.
    #[arg(short = 's', long, default_value = "all")]
    pub slot: String,

    /// Extra name filter (long flag only): keeps partitions with that
    /// name suffix (a, b, or all, default: all). Slotless partitions always match.
    #[arg(long, default_value = "all")]
    pub suffix: String,

    /// Force writable - ignore partition readonly attribute
    /// Useful for recovery/development scenarios
    #[arg(long)]
    pub force_writable: bool,
}

#[derive(Args)]
#[command(
    about = "Unmap device-mapper partition (Android/Recovery only)",
    long_about = "Remove a device-mapper device created by 'map'.\n\n\
Works ONLY on Android or recovery. Refuses unknown/inactive devices, then\n\
removes the mapping with the DM_DEV_REMOVE ioctl. Requires root privileges.\n\
On a Linux host use 'disconnect' instead.\n\n\
EXAMPLES:\n\
  super-image-worker unmap system_a\n\
  super-image-worker unmap vendor_b"
)]
pub struct UnmapArgs {
    /// Partition name to unmap (as shown by 'map' command)
    pub partition: String,
}

/// Loop node resolution shared with `connect`: Android keeps loops at
/// `/dev/block/loopN`, stock Linux at `/dev/loopN`.
fn loop_node(nr: i32) -> String {
    let a = format!("/dev/loop{nr}");
    if std::path::Path::new(&a).exists() {
        return a;
    }
    let b = format!("/dev/block/loop{nr}");
    if std::path::Path::new(&b).exists() {
        return b;
    }
    a
}

fn open_loop_node(nr: i32, flags: i32) -> i32 {
    let primary = loop_node(nr);
    if let Ok(c) = CString::new(primary.as_str()) {
        let fd = unsafe { libc::open(c.as_ptr(), flags) };
        if fd >= 0 {
            return fd;
        }
    }
    let alt = if primary.starts_with("/dev/block/loop") {
        format!("/dev/loop{nr}")
    } else {
        format!("/dev/block/loop{nr}")
    };
    if let Ok(c) = CString::new(alt.as_str()) {
        unsafe { libc::open(c.as_ptr(), flags) }
    } else {
        -1
    }
}

fn is_block_device(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.file_type().is_block_device())
        .unwrap_or(false)
}

/// dm-linear needs a block device as its base. A regular file image
/// (e.g. `/data/local/super.img`) is not directly mappable, so back it
/// by a whole-file loop device and return the loop node. Block devices
/// pass through unchanged (canonicalized).
/// Returns `(base_path, backing_loop_nr_or_None)`.
fn ensure_block_base(image: &std::path::Path) -> Result<(String, Option<i32>), String> {
    if is_block_device(image) {
        let abs = std::fs::canonicalize(image)
            .map_err(|e| format!("failed to resolve block device: {e}"))?;
        return Ok((abs.to_string_lossy().to_string(), None));
    }
    // Regular file: allocate a loop for the whole image.
    let abs = std::fs::canonicalize(image)
        .map_err(|e| format!("failed to resolve image path: {e}"))?;
    let abs_c = CString::new(abs.to_string_lossy().as_bytes())
        .map_err(|_| "invalid image path".to_string())?;
    let image_fd = unsafe { libc::open(abs_c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if image_fd < 0 {
        return Err(format!(
            "failed to open image for loop setup: {}",
            std::io::Error::last_os_error()
        ));
    }
    let ctl_c = CString::new("/dev/loop-control").unwrap();
    let ctl_fd = unsafe { libc::open(ctl_c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if ctl_fd < 0 {
        unsafe { libc::close(image_fd) };
        return Err(format!(
            "failed to open /dev/loop-control: {}",
            std::io::Error::last_os_error()
        ));
    }
    let dev_nr = unsafe { libc::ioctl(ctl_fd, 0x4C82 as _) };
    unsafe { libc::close(ctl_fd) };
    if dev_nr < 0 {
        unsafe { libc::close(image_fd) };
        return Err(format!(
            "LOOP_CTL_GET_FREE failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut dev_nr = dev_nr as i32;
    // GET_FREE may point past pre-created nodes (see connect.rs);
    // fall back to an existing free loop node.
    if !std::path::Path::new(&loop_node(dev_nr)).exists() {
        let mut found: Option<i32> = None;
        for nr in 0..256 {
            let node = loop_node(nr);
            if !std::path::Path::new(&node).exists() {
                continue;
            }
            let bf = format!("/sys/block/loop{nr}/loop/backing_file");
            match std::fs::read_to_string(&bf) {
                Ok(content) if !content.trim().is_empty() => continue,
                _ => {
                    found = Some(nr);
                    break;
                }
            }
        }
        if let Some(nr) = found {
            dev_nr = nr;
        }
    }
    let loop_fd = open_loop_node(dev_nr, libc::O_RDWR | libc::O_CLOEXEC);
    if loop_fd < 0 {
        unsafe { libc::close(image_fd) };
        return Err(format!(
            "failed to open {}: {}",
            loop_node(dev_nr),
            std::io::Error::last_os_error()
        ));
    }
    if unsafe { libc::ioctl(loop_fd, 0x4C00 as _, image_fd) } < 0 {
        let e = std::io::Error::last_os_error();
        unsafe { libc::close(loop_fd) };
        unsafe { libc::close(image_fd) };
        return Err(format!("LOOP_SET_FD failed: {e}"));
    }
    // Whole-file backing: offset 0, sizelimit 0.
    #[repr(C)]
    struct LoopInfo64Lite {
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
    let info: LoopInfo64Lite = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(loop_fd, 0x4C04 as _, &info) } < 0 {
        let e = std::io::Error::last_os_error();
        unsafe { libc::ioctl(loop_fd, 0x4C01 as _, 0) };
        unsafe { libc::close(loop_fd) };
        unsafe { libc::close(image_fd) };
        return Err(format!("LOOP_SET_STATUS64 failed: {e}"));
    }
    unsafe { libc::close(loop_fd) };
    unsafe { libc::close(image_fd) };
    Ok((loop_node(dev_nr), Some(dev_nr)))
}

fn clear_loop(nr: i32) {
    let fd = open_loop_node(nr, libc::O_RDWR | libc::O_CLOEXEC);
    if fd >= 0 {
        unsafe { libc::ioctl(fd, 0x4C01 as _, 0) };
        unsafe { libc::close(fd) };
    }
}

pub fn run_map(args: MapArgs) -> std::process::ExitCode {
    if let Err(e) = check_android_environment() {
        eprintln!("error: {e}");
        return std::process::ExitCode::FAILURE;
    }
    if let Err(e) = check_root() {
        eprintln!("error: {e}");
        return std::process::ExitCode::FAILURE;
    }

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

    // dm-linear needs a block device. Regular files are backed by a
    // whole-image loop automatically (old code passed the file path to
    // the kernel -> ENODEV).
    let (super_path, backing_loop) = match ensure_block_base(&args.image) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let readonly =
        (p.attributes & super_image_worker_core::LP_PARTITION_ATTR_READONLY != 0) && !args.force_writable;

    let mut table_data = Vec::new();
    let mut target_count = 0u32;
    let mut sector_offset = 0u64;

    for idx in p.first_extent_index..p.first_extent_index + p.num_extents {
        if let Some(extent) = data.extents.get(idx as usize) {
            match extent.target_type {
                LP_TARGET_TYPE_LINEAR => {
                    let target = build_linear_target(
                        sector_offset,
                        extent.num_sectors,
                        &super_path,
                        extent.target_data,
                    );
                    table_data.extend_from_slice(&target);
                    target_count += 1;
                }
                LP_TARGET_TYPE_ZERO => {
                    let target = build_zero_target(sector_offset, extent.num_sectors);
                    table_data.extend_from_slice(&target);
                    target_count += 1;
                }
                _ => {
                    eprintln!("error: unsupported extent type: {}", extent.target_type);
                    return std::process::ExitCode::FAILURE;
                }
            }
            sector_offset += extent.num_sectors;
        }
    }

    let dm_fd = match open_dm() {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    if let Err(e) = dm_create_device(dm_fd, &p.name) {
        eprintln!("error creating dm device: {e}");
        unsafe { libc::close(dm_fd) };
        if let Some(nr) = backing_loop {
            clear_loop(nr);
        }
        return std::process::ExitCode::FAILURE;
    }

    if let Err(e) = dm_table_load(dm_fd, &p.name, &table_data, target_count, readonly) {
        eprintln!("error loading table: {e}");
        let _ = dm_remove_device(dm_fd, &p.name);
        unsafe { libc::close(dm_fd) };
        if let Some(nr) = backing_loop {
            clear_loop(nr);
        }
        return std::process::ExitCode::FAILURE;
    }

    if let Err(e) = dm_resume(dm_fd, &p.name) {
        eprintln!("error activating device: {e}");
        let _ = dm_remove_device(dm_fd, &p.name);
        unsafe { libc::close(dm_fd) };
        if let Some(nr) = backing_loop {
            clear_loop(nr);
        }
        return std::process::ExitCode::FAILURE;
    }

    unsafe { libc::close(dm_fd) };

    let dm_path = format!("/dev/block/mapper/{}", p.name);
    println!("{dm_path}");
    if let Some(nr) = backing_loop {
        eprintln!("note: file-backed base loop {} kept for {dm_path}", loop_node(nr));
    }

    std::process::ExitCode::SUCCESS
}

pub fn run_unmap(args: UnmapArgs) -> std::process::ExitCode {
    if let Err(e) = check_android_environment() {
        eprintln!("error: {e}");
        return std::process::ExitCode::FAILURE;
    }
    if let Err(e) = check_root() {
        eprintln!("error: {e}");
        return std::process::ExitCode::FAILURE;
    }

    let dm_fd = match open_dm() {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // Existence check: the device must answer DM_DEV_STATUS.
    // (A previous ACTIVE_PRESENT bit check mis-detected freshly resumed
    // devices as inactive; dmsetup-style remove works on any present
    // device, active or suspended, so presence is the right gate.)
    if dm_get_state(dm_fd, &args.partition).is_err() {
        eprintln!("error: device '{}' not found", args.partition);
        unsafe { libc::close(dm_fd) };
        return std::process::ExitCode::FAILURE;
    }

    if let Err(e) = dm_remove_device(dm_fd, &args.partition) {
        eprintln!("error removing device: {e}");
        unsafe { libc::close(dm_fd) };
        return std::process::ExitCode::FAILURE;
    }

    unsafe { libc::close(dm_fd) };

    println!("unmapped {}", args.partition);
    std::process::ExitCode::SUCCESS
}
