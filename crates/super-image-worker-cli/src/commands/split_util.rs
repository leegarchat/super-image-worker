use super_image_worker_core::{
    MultiBlockImage, SuperData, load_super_all, load_super_in_slot, open_multiblock,
};
use std::path::Path;

/// Parse `--slot` (metadata slot selector): `all` (default) loads every
/// valid slot, otherwise one slot by index or by conventional letter
/// alias (`0|a|A|_a|_A` -> slot 0, `1|b|B|_b|_B` -> slot 1, plain
/// `2..=7` for the rest), validated against the geometry at load time.
/// This selects *which metadata copy* is used; the `_a`/`_b` name
/// letters are a separate `--suffix` filter.
pub fn parse_slot_opt(s: &str) -> std::result::Result<Option<u64>, String> {
    match s.trim() {
        "all" | "ALL" => Ok(None),
        "0" | "a" | "A" | "_a" | "_A" => Ok(Some(0)),
        "1" | "b" | "B" | "_b" | "_B" => Ok(Some(1)),
        other => match other.parse::<u64>() {
            Ok(n) if n <= 7 => Ok(Some(n)),
            _ => Err(format!(
                "unknown slot: {other} (use 0|a, 1|b, 2..7 or all)"
            )),
        },
    }
}

/// Parse `--suffix` (partition name letters): `a`, `b` or `all`
/// (default). Filters by the trailing `_a`/`_b` in partition names;
/// slotless partitions always match. Never selects a metadata slot.
pub fn parse_suffix_opt(s: &str) -> std::result::Result<Option<&str>, String> {
    match s {
        "a" | "A" => Ok(Some("a")),
        "b" | "B" => Ok(Some("b")),
        "all" | "ALL" => Ok(None),
        other => Err(format!("unknown suffix: {other} (use a, b, all)")),
    }
}

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

/// Metadata-slot loader: `Some(index)` opens that slot only,
/// `None` (`all`) opens every valid slot. Returns the loaded slot(s)
/// in index order. Never looks at name suffixes.
pub fn load_for_slot_filter(
    image: &Path,
    slot: Option<u64>,
) -> std::result::Result<Vec<SuperData>, String> {
    match slot {
        Some(idx) => {
            load_super_in_slot(image, idx).map(|d| vec![d]).map_err(|e| format!("{e}"))
        }
        None => load_super_all(image).map_err(|e| format!("{e}")),
    }
}

/// Pick the loaded slot holding an existing partition for mutating
/// commands (resize/remove/rename). An explicit `--slot` index wins;
/// otherwise the name (plus `--suffix`) must resolve in exactly one
/// slot — zero matches is `not found`, several is an ambiguity error
/// naming the slot indices so the user can pass `--slot <index>`.
/// Returns the position inside `datas`.
pub fn resolve_write_slot(
    datas: &[SuperData],
    name: &str,
    suffix: Option<&str>,
    slot: Option<u64>,
) -> std::result::Result<usize, String> {
    if let Some(idx) = slot {
        let pos = datas
            .iter()
            .position(|d| d.metadata_slot == idx)
            .ok_or_else(|| format!("metadata slot {idx} not loaded"))?;
        // Validate the partition exists there (honours the suffix
        // filter for base names).
        let data = &datas[pos];
        match data.resolve_partition(name, suffix) {
            Ok(_) => Ok(pos),
            Err(e) => Err(format!("slot {idx}: {e}")),
        }
    } else {
        let mut hits: Vec<usize> = Vec::new();
        for (pos, data) in datas.iter().enumerate() {
            // Exact names resolve regardless of suffix; base names go
            // through the suffix filter.
            let matched = if data.partitions.iter().any(|p| p.name == name) {
                match suffix {
                    Some(sfx) => SuperData::find_suffix(name).is_none_or(|s| s == sfx),
                    None => true,
                }
            } else {
                data.resolve_partition(name, suffix).is_ok()
            };
            if matched {
                hits.push(pos);
            }
        }
        match hits.len() {
            0 => Err(format!("partition '{name}' not found")),
            1 => Ok(hits[0]),
            _ => {
                let slots: Vec<u64> = hits.iter().map(|p| datas[*p].metadata_slot).collect();
                Err(format!(
                    "partition '{name}' exists in several metadata slots {slots:?}; pass -s/--slot <index> to select one"
                ))
            }
        }
    }
}

/// Pick the loaded slot for creating a brand-new partition
/// (add/create). The name must not exist yet (checked by callers via
/// the suffix filter); an explicit `--slot` wins, otherwise exactly
/// one loaded slot must exist.
pub fn resolve_new_slot(
    datas: &[SuperData],
    slot: Option<u64>,
) -> std::result::Result<usize, String> {
    if let Some(idx) = slot {
        return datas
            .iter()
            .position(|d| d.metadata_slot == idx)
            .ok_or_else(|| format!("metadata slot {idx} not loaded"));
    }
    if datas.len() == 1 {
        return Ok(0);
    }
    let slots: Vec<u64> = datas.iter().map(|d| d.metadata_slot).collect();
    Err(format!(
        "several metadata slots present {slots:?}; pass -s/--slot <index> to select the table to grow"
    ))
}

/// Find a partition across loaded metadata slots.
/// Exact name match wins; otherwise base-name + suffix-filter
/// resolution per slot. Returns `(slot_position_in_datas,
/// partition_index)`. Ambiguous base names across slots (e.g.
/// `system` with `--slot all`) produce a disambiguation error.
pub fn find_partition_across(
    datas: &[SuperData],
    name: &str,
    suffix: Option<&str>,
) -> std::result::Result<(usize, usize), String> {
    // 1) Exact match in any slot.
    for (di, data) in datas.iter().enumerate() {
        if let Some(pi) = data.partitions.iter().position(|p| p.name == name) {
            // Honour an explicit suffix filter on exact matches too:
            // `system_a` with `--suffix b` should not match.
            if let Some(sfx) = suffix {
                let s = SuperData::find_suffix(name);
                if s.is_some() && s != Some(sfx) {
                    continue;
                }
            }
            return Ok((di, pi));
        }
    }
    // 2) Base-name resolution per slot (handles Virtual A/B slotless).
    // An ambiguity *inside* one slot is a real ambiguity, not a miss:
    // remember it instead of swallowing it as "not found".
    let mut candidates: Vec<(usize, usize, String)> = Vec::new();
    let mut inner_ambiguous: Option<String> = None;
    for (di, data) in datas.iter().enumerate() {
        match data.resolve_partition(name, suffix) {
            Ok(pi) => {
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
            Err(super_image_worker_core::Error::Invalid(msg)) => {
                // Ambiguity inside one slot (vs plain absence): keep the
                // first such report instead of degrading to "not found".
                if inner_ambiguous.is_none() {
                    inner_ambiguous = Some(format!("invalid: {msg}"));
                }
            }
            Err(_) => {}
        }
    }
    match candidates.len() {
        0 => Err(inner_ambiguous
            .unwrap_or_else(|| format!("partition '{name}' not found"))),
        1 => {
            let (di, pi, _) = candidates.into_iter().next().unwrap_or((0, 0, String::new()));
            Ok((di, pi))
        }
        _ => {
            let names: Vec<&str> =
                candidates.iter().map(|(_, _, n)| n.as_str()).collect();
            Err(format!(
                "ambiguous name '{name}', matches: {names:?}. Use full name or --suffix"
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
