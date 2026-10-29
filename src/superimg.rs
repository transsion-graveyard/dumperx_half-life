//! Android dynamic partition (super.img) support.
//!
//! Parses the liblp metadata directly (geometry + metadata header + tables)
//! and extracts logical partitions without invoking `lpunpack`.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const GEOMETRY_MAGIC: u32 = 0x616c_4467;
pub const METADATA_HEADER_MAGIC: u32 = 0x414c_5030;
const PARTITION_RESERVED_BYTES: u64 = 4096;
const GEOMETRY_SIZE: u64 = 4096;
const METADATA_HEADER_MIN_SIZE: u32 = 128;
const SECTOR: u64 = 512;

const LP_PARTITION_ATTR_READONLY: u32 = 1 << 0;
const LP_PARTITION_ATTR_SLOT_SUFFIXED: u32 = 1 << 1;
const LP_PARTITION_ATTR_UPDATED: u32 = 1 << 2;
const LP_PARTITION_ATTR_DISABLED: u32 = 1 << 3;

#[derive(Debug, Clone)]
pub struct Geometry {
    pub metadata_max_size: u32,
    pub metadata_slot_count: u32,
    pub logical_block_size: u32,
}

#[derive(Debug, Clone)]
pub struct Partition {
    pub name: String,
    pub attributes: u32,
    pub first_extent_index: u32,
    pub num_extents: u32,
    pub group_index: u32,
}

#[derive(Debug, Clone)]
pub struct Extent {
    pub num_sectors: u64,
    pub target_type: u32,
    pub target_data: u32,
    pub target_source: u32,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub name: String,
    pub flags: u32,
    pub maximum_size: u64,
}

#[derive(Debug, Clone)]
pub struct BlockDevice {
    pub first_logical_sector: u64,
    pub size: u64,
    pub partition_name: String,
    pub flags: u32,
}

