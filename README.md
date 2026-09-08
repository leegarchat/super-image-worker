# super-image-worker

> High-performance standalone Rust utility for deep inspection, modification, and generation of Android `super` partition images: LP metadata (`liblp` dynamic partitions), raw and Android Sparse containers, single and split/retrofit multi-device layouts, plus a built-in `lpmake` analog.

The utility is completely standalone: for all offline work it requires no superuser privileges, no kernel mounts, and no external host binaries (`lpmake`, `lpunpack`, `simg2img`). Only `connect`/`disconnect` (loop devices) and `map`/`unmap` (device-mapper, Android/Recovery only) need root — everything else is pure user-space file I/O. All block and metadata manipulations stream with O(1) RAM and never load whole images into memory.

---

## Key Features

* **100% Standalone User-Space**: Direct parsing, mutation, and generation of LP metadata structures in pure safe Rust. No `lpmake`/`lpunpack`/`simg2img` required.
* **Zero-Panic Policy**: No `unwrap()`, `expect()`, or panicking indexing across all production code paths in `super-image-worker-core`. All binary structures are bounds-checked and return `Result<T, Error>`.
* **Strict Memory Discipline ($O(1)$ RAM / Streaming)**: Reads, writes, extraction, and image generation stream in fixed 1–4 MiB chunks. Multi-gigabyte payloads (e.g. 2–4 GB `system.img`) never sit in RAM whole — safe on 32-bit targets and RAM disks.
* **SHA-256 Integrity + Automatic Fallback**: Geometry checksum, header checksum, and tables checksum are validated on every load. A corrupt primary geometry/metadata automatically falls back to the backup copies (`0x2000` / backup slots).
* **Slot-Safe Writes**: Metadata updates rewrite the primary AND backup copies of the loaded slot only (`Backup(slot) = 0x3000 + (slot_count + slot) * max_size`); neighbouring slots are never touched.
* **Collision-Safe `resize`**: Growth expands the last extent in place when the tail is free, otherwise allocates a new linear extent and splices it into the partition window (later partitions shifted) — never overwrites the neighbour's blocks.
* **Split / Retrofit Super**: Multi-`block_devices` layouts (`target_source` routing) across several files via repeatable `--device` (`name=path` or auto-match by file name).
* **Built-in `lpmake` analog (`make`)**: Generates fresh images from scratch with arbitrary target slots (`0|1|a|b|all` — stock `lpmake` only writes slot 0), single or retrofit outputs, raw or Sparse containers, multi-device spanning extents when one device alone is too small, per-partition device pinning (`group@device`), per-device output overrides (`--output-map`, files or raw block nodes), and direct-to-block builds (the `--device` size must equal the probed block size; outputs are zero-filled first).
* **Slot vs Suffix Separation**: `-s/--slot` (`0|a`, `1|b`, …, `all`) picks which LP metadata copy is used; `--suffix` (`a|b|all`, long flag only) filters partition name letters inside it. A metadata slot holding both `_a` and `_b` entries reads with `--slot 0 --suffix b`.
* **Raw Block Devices Everywhere**: Every command accepts `/dev/block/by-name/*` nodes directly as inputs (size probed via `SEEK_END`, since block metadata reports 0).
* **Legacy Single-Slot Supers**: Slotless layouts (`system`, `vendor`, … without suffix letters, one metadata slot) are fully supported for generation and all read/write commands.
* **Multithreaded Extraction**: Parallel `extract` powered by `rayon`, each thread with an isolated image handle (no `Seek` races).
* **Script-First CLI**: `--slot`/`--suffix` everywhere, `human`/`json`/`tsv`/`env` output formats, `--get` scalar extraction without trailing newlines, streaming `read` with `--skip`/`--size` and graceful `BrokenPipe` handling.

---

## Repository Architecture

