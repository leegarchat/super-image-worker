use clap::Args;
use super_image_worker_core::{LP_TARGET_TYPE_LINEAR, LP_TARGET_TYPE_ZERO, load_super};
use std::ffi::CString;
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
const DM_ACTIVE_PRESENT_FLAG: u32 = 4;

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
NAME RESOLUTION: full name (-p system_a) or base name plus --slot.\n\n\
EXAMPLES:\n\
  super-image-worker map super.img -p system_a\n\
  super-image-worker map super.img -p vendor -s a\n\
  super-image-worker map super.img -p system_a --force-writable"
)]
pub struct MapArgs {
    /// Path to super image or block device (e.g., /dev/block/by-name/super)
    pub image: PathBuf,

    /// Partition name to map as device-mapper device
    #[arg(short, long)]
    pub partition: String,

    /// Filter by slot suffix: a, b, or all
    #[arg(short, long, default_value = "all")]
    pub slot: String,

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

fn resolve_partition(
    data: &super_image_worker_core::SuperData,
    name: &str,
    slot: &str,
) -> Result<super_image_worker_core::Partition, String> {
    let slot_filter = match slot {
        "a" | "A" => Some("a"),
        "b" | "B" => Some("b"),
        "all" => None,
        other => return Err(format!("unknown slot: {other} (use a, b, all)")),
    };

    let candidates: Vec<&super_image_worker_core::Partition> = data
        .partitions
        .iter()
        .filter(|p| {
            if p.name != name && super_image_worker_core::SuperData::strip_suffix(&p.name) != name {
                return false;
            }
            if let Some(sfx) = slot_filter {
                super_image_worker_core::SuperData::find_suffix(&p.name).is_none_or(|s| s == sfx)
            } else {
                true
            }
        })
        .collect();

    match candidates.len() {
        0 => Err(format!("partition '{name}' not found")),
        1 => Ok(candidates[0].clone()),
        _ => {
            let names: Vec<&str> = candidates.iter().map(|p| p.name.as_str()).collect();
            Err(format!(
                "ambiguous name '{name}', matches: {names:?}. Use full name or --slot"
            ))
        }
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

    let data = match load_super(&args.image) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let p = match resolve_partition(&data, &args.partition, &args.slot) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    if p.num_extents == 0 {
        eprintln!("error: partition '{}' has no data", p.name);
        return std::process::ExitCode::FAILURE;
    }

    let abs_image = match std::fs::canonicalize(&args.image) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let super_path = abs_image.to_string_lossy().to_string();

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
        return std::process::ExitCode::FAILURE;
    }

    if let Err(e) = dm_table_load(dm_fd, &p.name, &table_data, target_count, readonly) {
        eprintln!("error loading table: {e}");
        let _ = dm_remove_device(dm_fd, &p.name);
        unsafe { libc::close(dm_fd) };
        return std::process::ExitCode::FAILURE;
    }

    if let Err(e) = dm_resume(dm_fd, &p.name) {
        eprintln!("error activating device: {e}");
        let _ = dm_remove_device(dm_fd, &p.name);
        unsafe { libc::close(dm_fd) };
        return std::process::ExitCode::FAILURE;
    }

    unsafe { libc::close(dm_fd) };

    let dm_path = format!("/dev/block/mapper/{}", p.name);
    println!("{dm_path}");

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

    let flags = match dm_get_state(dm_fd, &args.partition) {
        Ok(f) => f,
        Err(_) => {
            eprintln!("error: device '{}' not found", args.partition);
            unsafe { libc::close(dm_fd) };
            return std::process::ExitCode::FAILURE;
        }
    };

    if flags & DM_ACTIVE_PRESENT_FLAG == 0 {
        eprintln!("error: device '{}' is not active", args.partition);
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
