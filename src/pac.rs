//! Spreadtrum/Unisoc `.pac` firmware extraction.

use crate::util;
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const PAC_HEADER_SIZE: usize = 2124;
const PAC_MAGIC: u32 = 0xfffa_fffa;
const FILE_HEADER_SIZE: usize = 2580;

#[derive(Debug, Clone)]
struct PacFile {
    name: String,
    data_offset: u64,
    size: u64,
}

pub fn is_pac(path: &Path) -> bool {
    path.extension()
        .map(|e| e.eq_ignore_ascii_case("pac"))
        .unwrap_or(false)
}

pub fn extract(path: &Path, out_dir: &Path) -> Result<Vec<(String, u64)>> {
    util::ensure_dir(out_dir)?;
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let file_size = file.metadata()?.len();
    if file_size < PAC_HEADER_SIZE as u64 {
        bail!("PAC file is shorter than its header");
    }
    let mut header = vec![0u8; PAC_HEADER_SIZE];
    file.read_exact(&mut header)?;

    let version = utf16le(&header[0..44]);
    if version != "BP_R1.0.0" && version != "BP_R2.0.1" {
        util::warn(format!(
            "PAC version '{version}' is not one of BP_R1.0.0/BP_R2.0.1; trying anyway"
        ));
    }
    let hi_size = u32_at(&header, 44)? as u64;
    let lo_size = u32_at(&header, 48)? as u64;
    let declared_size = (hi_size << 32) | lo_size;
    if declared_size != file_size {
        bail!("PAC size mismatch: header says {declared_size}, file is {file_size}");
    }
    let partition_count = u32_at(&header, 1076)? as u64;
    let list_start = u32_at(&header, 1080)? as u64;
    if list_start < PAC_HEADER_SIZE as u64 || partition_count > 4096 {
        bail!("invalid PAC partition table (count {partition_count}, start {list_start})");
    }
    if list_start + partition_count * FILE_HEADER_SIZE as u64 > file_size {
        bail!("PAC partition table extends past end of file");
    }

    let mut entries = Vec::new();
    for i in 0..partition_count {
        let offset = list_start + i * FILE_HEADER_SIZE as u64;
        file.seek(SeekFrom::Start(offset))?;
        let mut fh = vec![0u8; FILE_HEADER_SIZE];
        file.read_exact(&mut fh)?;
        let length = u32_at(&fh, 0)? as usize;
        if length != FILE_HEADER_SIZE {
            bail!("unknown PAC file header size {length} at entry {i}");
        }
        let name = utf16le(&fh[516..1028]);
        let hi_part = u32_at(&fh, 1532)? as u64;
        let hi_off = u32_at(&fh, 1536)? as u64;
        let lo_part = u32_at(&fh, 1540)? as u64;
        let nfile = u32_at(&fh, 1544)?;
        let lo_off = u32_at(&fh, 1552)? as u64;
        let size = (hi_part << 32) | lo_part;
        let data_offset = (hi_off << 32) | lo_off;
        if nfile == 0 || size == 0 || name.is_empty() {
            continue;
        }
        if data_offset + size > file_size {
            bail!(
                "PAC entry '{name}' data extends past end of file (offset {data_offset}, size {size})"
            );
        }
        entries.push(PacFile {
            name: sanitize(&name),
            data_offset,
            size,
        });
    }

    let mut results = Vec::new();
    for entry in entries {
        if entry.name.is_empty() {
            continue;
        }
        let out_path = out_dir.join(&entry.name);
        file.seek(SeekFrom::Start(entry.data_offset))?;
        let mut out =
            File::create(&out_path).with_context(|| format!("creating {}", out_path.display()))?;
        let copied = std::io::copy(&mut Read::by_ref(&mut file).take(entry.size), &mut out)?;
        out.flush()?;
        if copied != entry.size {
            bail!("short read while extracting PAC entry {}", entry.name);
        }
        util::info(format!(
            "  {} -> {} ({})",
            entry.name,
            out_path.display(),
            util::human_size(entry.size)
        ));
        results.push((entry.name, entry.size));
    }
    Ok(results)
}

fn u32_at(data: &[u8], offset: usize) -> Result<u32> {
    let end = offset.checked_add(4).context("PAC offset overflow")?;
    if end > data.len() {
        bail!("PAC field at {offset} is out of bounds");
    }
    Ok(u32::from_le_bytes(data[offset..end].try_into().unwrap()))
}

fn utf16le(data: &[u8]) -> String {
    let mut units = Vec::with_capacity(data.len() / 2);
    for chunk in data.chunks_exact(2) {
        units.push(u16::from_le_bytes([chunk[0], chunk[1]]));
    }
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
        .trim()
        .trim_matches('\u{feff}')
        .to_string()
}

fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    base.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' '))
        .collect::<String>()
        .trim_matches('.')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_u32(buf: &mut [u8], offset: usize, value: u32) {
        buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_utf16(buf: &mut [u8], offset: usize, value: &str) {
        for (i, unit) in value.encode_utf16().enumerate() {
            let start = offset + i * 2;
            buf[start..start + 2].copy_from_slice(&unit.to_le_bytes());
        }
    }

    #[test]
    fn extracts_pac_entries() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/test-pac");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let data = b"PACDATA!";
        let list_start = PAC_HEADER_SIZE as u32;
        let data_offset = list_start as usize + FILE_HEADER_SIZE;
        let total = data_offset + data.len();
        let mut bytes = vec![0u8; total];
        put_utf16(&mut bytes[0..44], 0, "BP_R2.0.1");
        put_u32(&mut bytes, 44, 0);
        put_u32(&mut bytes, 48, total as u32);
        put_u32(&mut bytes, 1076, 1);
        put_u32(&mut bytes, 1080, list_start);
        let fh = PAC_HEADER_SIZE;
        put_u32(&mut bytes, fh, FILE_HEADER_SIZE as u32);
        put_utf16(&mut bytes[fh..], 4, "boot");
        put_utf16(&mut bytes[fh..], 516, "boot.img");
        put_u32(&mut bytes, fh + 1540, data.len() as u32);
        put_u32(&mut bytes, fh + 1544, 1);
        put_u32(&mut bytes, fh + 1552, data_offset as u32);
        bytes[data_offset..].copy_from_slice(data);
        let pac = dir.join("firmware.pac");
        std::fs::write(&pac, &bytes).unwrap();
        let out = dir.join("out");
        let results = extract(&pac, &out).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(std::fs::read(out.join("boot.img")).unwrap(), data);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
