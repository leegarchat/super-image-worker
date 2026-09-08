use super_image_worker_core::{
    MultiBlockImage, SuperData, load_super_all, load_super_in_slot, open_multiblock,
    suffix_to_slot,
};
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

/// Slot-aware loader: `a|b` opens that metadata slot only
/// (0 <-> `_a`, 1 <-> `_b`); `all` opens every valid slot.
/// Returns the loaded slot(s) in index order.
pub fn load_for_slot_filter(
    image: &Path,
    slot: Option<&str>,
) -> std::result::Result<Vec<SuperData>, String> {
    match slot {
        Some(sfx) => {
            let idx = suffix_to_slot(sfx)
                .ok_or_else(|| format!("unknown slot: {sfx} (use a, b, all)"))?;
            load_super_in_slot(image, idx).map(|d| vec![d]).map_err(|e| format!("{e}"))
        }
        None => load_super_all(image).map_err(|e| format!("{e}")),
    }
}

/// Slot-aware loader for mutating commands (add/resize/remove/rename/
/// create): picks the metadata slot matching the partition's `_a`/`_b`
/// suffix, or an explicit `--slot` when given. Without any hint falls
/// back to the first valid slot (legacy behaviour for slotless images).
pub fn load_for_write(
    image: &Path,
    name_hint: &str,
    slot: Option<&str>,
) -> std::result::Result<SuperData, String> {
    if let Some(sfx) = slot {
        let idx = suffix_to_slot(sfx)
            .ok_or_else(|| format!("unknown slot: {sfx} (use a, b, all)"))?;
        return load_super_in_slot(image, idx).map_err(|e| format!("{e}"));
    }
    if let Some(sfx) = SuperData::find_suffix(name_hint)
        && let Some(idx) = suffix_to_slot(sfx)
        && let Ok(d) = load_super_in_slot(image, idx)
    {
        // Prefer the suffix-matching slot; fall back to first valid
        // when that slot is empty/invalid (e.g. single-slot images).
        return Ok(d);
    }
    super_image_worker_core::load_super(image).map_err(|e| format!("{e}"))
}

/// Find a partition across loaded metadata slots.
/// Exact name match wins; otherwise base-name + slot-filter resolution
/// per slot. Returns `(slot_position_in_datas, partition_index)`.
/// Ambiguous base names across slots (e.g. `system` with `all`)
/// produce a disambiguation error listing every match.
pub fn find_partition_across(
    datas: &[SuperData],
    name: &str,
    slot: Option<&str>,
) -> std::result::Result<(usize, usize), String> {
    // 1) Exact match in any slot (covers `system_b` with `--slot all`).
    for (di, data) in datas.iter().enumerate() {
        if let Some(pi) = data.partitions.iter().position(|p| p.name == name) {
            // Honour an explicit slot filter on exact matches too:
            // `system_a` with `-s b` should not match.
            if let Some(sfx) = slot {
                let suffix = SuperData::find_suffix(name);
                if suffix.is_some() && suffix != Some(sfx) {
                    continue;
                }
            }
            return Ok((di, pi));
        }
    }
    // 2) Base-name resolution per slot (handles Virtual A/B slotless).
    let mut candidates: Vec<(usize, usize, String)> = Vec::new();
    for (di, data) in datas.iter().enumerate() {
        if let Ok(pi) = data.resolve_partition(name, slot) {
            let pname = data
                .partitions
                .get(pi)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            // Skip exact matches already handled (avoid duplicates).
            if data.partitions.iter().any(|p| p.name == name) {
                continue;
            }
            candidates.push((di, pi, pname));
        }
    }
    match candidates.len() {
        0 => Err(format!("partition '{name}' not found")),
        1 => {
            let (di, pi, _) = candidates.into_iter().next().unwrap_or((0, 0, String::new()));
            Ok((di, pi))
        }
        _ => {
            let names: Vec<&str> =
                candidates.iter().map(|(_, _, n)| n.as_str()).collect();
            Err(format!(
                "ambiguous name '{name}', matches: {names:?}. Use full name or --slot"
            ))
        }
    }
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
