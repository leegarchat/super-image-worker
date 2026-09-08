use super_image_worker_core::{MultiBlockImage, SuperData, open_multiblock};
use std::path::Path;

/// Open primary + `--device` secondaries for split/retrofit images.
///
/// When the metadata lists a single block device, specs must be empty and a
/// plain single-file image is opened (backwards compatible). When several
/// block devices exist, secondaries are bound by name (`--device
/// vendor=path`) or auto-matched (`--device path`).
pub fn open_split(
    primary: &Path,
    specs: &[String],
    data: &SuperData,
) -> std::result::Result<MultiBlockImage, String> {
    if data.devices.len() <= 1 && specs.is_empty() {
        return MultiBlockImage::open_primary(primary).map_err(|e| format!("{e}"));
    }
    open_multiblock(primary, specs, &data.devices).map_err(|e| format!("{e}"))
}

/// Collect the extent window of a partition (bounds-checked).
pub fn partition_extents(
    data: &SuperData,
    part_idx: usize,
) -> std::result::Result<Vec<super_image_worker_core::Extent>, String> {
    let p = data
        .partitions
        .get(part_idx)
        .ok_or_else(|| "partition index out of range".to_string())?;
    let mut out = Vec::new();
    let mut idx = p.first_extent_index as u64;
    for _ in 0..p.num_extents {
        match data.extents.get(idx as usize) {
            Some(e) => out.push(e.clone()),
            None => return Err("extent index out of range (corrupt metadata)".to_string()),
        }
        idx = idx.saturating_add(1);
    }
    Ok(out)
}

/// Human-readable device binding report for `info`.
pub fn binding_report(data: &SuperData, specs: &[String], primary: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    if data.devices.len() <= 1 && specs.is_empty() {
        return lines;
    }
    match super_image_worker_core::resolve_device_bindings(&data.devices, specs, primary) {
        Ok(map) => {
            for (i, d) in data.devices.iter().enumerate() {
                let what = if i == 0 {
                    primary.display().to_string()
                } else {
                    match map.get(&(i as u32)) {
                        Some(p) => p.display().to_string(),
                        None => "(not bound)".to_string(),
                    }
                };
                lines.push(format!("  [{}] {} -> {}", i, d.partition_name, what));
            }
        }
        Err(e) => lines.push(format!("  binding error: {e}")),
    }
    lines
}