```text
Cargo.toml                  Workspace root (members + centralized [profile.release]: LTO fat, abort, strip)
build.sh                    Static musl multi-arch builder (x86_64/x86/aarch64/armv7) via cargo or cross
dist/                       Prebuilt static binaries (super-image-worker-linux-*)
crates/super-image-worker-core/   No-panic parsing/allocation/serialization library:
  src/format/mod.rs         LP constants, table structs, AOSP slot-offset helpers
  src/reader/source.rs      Raw / Android Sparse images, O(log N) chunk lookup
  src/reader/parser.rs      SHA-256-validated geometry + metadata loader with fallbacks
  src/reader/extent_stream.rs   Single-file streaming extent reader + extractor
  src/reader/multiblock.rs  Split/retrofit container (target_source routing, --device binding)
  src/reader/mod.rs         SuperData model, slot/group/power helpers, load_super
  src/writer.rs             Allocator (alignment + alignment_offset), slot renderer, payload streaming
  src/sparse.rs             Sized file creation + streaming raw->sparse encoder
crates/super-image-worker-cli/    Binary crate (clap derive):
  src/main.rs               Command dispatch + global manual
  src/commands/             info, extract, add, resize, remove, rename, create,
                            read, connect, map, make (+ split_util shared helper)
  src/output/               human_size + human/json/tsv/env/get renderers
```

---

## Technical Highlights & Fixes Over Common Implementations

`super-image-worker` fixes multiple pervasive flaws found in naive LP tooling:

| Spec / Format Quirk | Common Implementation Flaw | `super-image-worker` Implementation |
|---|---|---|
| **Backup slot layout** | Backup written at `offset + max_size`, clobbering primary of slot B. | AOSP rule `Backup(slot) = 0x3000 + (slot_count + slot) * max_size`; slot isolation verified by hash. |
| **`resize` growth** | `num_sectors` of the last extent is blindly enlarged, overwriting the next physical partition. | In-place expansion only on proven-free tail (`is_range_free`); otherwise a new extent is spliced in with index shifting. |
| **Payload RAM** | `fs::read(payload)` loads whole multi-GB images → OOM/SIGKILL. | 1 MiB `copy_payload_stream`/`copy_payload_segment`; no `fs::read` of binary data anywhere. |
| **Allocator bounds** | Free sector past end-of-disk is returned on full images. | Hard `device.size / 512` limit → `not enough free space on block device`. |
| **`alignment_offset`** | Honoured in metadata but ignored during allocation (wrong erase-block phase on exotic eMMC/UFS). | `(sector·512) % alignment == offset % alignment` in `align_up`; exact for sector-granular geometries. |
| **Fragmented `connect`** | Loop device sized from the first extent only, capturing foreign blocks. | Multi-extent partitions refused with a `map` hint; single-linear-extent enforced. |
| **`map` gating** | Allowed/blocked on the wrong OS (dm-linear needs a loop base on Linux). | Strictly Android/Recovery (`/system/build.prop`, `/sbin/recovery`, `/system/bin/getprop`, `/dev/block/mapper`); Linux hosts get a `connect` pointer. |
| **Sparse reads** | Linear chunk scan — O(N) per read, stalls on 10k+ chunks. | Binary search by logical range — O(log N). |
| **Checksums** | Geometry/header/tables hashes never verified; corrupt copies trusted. | All three SHA-256 validated; primary→backup fallback on mismatch. |
| **Single-device `make`** | Payload larger than any one device fails even when total space suffices. | `plan_spanning_allocation` carves a per-device extent chain (`largest_free_run`-capped). |
| **Unbound writes** | Payload allocated on a device whose file was never passed. | `add` allocates only on bound devices (`find_free_sectors_any_in`); fast failure with `--device` hint. |
| **Group overcommit** | `maximum_size` ignored on `add`/`create`/`make`. | `used + needed ≤ max` enforced everywhere unless `--force`. |
| **Slot/suffix conflation** | One flag means both the metadata copy and the name letters, so `_b` rows living in metadata slot 0 are unreachable. | `-s/--slot` selects the metadata copy (index or `a`/`b` alias), `--suffix` filters the letters; `--slot 0 --suffix b` reaches them. |
| **Block-device inputs** | `metadata().len()` is 0 for block nodes, so every tool fails on `/dev/block/...`. | Size probed via `SEEK_END`; live `super` partitions read directly. |
| **`map` on file images** | File path passed straight to `DM_TABLE_LOAD` → `ENODEV`. | Regular files are auto-backed by a whole-file loop; block nodes pass through canonicalized. |
| **Loop nodes on Android** | Hardcoded `/dev/loopN` missing on devices exposing `/dev/block/loopN`. | Both prefixes tried; free-node fallback when `GET_FREE` points past pre-created nodes. |

