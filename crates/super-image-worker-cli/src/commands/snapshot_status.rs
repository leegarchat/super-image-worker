use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
#[command(
    about = "Print OTA snapshot update state (snapshotctl dump analog)",
    long_about = "Report whether the system is inside a Virtual A/B update.\n\n\
Mirrors the `Update state:` line of `snapshotctl dump`, which DFE-style\n\
installers consume as `snapshotctl dump | grep '^Update state:'` with\n\
states none / initiated / unverified / merging / merge-completed /\n\
merge-needs-reboot / merge-failed / cancelled. A static binary cannot\n\
reach the SnapshotManager HAL, so this reads the same on-disk source\n\
directly: `<metadata-dir>/state` (default /metadata/ota/state).\n\n\
DECODING (matches libsnapshot ReadSnapshotUpdateStatus): a missing file\n\
means `none`; otherwise the file is a serialized SnapshotUpdateStatus\n\
proto and only field #1 (state enum, varint) is decoded with a tiny\n\
hand-rolled TLV scan (no protobuf dependency); unparseable content falls\n\
back to the legacy plain-text format (`none`, `initiated`, ...), and\n\
anything else degrades to `none`. Stdout carries exactly one line\n\
(`Update state: <state>`) for script piping; diagnostics go to stderr.\n\
Read-only, no root needed beyond file permissions. Exit code is always 0.\n\n\
EXAMPLES:\n\
  super-image-worker snapshot-status\n\
  super-image-worker snapshot-status | grep '^Update state:'\n\
  super-image-worker snapshot-status --metadata-dir /tmp/ota-test"
)]
pub struct SnapshotStatusArgs {
    /// Directory holding the snapshot `state` file (default /metadata/ota)
    #[arg(long, default_value = "/metadata/ota")]
    pub metadata_dir: PathBuf,
}

fn state_name(value: u64) -> Option<&'static str> {
    match value {
        0 => Some("none"),
        1 => Some("initiated"),
        2 => Some("unverified"),
        3 => Some("merging"),
        4 => Some("merge-needs-reboot"),
        5 => Some("merge-completed"),
        6 => Some("merge-failed"),
        7 => Some("cancelled"),
        _ => None,
    }
}

fn read_varint(data: &[u8], mut pos: usize) -> Option<(u64, usize)> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    let start = pos;
    while pos < data.len() {
        let byte = data[pos];
        pos += 1;
        let bits = (byte & 0x7F) as u64;
        value |= bits.checked_shl(shift)?;
        shift = shift.checked_add(7)?;
        if shift > 63 {
            return None;
        }
        if byte & 0x80 == 0 {
            return Some((value, pos - start));
        }
    }
    None
}

/// Minimal protobuf TLV scan decoding only field #1 (state enum).
/// Returns None when the buffer is not a plausible encoding.
fn parse_proto_state(data: &[u8]) -> Option<&'static str> {
    if data.is_empty() {
        return None;
    }
    let mut pos = 0usize;
    while pos < data.len() {
        let (key, used) = read_varint(data, pos)?;
        pos += used;
        let field = key >> 3;
        match key & 0x07 {
            0 => {
                let (value, used) = read_varint(data, pos)?;
                pos += used;
                if field == 1 {
                    return state_name(value);
                }
            }
            1 => {
                pos = pos.checked_add(8)?;
                if pos > data.len() {
                    return None;
                }
            }
            2 => {
                let (len, used) = read_varint(data, pos)?;
                pos += used;
                let len = usize::try_from(len).ok()?;
                pos = pos.checked_add(len)?;
                if pos > data.len() {
                    return None;
                }
            }
            5 => {
                pos = pos.checked_add(4)?;
                if pos > data.len() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    None
}

/// Legacy plain-text format (libsnapshot UpdateStateFromString).
fn parse_legacy_state(data: &[u8]) -> Option<&'static str> {
    let text = std::str::from_utf8(data).ok()?;
    let text = text.trim();
    match text {
        "" | "none" => Some("none"),
        "initiated" => Some("initiated"),
        "unverified" => Some("unverified"),
        "merging" => Some("merging"),
        "merge-completed" => Some("merge-completed"),
        "merge-needs-reboot" => Some("merge-needs-reboot"),
        "merge-failed" => Some("merge-failed"),
        "cancelled" => Some("cancelled"),
        _ => None,
    }
}

fn read_update_state(metadata_dir: &std::path::Path) -> &'static str {
    let path = metadata_dir.join("state");
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("note: cannot read {} ({e}); reporting none", path.display());
            return "none";
        }
    };
    if let Some(state) = parse_proto_state(&data) {
        return state;
    }
    if let Some(state) = parse_legacy_state(&data) {
        return state;
    }
    eprintln!(
        "note: {} is neither SnapshotUpdateStatus proto nor legacy text; reporting none",
        path.display()
    );
    "none"
}

pub fn run(args: SnapshotStatusArgs) -> std::process::ExitCode {
    println!("Update state: {}", read_update_state(&args.metadata_dir));
    std::process::ExitCode::SUCCESS
}
