//! Huawei `UPDATE.APP` extraction (native eMMC firmware container).

use crate::util;
use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const MAGIC: [u8; 4] = [0x55, 0xaa, 0x5a, 0xa5];
const CHUNK: usize = 1024 * 1024;

pub fn is_update_app(path: &Path, head: &[u8]) -> bool {
    path.file_name()
        .map(|n| n.to_string_lossy() == "UPDATE.APP")
        .unwrap_or(false)
        || head.starts_with(&MAGIC)
}

/// Extract all (or selected) entries of an UPDATE.APP file.
/// `wanted` names are lowercase without the `.img` suffix; an empty list
/// extracts everything.
pub fn extract(path: &Path, out_dir: &Path, wanted: &[String]) -> Result<Vec<(String, u64)>> {
    util::ensure_dir(out_dir)?;
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let file_len = file.metadata()?.len();
    let mut results = Vec::new();
    let mut found_any = false;
    let mut offset = 0u64;

    'scan: while offset + MAGIC.len() as u64 <= file_len {
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; CHUNK];
        let read = file.read(&mut buf)?;
        buf.truncate(read);
        if buf.len() < MAGIC.len() {
            break;
        }
        if let Some(pos) = find_magic(&buf) {
            let abs = offset + pos as u64;
            file.seek(SeekFrom::Start(abs + 4))?;
            match parse_entry(&mut file, file_len, abs) {
                Ok((name, size, data_start)) => {
                    found_any = true;
                    if wanted.is_empty() || wanted.iter().any(|w| w == &name) {
                        let out_path = out_dir.join(format!("{name}.img"));
                        extract_data(&mut file, data_start, size, &out_path)?;
                        results.push((name, size));
                        util::info(format!(
                            "  {} -> {} ({})",
                            path.file_name().unwrap_or_default().to_string_lossy(),
                            out_path.display(),
                            util::human_size(size)
                        ));
                    }
                    offset = (data_start + size + 3) & !3;
                    continue 'scan;
                }
                Err(e) => {
                    util::warn(format!(
                        "UPDATE.APP: ignoring malformed entry at offset {abs:#x}: {e:#}"
                    ));
                    offset = abs + 4;
                    continue 'scan;
                }
            }
        }
        // Keep a 3-byte overlap so a magic split across chunks is found.
        offset += (buf.len() as u64).saturating_sub(3).max(1);
    }

    if !found_any {
        bail!("no UPDATE.APP entries found");
    }
    Ok(results)
}

fn find_magic(buf: &[u8]) -> Option<usize> {
    buf.windows(MAGIC.len()).position(|w| w == MAGIC)
}

fn parse_entry(file: &mut File, file_len: u64, match_offset: u64) -> Result<(String, u64, u64)> {
    let mut u32buf = [0u8; 4];
    file.read_exact(&mut u32buf)?;
    let header_size = u32::from_le_bytes(u32buf) as u64;
    if header_size < 98 || match_offset + header_size > file_len {
        bail!("invalid header size {header_size}");
    }
    file.seek(SeekFrom::Current(16))?;
    file.read_exact(&mut u32buf)?;
    let filesize = u32::from_le_bytes(u32buf) as u64;
    file.seek(SeekFrom::Current(32))?;
    let mut name_buf = [0u8; 16];
    file.read_exact(&mut name_buf)?;
    let raw_name = String::from_utf8_lossy(&name_buf);
    let name: String = raw_name
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect::<String>()
        .to_ascii_lowercase();
    let name = sanitize_name(&name);
    if name.is_empty() {
        bail!("empty entry name");
    }
    file.seek(SeekFrom::Current(22))?;
    let remaining = header_size.saturating_sub(98);
    if remaining > 0 {
        let mut skip = vec![0u8; remaining.min(1024 * 1024) as usize];
        let mut left = remaining;
        while left > 0 {
            let want = left.min(skip.len() as u64) as usize;
            file.read_exact(&mut skip[..want])?;
            left -= want as u64;
        }
    }
    let data_start = match_offset + header_size;
    Ok((name, filesize, data_start))
}

fn sanitize_name(name: &str) -> String {
    let name = name.trim();
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim();
    base.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' '))
        .collect::<String>()
        .trim_matches('.')
        .to_string()
}

fn extract_data(
    reader: &mut File,
    data_start: u64,
    size: u64,
    out_path: &Path,
) -> Result<()> {
    reader.seek(SeekFrom::Start(data_start))?;
    let mut out = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(out_path)
        .with_context(|| format!("creating {}", out_path.display()))?;
    let mut left = size;
    let mut buf = vec![0u8; CHUNK];
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        reader.read_exact(&mut buf[..want])?;
        out.write_all(&buf[..want])?;
        left -= want as u64;
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, data: &[u8]) -> Vec<u8> {
        let header_size = 98usize;
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&(header_size as u32).to_le_bytes());
        out.extend_from_slice(&[0u8; 16]);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0u8; 32]);
        let mut n = [0u8; 16];
        n[..name.len()].copy_from_slice(name.as_bytes());
        out.extend_from_slice(&n);
        out.extend_from_slice(&[0u8; 22]);
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out
    }

    #[test]
    fn extracts_entries() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/test-uapp");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let app = dir.join("UPDATE.APP");
        let mut bytes = entry("boot", b"bootdata");
        bytes.extend(entry("system", b"systemdata"));
        std::fs::write(&app, bytes).unwrap();
        let out = dir.join("out");
        let results = extract(&app, &out, &[]).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(std::fs::read(out.join("boot.img")).unwrap(), b"bootdata");
        assert_eq!(std::fs::read(out.join("system.img")).unwrap(), b"systemdata");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