---

## Hard Integrity Caps

To prevent denial-of-service, unbounded memory growth, and arithmetic corruption on hostile inputs, strict caps are enforced:

* **Maximum `metadata_max_size`**: 16 MiB; **metadata slots**: 1–8; **tables**: 65,536 entries per table, 16 block devices max.
* **Maximum `header_size`**: 1 MiB; **`tables_size`**: 64 MiB; **sparse chunks**: 1,000,000; **sparse block size**: 512 B–1 MiB.
* **Streaming buffers**: 1 MiB payload I/O, 4 MiB sparse-encode windows, 8 MiB max RAW run — peak RSS stays flat regardless of image size.
* **Arithmetic**: every sector/byte/offset computation uses `checked_*`/`saturating_*`; table entry reads are range-validated against the image size.
* **Sparse output**: RAW chunk totals capped at `u32::MAX`; `DONT_CARE` runs split accordingly.

---

## Building & Compilation

**Prerequisites**:
* Rust toolchain (stable, edition 2024).
* Cargo package manager.
* For static builds: musl toolchains or `cross` (see `build.sh --help` for per-distro install hints).

```bash
# Clone the repository
git clone https://github.com/leegarchat/super-image-worker.git
cd super-image-worker

# Fast local build
cargo build --release
# -> target/release/super-image-worker (~1.2 MB, LTO + stripped)

# Static multi-arch binaries into dist/
./build.sh --cargo --arch x64      # x86_64-unknown-linux-musl
./build.sh --arch all              # x86_64, x86, aarch64, armv7

# Checks
cargo clippy --all-targets -- -D warnings
cargo test
```

---

## CLI Reference

Global conventions: `-s/--slot <0|a|1|b|…|all>` picks the LP metadata copy (default `all`, like `lpdump -a`); `--suffix <a|b|all>` (long flag only) filters partition name letters inside it. Base-name resolution: `-p system --suffix a` matches `system_a`; slotless Virtual A/B partitions always match. Every command accepts raw images, Android-sparse images, and raw block devices (`/dev/block/by-name/super`) as inputs. `--device` (repeatable, `info`/`extract`/`read`/`add`/`resize`/`create`) binds retrofit secondaries as `name=path` or auto-matched bare paths (`vendor.img`, `super_vendor.img`, `*_vendor.img`). Sizes accept `K`/`M`/`G` (any case) and `s` sectors in `resize`. Exit codes: `0` success, `1` failure, `2` bad `--get` key (info). Every subcommand documents itself in full via `super-image-worker <command> --help`.

### 1. `info` — Inspector (read-only, no root)

```bash
super-image-worker info super.img                              # Everything, human tables
super-image-worker info super.img --suffix a -f tsv -H -c name,size_bytes | awk '{print $1}'
super-image-worker info /dev/block/by-name/super -s 1          # Metadata slot 1 of the live partition
super-image-worker info super.img -f json | jq '.partitions[] | .name'
eval $(super-image-worker info super.img -f env); echo $SUPER_PART_SYSTEM_A_SIZE
super-image-worker info super.img --get partitions.system_a.size
super-image-worker info super.img --get partitions.system_a.extent.0.phys_offset_hex
super-image-worker info super.img --get available_suffixes
super-image-worker info sys.img --device vendor=vend.img       # Retrofit pair + binding report
```

Formats: `human` (default), `json` (pretty, jq-safe), `tsv` (`-H` drops header, `-c` picks from 17 columns), `env` (`SUPER_*`). Section flags `-i/-p/-e/-g/-d/-m/-a`, `-b` for raw bytes in TSV, `--list-keys` catalogues every `--get` path.

### 2. `extract` — lpunpack analog (read-only, no root)

```bash
super-image-worker extract super.img -o ./out                  # Unpack everything (rayon-parallel)
super-image-worker extract super.img -o ./out -p system_a
super-image-worker extract super.img -o ./out --suffix a
super-image-worker extract sys.img -o ./out --device vendor=vend.img
super-image-worker extract super.img --dry-run                 # Names + sizes only
super-image-worker extract super.img -o ./out --force          # Overwrite outputs
```

