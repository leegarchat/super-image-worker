use super::source::Image;
use crate::error::{Error, Result};
use crate::format::{BlockDevice, Extent, LP_SECTOR_SIZE, LP_TARGET_TYPE_LINEAR};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Split / Retrofit super container.
///
/// On retrofit devices the logical `super` spans several physical block
/// devices (e.g. `system` + `vendor`). LP extents reference them via
/// `extent.target_source` (index into the `block_devices` table).
/// This type maps each `target_source` index to its backing file:
/// index 0 is the primary image, the rest are bound via `--device`.
#[derive(Debug)]
pub struct MultiBlockImage {
    primary: Image,
    extra: HashMap<u32, Image>,
}

impl MultiBlockImage {
    pub fn open_primary(path: &Path) -> Result<Self> {
        Ok(Self {
            primary: Image::open(path)?,
            extra: HashMap::new(),
        })
    }

    /// Bind a secondary file to a `target_source` index.
    pub fn bind(&mut self, index: u32, path: &Path) -> Result<()> {
        if index == 0 {
            return Err(Error::Invalid(
                "device index 0 is the primary image; do not bind it with --device".into(),
            ));
        }
        if self.extra.contains_key(&index) {
            return Err(Error::Invalid(format!(
                "block device index {index} bound twice"
            )));
        }
        self.extra.insert(index, Image::open(path)?);
        Ok(())
    }

    pub fn is_bound(&self, index: u32) -> bool {
        index == 0 || self.extra.contains_key(&index)
    }

    /// Read `buf.len()` bytes at `offset` from the file backing `source`.
    pub fn read_at(&mut self, source: u32, offset: u64, buf: &mut [u8]) -> Result<()> {
        if source == 0 {
            return self.primary.read_at(offset, buf);
        }
        match self.extra.get_mut(&source) {
            Some(img) => img.read_at(offset, buf),
            None => Err(Error::NotFound(format!(
                "block device index {source} is not bound; pass --device <name>=<path>"
            ))),
        }
    }

    /// Logical size of one backing file (for diagnostics).
    pub fn backing_size(&self, source: u32) -> Option<u64> {
        if source == 0 {
            Some(self.primary.size())
        } else {
            self.extra.get(&source).map(|i| i.size())
        }
    }

    /// Ensure every linear extent's `target_source` is bound.
    /// Returns a descriptive error naming the missing device.
    pub fn check_extents_bound(&self, extents: &[Extent], devices: &[BlockDevice]) -> Result<()> {
        for e in extents {
            if e.target_type != LP_TARGET_TYPE_LINEAR {
                continue;
            }
            if !self.is_bound(e.target_source) {
                let name = devices
                    .get(e.target_source as usize)
                    .map(|d| d.partition_name.as_str())
                    .unwrap_or("?");
                return Err(Error::NotFound(format!(
                    "partition data lives on block device '{name}' (index {}), \
                     but it is not bound; pass --device {name}=<path>",
                    e.target_source,
                    name = name
                )));
            }
        }
        Ok(())
    }
}

/// Resolve CLI `--device` specs against the `block_devices` table.
///
/// Accepted forms:
/// - `name=path` — explicit binding to the device named `name`;
/// - `path` — auto-match: the file stem must equal the device's
///   `partition_name`, either exactly (`vendor.img`), minus a `super_`/`super-`
///   prefix (`super_vendor.img`), or as a `_name` suffix (`rt_super_vendor.img`,
///   `super_vendor_a.img` -> device `vendor_a`). Ambiguous stems are rejected.
///
/// Returns index -> file path. Index 0 (primary) is never included; if a spec
/// resolves to the primary device it must point at the primary file itself
/// and is silently dropped.
pub fn resolve_device_bindings(
    devices: &[BlockDevice],
    specs: &[String],
    primary_path: &Path,
) -> Result<HashMap<u32, PathBuf>> {
    let mut out: HashMap<u32, PathBuf> = HashMap::new();
    for spec in specs {
        let (name_opt, path_str) = match spec.split_once('=') {
            Some((n, p)) => (Some(n.trim()), p.trim()),
            None => (None, spec.trim()),
        };
        if path_str.is_empty() {
            return Err(Error::Invalid(format!("invalid --device spec '{spec}'")));
        }
        let path = PathBuf::from(path_str);
        let idx = match name_opt {
            Some(name) => {
                if name.is_empty() {
                    return Err(Error::Invalid(format!("invalid --device spec '{spec}'")));
                }
                devices
                    .iter()
                    .position(|d| d.partition_name == name)
                    .ok_or_else(|| {
                        let known: Vec<&str> =
                            devices.iter().map(|d| d.partition_name.as_str()).collect();
                        Error::NotFound(format!(
                            "block device '{name}' not found in metadata (known: {known:?})"
                        ))
                    })? as u32
            }
            None => auto_match_device(devices, &path).ok_or_else(|| {
                let stem = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let known: Vec<&str> = devices.iter().map(|d| d.partition_name.as_str()).collect();
                Error::NotFound(format!(
                    "cannot auto-match '{stem}' to any block device (known: {known:?}); \
                     use --device <name>=<path>"
                ))
            })?,
        };
        if idx == 0 {
            // Primary device: only accept if it is the primary file itself.
            if same_file_hint(&path, primary_path) {
                continue;
            }
            return Err(Error::Invalid(
                "device index 0 is the primary image; pass it as the main IMAGE argument, not --device".into(),
            ));
        }
        if out.contains_key(&idx) {
            return Err(Error::Invalid(format!(
                "block device index {idx} bound twice"
            )));
        }
        out.insert(idx, path);
    }
    Ok(out)
}