#[derive(Debug, Clone)]
pub struct Metadata {
    pub major: u16,
    pub minor: u16,
    pub partitions: Vec<Partition>,
    pub extents: Vec<Extent>,
    pub groups: Vec<Group>,
    pub block_devices: Vec<BlockDevice>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PartitionInfo {
    pub name: String,
    pub size: u64,
    pub attributes: String,
    pub extents: Vec<ExtentInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtentInfo {
    pub num_sectors: u64,
    pub target_type: u32,
    pub target_data: u32,
    pub target_source: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotMode {
    Auto,
    A,
    B,
    All,
}

impl SlotMode {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "auto" => Ok(SlotMode::Auto),
            "a" => Ok(SlotMode::A),
            "b" => Ok(SlotMode::B),
            "all" => Ok(SlotMode::All),
            other => bail!("invalid slot mode '{other}' (expected auto|a|b|all)"),
        }
    }
}

pub fn is_super_image(data: &[u8]) -> bool {
    data.len() >= (PARTITION_RESERVED_BYTES as usize + 4)
        && u32::from_le_bytes(
            data[PARTITION_RESERVED_BYTES as usize..PARTITION_RESERVED_BYTES as usize + 4]
                .try_into()
                .unwrap(),
        ) == GEOMETRY_MAGIC
}

pub fn read_geometry(file: &mut File) -> Result<Geometry> {
    let mut candidates = Vec::new();
    // Primary geometry at 4096, backup geometry one geometry block later.
    for offset in [PARTITION_RESERVED_BYTES, PARTITION_RESERVED_BYTES + GEOMETRY_SIZE] {
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = [0u8; 52];
        if file.read_exact(&mut buf).is_err() {
            continue;
        }
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != GEOMETRY_MAGIC {
            continue;
        }
        let struct_size = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        if struct_size < 52 {
            continue;
        }
        let metadata_max_size = u32::from_le_bytes(buf[40..44].try_into().unwrap());
        let metadata_slot_count = u32::from_le_bytes(buf[44..48].try_into().unwrap());
        let logical_block_size = u32::from_le_bytes(buf[48..52].try_into().unwrap());
        if metadata_max_size == 0 || metadata_slot_count == 0 {
            continue;
        }
        candidates.push(Geometry {
            metadata_max_size,
            metadata_slot_count,
            logical_block_size,
        });
    }
    candidates
        .into_iter()
        .next()
        .context("no valid liblp geometry found (not a super image?)")
}

pub fn read_metadata(file: &mut File) -> Result<Metadata> {
    let geometry = read_geometry(file)?;
    let primary_start = PARTITION_RESERVED_BYTES + GEOMETRY_SIZE * 2;
    let backup_start = primary_start
        + geometry.metadata_max_size as u64 * geometry.metadata_slot_count as u64;

    let mut last_error = None;
    for slot in 0..geometry.metadata_slot_count as u64 {
        for start in [primary_start, backup_start] {
            let offset = start + slot * geometry.metadata_max_size as u64;
            match read_metadata_at(file, offset, &geometry) {
                Ok(meta) => return Ok(meta),
                Err(e) => {
                    if std::env::var_os("DUMPRX_DEBUG_LIBLP").is_some() {
                        eprintln!("liblp: metadata at offset {offset} failed: {e:#}");
                    }
                    last_error = Some(e.context(format!("metadata at offset {offset}")));
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no liblp metadata found")))
}

fn read_metadata_at(file: &mut File, offset: u64, geometry: &Geometry) -> Result<Metadata> {
    file.seek(SeekFrom::Start(offset))?;
    let mut header = vec![0u8; METADATA_HEADER_MIN_SIZE as usize];
    file.read_exact(&mut header)?;

    let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
    if magic != METADATA_HEADER_MAGIC {
        bail!("bad metadata magic {magic:#x}");
    }
    let major = u16::from_le_bytes(header[4..6].try_into().unwrap());
    let minor = u16::from_le_bytes(header[6..8].try_into().unwrap());
    if major != 10 {
        bail!("unsupported liblp metadata version {major}.{minor}");
    }
    let header_size = u32::from_le_bytes(header[8..12].try_into().unwrap());
    if header_size < METADATA_HEADER_MIN_SIZE {
        bail!("metadata header size {header_size} is too small");
    }
    if header_size as u64 > geometry.metadata_max_size as u64 {
        bail!("metadata header size {header_size} exceeds metadata slot size");
    }
    if header_size as usize > header.len() {
        header.resize(header_size as usize, 0);
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut header)?;
    }

    let tables_size = u32::from_le_bytes(header[44..48].try_into().unwrap());
    let tables_start = offset + header_size as u64;
    file.seek(SeekFrom::Start(tables_start))?;
    let mut tables = vec![0u8; tables_size as usize];
    file.read_exact(&mut tables)?;

    let partitions_desc = descriptor(&header, 80)?;
    let extents_desc = descriptor(&header, 92)?;
    let groups_desc = descriptor(&header, 104)?;
    let block_devices_desc = descriptor(&header, 116)?;
    if std::env::var_os("DUMPRX_DEBUG_LIBLP").is_some() {
        eprintln!(
            "liblp: hdr_size={header_size} tables_size={tables_size} tables_start={tables_start} part={partitions_desc:?} ext={extents_desc:?} grp={groups_desc:?} blk={block_devices_desc:?}"
        );
    }

    let partitions = parse_partitions(&tables, partitions_desc)?;
    let extents = parse_extents(&tables, extents_desc)?;
    let groups = parse_groups(&tables, groups_desc)?;
    let block_devices = parse_block_devices(&tables, block_devices_desc)?;

    Ok(Metadata {
        major,
        minor,
        partitions,
        extents,
        groups,
        block_devices,
    })
}

#[derive(Debug, Clone, Copy)]
struct Descriptor {
    offset: u32,
    num_entries: u32,
    entry_size: u32,
}

fn descriptor(header: &[u8], off: usize) -> Result<Descriptor> {
    if off + 12 > header.len() {
        bail!("metadata table descriptor at {off} is out of bounds");
    }
    Ok(Descriptor {
        offset: u32::from_le_bytes(header[off..off + 4].try_into().unwrap()),
        num_entries: u32::from_le_bytes(header[off + 4..off + 8].try_into().unwrap()),
        entry_size: u32::from_le_bytes(header[off + 8..off + 12].try_into().unwrap()),
    })
}

fn entry<'a>(tables: &'a [u8], desc: Descriptor, index: u32, min_size: u32) -> Result<&'a [u8]> {
    if desc.entry_size < min_size {
        bail!(
            "metadata table entry size {} is smaller than required {min_size}",
            desc.entry_size
        );
    }
    let start = desc.offset as u64 + index as u64 * desc.entry_size as u64;
    let end = start + desc.entry_size as u64;
    if end > tables.len() as u64 {
        bail!("metadata table entry {index} is out of bounds");
    }
    Ok(&tables[start as usize..end as usize])
}

fn parse_partitions(tables: &[u8], desc: Descriptor) -> Result<Vec<Partition>> {
    let mut out = Vec::new();
    for i in 0..desc.num_entries {
        let e = entry(tables, desc, i, 52)?;
        out.push(Partition {
            name: cstr(&e[..36]),
            attributes: u32::from_le_bytes(e[36..40].try_into().unwrap()),
            first_extent_index: u32::from_le_bytes(e[40..44].try_into().unwrap()),
            num_extents: u32::from_le_bytes(e[44..48].try_into().unwrap()),
            group_index: u32::from_le_bytes(e[48..52].try_into().unwrap()),
        });
    }
    Ok(out)
}

fn parse_extents(tables: &[u8], desc: Descriptor) -> Result<Vec<Extent>> {
    let mut out = Vec::new();
    for i in 0..desc.num_entries {
        let e = entry(tables, desc, i, 24)?;
        out.push(Extent {
            num_sectors: u64::from_le_bytes(e[0..8].try_into().unwrap()),
            target_type: u32::from_le_bytes(e[8..12].try_into().unwrap()),
            target_data: u32::from_le_bytes(e[12..16].try_into().unwrap()),
            target_source: u32::from_le_bytes(e[16..20].try_into().unwrap()),
        });
    }
    Ok(out)
}

fn parse_groups(tables: &[u8], desc: Descriptor) -> Result<Vec<Group>> {
    let mut out = Vec::new();
    for i in 0..desc.num_entries {
        let e = entry(tables, desc, i, 48)?;
        out.push(Group {
            name: cstr(&e[..36]),
            flags: u32::from_le_bytes(e[36..40].try_into().unwrap()),
            maximum_size: u64::from_le_bytes(e[40..48].try_into().unwrap()),
        });
    }
    Ok(out)
}

fn parse_block_devices(tables: &[u8], desc: Descriptor) -> Result<Vec<BlockDevice>> {
    let mut out = Vec::new();
    for i in 0..desc.num_entries {
        let e = entry(tables, desc, i, 64)?;
        out.push(BlockDevice {
            first_logical_sector: u64::from_le_bytes(e[0..8].try_into().unwrap()),
            size: u64::from_le_bytes(e[16..24].try_into().unwrap()),
            partition_name: cstr(&e[24..60]),
            flags: u32::from_le_bytes(e[60..64].try_into().unwrap()),
        });
    }
    Ok(out)
}

fn cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

pub fn partition_size(meta: &Metadata, p: &Partition) -> u64 {
    let start = p.first_extent_index as usize;
    let end = start.saturating_add(p.num_extents as usize);
    meta.extents[start.min(meta.extents.len())..end.min(meta.extents.len())]
        .iter()
        .map(|e| e.num_sectors * SECTOR)
        .sum()
}

pub fn partition_infos(meta: &Metadata) -> Vec<PartitionInfo> {
    meta.partitions
        .iter()
        .map(|p| {
            let start = p.first_extent_index as usize;
            let end = start.saturating_add(p.num_extents as usize);
            PartitionInfo {
                name: p.name.clone(),
                size: partition_size(meta, p),
                attributes: attributes_to_string(p.attributes),
                extents: meta.extents[start.min(meta.extents.len())..end.min(meta.extents.len())]
                    .iter()
                    .map(|e| ExtentInfo {
                        num_sectors: e.num_sectors,
                        target_type: e.target_type,
                        target_data: e.target_data,
                        target_source: e.target_source,
                    })
                    .collect(),
            }
        })
        .collect()
}

fn attributes_to_string(attr: u32) -> String {
    let mut out = Vec::new();
    if attr & LP_PARTITION_ATTR_READONLY != 0 {
        out.push("readonly");
    }
    if attr & LP_PARTITION_ATTR_SLOT_SUFFIXED != 0 {
        out.push("slot_suffixed");
    }
    if attr & LP_PARTITION_ATTR_UPDATED != 0 {
        out.push("updated");
    }
    if attr & LP_PARTITION_ATTR_DISABLED != 0 {
        out.push("disabled");
    }
    out.join(",")
}

/// Decide whether `name` should be extracted under the requested slot mode,
/// and return the output base name.
fn output_name(name: &str, meta: &Metadata, slot: SlotMode) -> Option<String> {
    let has_a = meta
        .partitions
        .iter()
        .any(|p| p.name == format!("{name}_a"));
    let has_b = meta
        .partitions
        .iter()
        .any(|p| p.name == format!("{name}_b"));
    let strip_suffix = |s: &str, suffix: &str| s.strip_suffix(suffix).map(str::to_owned);

    if name.ends_with("_a") {
        let base = strip_suffix(name, "_a").unwrap();
        match slot {
            SlotMode::A | SlotMode::Auto | SlotMode::All => Some(base),
            SlotMode::B => None,
        }
    } else if name.ends_with("_b") {
        let base = strip_suffix(name, "_b").unwrap();
        match slot {
            SlotMode::B => Some(base),
            SlotMode::All => Some(name.to_owned()),
            SlotMode::Auto if !has_a => Some(base),
            SlotMode::Auto | SlotMode::A => None,
        }
    } else if has_a || has_b {
        // A partition without a suffix that also has a suffixed variant is
        // usually metadata, not an extractable slot; keep it.
        Some(name.to_owned())
    } else {
        Some(name.to_owned())
    }
}

/// Extract all selected partitions into `out_dir`.
pub fn extract(
    super_path: &Path,
    out_dir: &Path,
    slot: SlotMode,
) -> Result<Vec<(String, PathBuf, u64)>> {
    extract_filtered(super_path, out_dir, slot, &[], false)
}

/// Extract selected partitions into `out_dir`.  An empty `filter` extracts
/// everything; otherwise each entry is matched exactly or as a base name
/// (e.g. `vendor` matches `vendor_a`).
pub fn extract_filtered(
    super_path: &Path,
    out_dir: &Path,
    slot: SlotMode,
    filter: &[String],
    disk_image: bool,
) -> Result<Vec<(String, PathBuf, u64)>> {
    let mut src = File::open(super_path)
        .with_context(|| format!("opening {}", super_path.display()))?;
    let meta = read_metadata(&mut src)?;
    std::fs::create_dir_all(out_dir)?;

    let matches_filter = |part_name: &str| -> bool {
        if filter.is_empty() {
            return true;
        }
        filter.iter().any(|f| {
            f == part_name
                || f.strip_suffix("_a") == Some(part_name)
                || f.strip_suffix("_b") == Some(part_name)
                || part_name.strip_suffix("_a") == Some(f.as_str())
                || part_name.strip_suffix("_b") == Some(f.as_str())
        })
    };

    let mut selected = Vec::new();
    let mut used_names = std::collections::HashSet::new();
    for p in &meta.partitions {
        if p.name.ends_with("-cow") || p.name.contains("-cow") {
            continue;
        }
        if p.attributes & LP_PARTITION_ATTR_DISABLED != 0 {
            continue;
        }
        if !matches_filter(&p.name) {
            continue;
        }
        if let Some(base) = output_name(&p.name, &meta, slot) {
            // First winner (e.g. _a) keeps the unsuffixed name; later
            // duplicates get their original name instead of overwriting.
            let name = if used_names.insert(base.clone()) {
                base
            } else {
                p.name.clone()
            };
            selected.push((p.clone(), name));
        }
    }

    let mut results = Vec::new();
    for (partition, name) in selected {
        let size = partition_size(&meta, &partition);
        let path = out_dir.join(format!("{name}.img"));
        let mut out = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        out.set_len(size)?;

        let start = partition.first_extent_index as usize;
        let end = start.saturating_add(partition.num_extents as usize);
        let mut out_sector: u64 = 0;
        for extent in &meta.extents[start.min(meta.extents.len())..end.min(meta.extents.len())] {
            if extent.target_type != 0 {
                bail!(
                    "partition {} uses unsupported extent target type {}",
                    partition.name,
                    extent.target_type
                );
            }
            let block_dev = meta
                .block_devices
                .get(extent.target_source as usize)
                .with_context(|| {
                    format!(
                        "partition {} references unknown block device {}",
                        partition.name, extent.target_source
                    )
                })?;
            // `target_data` is relative to the start of the super partition.
            // When reading a standalone super.img that is exactly what we
            // need.  For a full-disk image (GPT + super at an offset), the
            // block device's first_logical_sector must be added.
            let base_sector = if disk_image {
                block_dev.first_logical_sector
            } else {
                0
            };
            let src_offset = (base_sector + extent.target_data as u64) * SECTOR;
            let bytes = extent.num_sectors * SECTOR;
            src.seek(SeekFrom::Start(src_offset))?;
            out.seek(SeekFrom::Start(out_sector * SECTOR))?;
            copy_n(&mut src, &mut out, bytes).with_context(|| {
                format!(
                    "copying extent for partition {} from offset {src_offset}",
                    partition.name
                )
            })?;
            out_sector += extent.num_sectors;
        }
        out.flush()?;
        results.push((partition.name.clone(), path, size));
    }
    Ok(results)
}

fn copy_n(src: &mut File, dst: &mut File, mut bytes: u64) -> Result<()> {
    let mut buf = vec![0u8; 1024 * 1024];
    while bytes > 0 {
        let want = bytes.min(buf.len() as u64) as usize;
        let read = src.read(&mut buf[..want])?;
        if read == 0 {
            bail!("unexpected end of super image while copying extent");
        }
        dst.write_all(&buf[..read])?;
        bytes -= read as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn put_u16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn put_u32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn put_u64(v: &mut Vec<u8>, x: u64) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn name36(v: &mut Vec<u8>, s: &str) {
        let mut n = [0u8; 36];
        n[..s.len()].copy_from_slice(s.as_bytes());
        v.extend_from_slice(&n);
    }

    fn build_super() -> Vec<u8> {
        let max_size = 4096u32;
        let mut buf = vec![0u8; (PARTITION_RESERVED_BYTES + GEOMETRY_SIZE * 2 + max_size as u64) as usize];
        let mut geometry = Vec::new();
        put_u32(&mut geometry, GEOMETRY_MAGIC);
        put_u32(&mut geometry, 52);
        geometry.extend_from_slice(&[0u8; 32]);
        put_u32(&mut geometry, max_size);
        put_u32(&mut geometry, 1);
        put_u32(&mut geometry, 4096);
        buf[PARTITION_RESERVED_BYTES as usize..PARTITION_RESERVED_BYTES as usize + 52]
            .copy_from_slice(&geometry);

        let table_start = PARTITION_RESERVED_BYTES + GEOMETRY_SIZE * 2;
        let mut header = Vec::new();
        put_u32(&mut header, METADATA_HEADER_MAGIC);
        put_u16(&mut header, 10);
        put_u16(&mut header, 2);
        put_u32(&mut header, 256);
        header.extend_from_slice(&[0u8; 32]);
        // tables_size is filled later
        let tables_size_pos = header.len();
        put_u32(&mut header, 0);
        header.extend_from_slice(&[0u8; 32]);
        // descriptors: partitions, extents, groups, block devices
        put_u32(&mut header, 0); // partitions offset
        put_u32(&mut header, 1);
        put_u32(&mut header, 52);
        put_u32(&mut header, 52); // extents offset
        put_u32(&mut header, 1);
        put_u32(&mut header, 24);
        put_u32(&mut header, 76); // groups offset
        put_u32(&mut header, 1);
        put_u32(&mut header, 48);
        put_u32(&mut header, 124); // block devices offset
        put_u32(&mut header, 1);
        put_u32(&mut header, 64);
        put_u32(&mut header, 0);
        header.resize(256, 0);

        let mut tables = Vec::new();
        name36(&mut tables, "system_a");
        put_u32(&mut tables, LP_PARTITION_ATTR_READONLY | LP_PARTITION_ATTR_SLOT_SUFFIXED);
        put_u32(&mut tables, 0);
        put_u32(&mut tables, 1);
        put_u32(&mut tables, 0);
        put_u64(&mut tables, 2); // 2 sectors
        put_u32(&mut tables, 0);
        put_u32(&mut tables, 4); // target_data sector 4
        put_u32(&mut tables, 0);
        put_u32(&mut tables, 0); // padding to sizeof(LpMetadataExtent) == 24
        name36(&mut tables, "main");
        put_u32(&mut tables, 0);
        put_u64(&mut tables, 0);
        put_u64(&mut tables, 0); // first_logical_sector
        put_u32(&mut tables, 0);
        put_u32(&mut tables, 0);
        put_u64(&mut tables, 1 << 20);
        name36(&mut tables, "super");
        put_u32(&mut tables, 0);

        header[tables_size_pos..tables_size_pos + 4]
            .copy_from_slice(&(tables.len() as u32).to_le_bytes());
        buf[table_start as usize..table_start as usize + header.len()].copy_from_slice(&header);
        let tables_start = table_start as usize + 256;
        buf[tables_start..tables_start + tables.len()].copy_from_slice(&tables);
        // data at sector 4
        let data_off = 4 * SECTOR as usize;
        buf[data_off..data_off + 8].copy_from_slice(b"DATA1234");
        // metadata_max_size slot padding is already zero; ensure data fits
        buf
    }

    #[test]
    fn reads_metadata_and_extracts_partition() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/test-super");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let super_img = dir.join("super.img");
        std::fs::write(&super_img, build_super()).unwrap();
        let mut f = File::open(&super_img).unwrap();
        let meta = read_metadata(&mut f).unwrap();
        assert_eq!(meta.partitions.len(), 1);
        assert_eq!(meta.partitions[0].name, "system_a");
        assert_eq!(partition_size(&meta, &meta.partitions[0]), 1024);
        let out = dir.join("out");
        let extracted = extract(&super_img, &out, SlotMode::Auto).unwrap();
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].0, "system_a");
        assert_eq!(extracted[0].1, out.join("system.img"));
        let data = std::fs::read(out.join("system.img")).unwrap();
        assert_eq!(&data[..8], b"DATA1234");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