Creates `<partition_name>.img` per partition (empty files for extent-less ones), skips existing outputs unless `--force`, deletes failed outputs, still exits `1` if anything failed.

### 3. `read` — Streaming stdout (read-only, no root)

```bash
super-image-worker read super.img -p system --suffix a | file -   # (-s = size, --slot/--suffix long-only here)
super-image-worker read super.img -p vendor_a | sha256sum
super-image-worker read super.img -p vendor_a --skip 1M --size 10M | xxd | head   # BrokenPipe-safe
super-image-worker read sys.img -p vendor_a --device vendor=vend.img > vendor.img
```

Follows the full multi-extent/multi-device chain with O(1) RAM; ranges clamp at end-of-partition.

### 4. `make` — lpmake analog (writes raw images, no root)

```bash
super-image-worker make -o super.img --device super:4G --group default:4G \
    --partition system_a:readonly:default:system.img
super-image-worker make -o out/super --retrofit --device system:2G --device vendor:1G \
    --group google_dynamic_partitions_a:3G \
    --partition system_a:readonly:google_dynamic_partitions_a:sys.img
super-image-worker make -o super.img --sparse --slot all --device super:4G \
    --group g:4G --partition system_a:readonly:g:a.img --partition system_b:readonly:g:b.img
super-image-worker make -o super.img --device super:4G --group g:4G \
    --partition sys:readonly:g:sys.img --dry-run
super-image-worker make -o /dev/block/by-name/super --device super:9126805504 \
    --group g:9124708352 --partition system_a:none:g:system_a.img   # Straight into the block node
super-image-worker make -o s/super --retrofit --device super:8G --device cust:2G \
    --output-map super=/dev/block/by-name/super --output-map cust=/dev/block/by-name/cust \
    --group g:9G --partition vendor_a:none:g@cust:vendor_a.img      # Split into blocks, pinned
```

Specs: `--device name:size[:alignment[:alignment_offset]]`, `--group name:max_size`, `--partition name:attrs:group[:payload]` (`readonly,slot_suffixed,updated,disabled,none`; no payload = extent-less placeholder), `--partition name:attrs:group@device:payload` pins one partition to a single split device. Slots `0|1|a|b|all` (default `0`, `a`/`b` alias `0`/`1`); metadata (geometry + slots) lives in the first device file, secondaries carry data only; oversized payloads span devices; `--sparse` emits RAW + DONT_CARE containers (staging `*.raw-tmp` removed); every build self-verifies via reload. Outputs (`-o`, `--output-map name=path`) may be regular files or raw block devices: for blocks the spec size must equal the probed block size exactly (checked in `--dry-run` too), the node is zero-filled before writing, and `--sparse` to a block is refused.

### 5. `add` — Append partition + payload (raw only, no root)

```bash
super-image-worker add super.img -n my_part_a -p payload.img
super-image-worker add super.img -n custom -p data.bin -g my_group
super-image-worker add super.img -n test_a -p file.txt --attrs readonly,slot_suffixed
super-image-worker add super.img -n big_a -p big.img --force    # Over group maximum_size
super-image-worker add sys.img -n extra_a -p extra.img --device vendor=vend.img
```

Group auto-selection (`qti_* > google_* > samsung_*/sec_* > mtk_* > largest`) with `maximum_size` enforcement; allocation restricted to bound devices.

### 6. `resize` — Collision-safe resize (raw only, no root)

```bash
super-image-worker resize super.img system_a 2G
super-image-worker resize super.img system --suffix a 900M
super-image-worker resize super.img vendor_a 512M --allow-shrink
super-image-worker resize super.img system_a 4G --force
super-image-worker resize super.img system_a 1G --dry-run
```

Growth expands in place or splices a new extent; shrink truncates/drops trailing extents (0 = drop all).

### 7. `remove` — Delete partition or group (raw only, no root)

```bash
super-image-worker remove super.img my_partition
super-image-worker remove super.img system --suffix a
super-image-worker remove super.img my_group --group
super-image-worker remove super.img my_group --group --force   # Cascade: partitions + extents
super-image-worker remove super.img test_a --dry-run
```

### 8. `rename` — Rename entry (raw only, no root)