fn auto_match_device(devices: &[BlockDevice], path: &Path) -> Option<u32> {
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let stripped = stem
        .strip_prefix("super_")
        .or_else(|| stem.strip_prefix("super-"))
        .unwrap_or(stem.as_str());
    let mut hits: Vec<u32> = Vec::new();
    for (i, d) in devices.iter().enumerate() {
        let dn = d.partition_name.as_str();
        if dn == stem || dn == stripped || stem.ends_with(format!("_{dn}").as_str()) {
            hits.push(i as u32);
        }
    }
    // Case-insensitive fallback if nothing matched exactly.
    if hits.is_empty() {
        let lower = stem.to_lowercase();
        let lower_stripped = stripped.to_lowercase();
        for (i, d) in devices.iter().enumerate() {
            let dn = d.partition_name.to_lowercase();
            if dn == lower || dn == lower_stripped {
                hits.push(i as u32);
            }
        }
    }
    match hits.len() {
        1 => hits.first().copied(),
        _ => None,
    }
}

fn same_file_hint(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    // Compare canonicalized paths when possible; fall back to file names.
    if let (Ok(ca), Ok(cb)) = (a.canonicalize(), b.canonicalize()) {
        return ca == cb;
    }
    a.file_name() == b.file_name()
}

/// Open the primary image plus all `--device` secondary files.
pub fn open_multiblock(
    primary_path: &Path,
    specs: &[String],
    devices: &[BlockDevice],
) -> Result<MultiBlockImage> {
    let bindings = resolve_device_bindings(devices, specs, primary_path)?;
    let mut mb = MultiBlockImage::open_primary(primary_path)?;
    let mut ordered: Vec<(u32, PathBuf)> = bindings.into_iter().collect();
    ordered.sort_by_key(|(i, _)| *i);
    for (idx, path) in &ordered {
        mb.bind(*idx, path)?;
    }
    Ok(mb)
}

/// Streaming reader over a partition's extent chain backed by split files.
/// Mirrors `ExtentReader` but routes each extent through the owning device.
pub struct SplitExtentReader<'a> {
    mb: &'a mut MultiBlockImage,
    extents: Vec<Extent>,
    current_extent: usize,
    current_offset: u64,
    total_size: u64,
    position: u64,
}

impl<'a> SplitExtentReader<'a> {
    pub fn new(mb: &'a mut MultiBlockImage, extents: Vec<Extent>) -> Self {
        let mut total_size: u64 = 0;
        for e in &extents {
            total_size = total_size.saturating_add(e.num_sectors.saturating_mul(LP_SECTOR_SIZE));
        }
        Self {
            mb,
            extents,
            current_extent: 0,
            current_offset: 0,
            total_size,
            position: 0,
        }
    }

    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() || self.position >= self.total_size {
            return Ok(0);
        }
        let mut written = 0;
        while written < buf.len() && self.current_extent < self.extents.len() {
            let extent = match self.extents.get(self.current_extent) {
                Some(e) => e.clone(),
                None => break,
            };
            let extent_size = extent.num_sectors.saturating_mul(LP_SECTOR_SIZE);
            if extent_size == 0 {
                self.current_extent = self.current_extent.saturating_add(1);
                self.current_offset = 0;
                continue;
            }
            let remaining_in_extent = extent_size.saturating_sub(self.current_offset);
            if remaining_in_extent == 0 {
                self.current_extent = self.current_extent.saturating_add(1);
                self.current_offset = 0;
                continue;
            }
            let remaining_in_buf = (buf.len() - written) as u64;
            let to_read = remaining_in_extent.min(remaining_in_buf) as usize;
            if to_read == 0 {
                break;
            }
            let dest = buf
                .get_mut(written..written + to_read)
                .ok_or_else(|| Error::Invalid("split reader buffer range error".into()))?;
            match extent.target_type {
                LP_TARGET_TYPE_LINEAR => {
                    let phys_offset = extent
                        .target_data
                        .checked_mul(LP_SECTOR_SIZE)
                        .and_then(|v| v.checked_add(self.current_offset))
                        .ok_or_else(|| Error::Invalid("physical offset overflow".into()))?;
                    self.mb.read_at(extent.target_source, phys_offset, dest)?;
                }
                crate::format::LP_TARGET_TYPE_ZERO => {
                    dest.fill(0);
                }
                _ => {
                    return Err(Error::Invalid(format!(
                        "unsupported extent type: {}",
                        extent.target_type
                    )));
                }
            }
            written += to_read;
            self.current_offset = self.current_offset.saturating_add(to_read as u64);
            self.position = self.position.saturating_add(to_read as u64);
            if self.current_offset >= extent_size {
                self.current_extent = self.current_extent.saturating_add(1);
                self.current_offset = 0;
            }
        }
        Ok(written)
    }
}

/// Extract a partition from split files (O(1) RAM, 1 MiB stream buffer).
pub fn extract_partition_split(
    mb: &mut MultiBlockImage,
    extents: &[Extent],
    output: &mut dyn std::io::Write,
) -> Result<u64> {
    let mut reader = SplitExtentReader::new(mb, extents.to_vec());
    let total = reader.total_size();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut written = 0u64;
    while written < total {
        let remaining = total.saturating_sub(written) as usize;
        let to_read = remaining.min(buf.len());
        if to_read == 0 {
            break;
        }
        let chunk = buf
            .get_mut(..to_read)
            .ok_or_else(|| Error::Invalid("split extract buffer range error".into()))?;
        let n = reader.read(chunk)?;
        if n == 0 {
            break;
        }
        let out_slice = buf
            .get(..n)
            .ok_or_else(|| Error::Invalid("split extract output range error".into()))?;
        output.write_all(out_slice).map_err(Error::Io)?;
        written = written.saturating_add(n as u64);
    }
    Ok(written)
}