```bash
super-image-worker rename super.img old_name new_name
super-image-worker rename super.img old_group new_group --group
super-image-worker rename super.img test_a test_b --dry-run
```

Name field only (≤36 bytes, must stay unique); checksums refreshed. `--slot` picks the metadata copy when the name exists in several slots.

### 9. `create` — Metadata-only partition (raw only, no root)

```bash
super-image-worker create super.img -n new_part -g qti_dynamic_partitions_a
super-image-worker create super.img -n new_part -g new_group --group-size 4G --size 1M
super-image-worker create super.img -n empty_b -g default --size 0
super-image-worker create super.img -n big_a -g qti_dynamic_partitions_a --size 2G --force
```

Like `add` without payload bytes; `--size 0` reserves an extent-less placeholder.

### 10. `connect` / `disconnect` — Loop devices (Linux + Android, root)

```bash
sudo super-image-worker connect super.img -p odm_a        # Prints /dev/loopN (or /dev/block/loopN on Android)
sudo super-image-worker connect super.img -p system --suffix a
sudo mount /dev/loop14 /mnt && sudo umount /mnt
sudo super-image-worker disconnect super.img -p odm_a
```

Single-linear-extent partitions only (fragmented ones are refused with a `map` hint).

### 11. `map` / `unmap` — Device-mapper (Android/Recovery only, root)

```bash
super-image-worker map super.img -p system_a              # -> /dev/block/mapper/system_a
super-image-worker map super.img -p vendor --suffix a --force-writable
super-image-worker unmap system_a
```

Full extent chains (linear + zero). Refuses to run on host Linux — use `connect` there.

---

## Critical Environment Notice: `TMPDIR` Exhaustion

### The Failure Mode

Copying a 9 GB `super.img` (or extracting all partitions) into a small `/tmp` (tmpfs, often 8–16 GB) silently fills the filesystem and crashes the shell session mid-write.

### Mitigation

Never stage images under `/tmp`. Use a directory on a large physical volume and export it for the session:

```bash
# Single command with an explicit workspace:
mkdir -p ~/ws/tmp && cp /images/super.row.img ~/ws/tmp/work.img

# Export for an entire shell session or pipeline script:
export TMPDIR=~/ws/tmp
```

All `read`/`extract`/`make` data paths stream with O(1) RAM, but *output files and image copies* still need real disk space — plan ~2× the image size as headroom.

---

## Performance Benchmarks & Validation

Measured on a 9.0 GiB real-device `super` image (14 partitions, 7 with data):

* `extract` (rayon, 7 data partitions): ~11 s from raw, ~34 s from Android Sparse (chunk seeks dominate).
* Raw-vs-sparse extraction: all 7 partitions bit-identical (sha256).
* `read --size 100M` vs `extract` dump: identical sha256; `--skip/--size` matches `dd` windows.
* `make` single 512 MiB (40+20+30 MiB payloads): metadata in requested slot only; all extracts match sources; `--sparse` shrinks 512 MiB → 91 MiB and re-reads identically.
* `make --retrofit` 100 + 200 MiB with 60 + 40 MiB payloads: spillover onto device 1; split `extract`/`read` match sources.
* 90 MiB payload over 60 + 60 MiB devices: automatic 2-extent span (59 + 31 MiB), round-trip intact.
* Slot isolation: after `add`, primary+backup of slot 0 change identically while slots 1–2 hashes are untouched.
* Dual-slot Pixel image (current slot `_b`): `--slot 1 --suffix b` exposes the live table; block reads match the file dump bit-identically (sha256).
* Split super across `super`+`cust`+`modem_a`+`modem_b` block nodes (`--output-map`, `@device` pins): file builds, `dd` imaging, and direct-to-block builds all read back identically under `lpdump` and `siw`.
* Legacy single-slot image (slotless `system`/`vendor`, one metadata slot): generate/read/extract/resize/rename round-trip verified.
* `cargo clippy --all-targets -- -D warnings`: clean. `cargo test`: pass.

---

## Licensing

Dual-licensed under the terms of the [MIT License](LICENSE-MIT) and the [Apache License 2.0](LICENSE-APACHE), at your option — same scheme as the sibling `image-worker` project.
