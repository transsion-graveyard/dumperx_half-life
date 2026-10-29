//! cpio (newc) ramdisk extraction.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Default)]
pub struct ExtractStats {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub skipped: u64,
}

const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;

pub fn extract(data: &[u8], out_dir: &Path) -> Result<ExtractStats> {
    if !data.starts_with(b"070701") && !data.starts_with(b"070702") {
        bail!("not a newc cpio archive");
    }
    fs::create_dir_all(out_dir)?;
    let mut stats = ExtractStats::default();
    let mut pos = 0usize;
    let mut hardlinks: HashMap<(u32, u32, u32), PathBuf> = HashMap::new();

    while pos + 110 <= data.len() {
        let header = &data[pos..pos + 110];
        if &header[..6] != b"070701" && &header[..6] != b"070702" {
            bail!("bad cpio header magic at offset {pos}");
        }
        let field = |i: usize| -> Result<u32> {
            let start = 6 + i * 8;
            let text = std::str::from_utf8(&header[start..start + 8])
                .context("non-UTF8 cpio header field")?;
            u32::from_str_radix(text, 16).context("invalid cpio header field")
        };
        let ino = field(0)?;
        let mode = field(1)?;
        let _uid = field(2)?;
        let _gid = field(3)?;
        let nlink = field(4)?;
        let _mtime = field(5)?;
        let filesize = field(6)? as usize;
        let devmajor = field(7)?;
        let devminor = field(8)?;
        let namesize = field(11)? as usize;
        if namesize == 0 || pos + 110 + namesize > data.len() {
            bail!("invalid cpio name size {namesize} at offset {pos}");
        }
        let name_bytes = &data[pos + 110..pos + 110 + namesize];
        let name_end = name_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(name_bytes.len());
        let name = String::from_utf8_lossy(&name_bytes[..name_end]).into_owned();
        pos += 110 + namesize;
        pos = align4(pos);
        if name == "TRAILER!!!" {
            break;
        }
        if pos + filesize > data.len() {
            bail!("cpio entry '{name}' data extends past archive end");
        }
        let file_data = &data[pos..pos + filesize];
        pos = align4(pos + filesize);

        let Some(relative) = safe_relative(&name) else {
            stats.skipped += 1;
            continue;
        };
        let target = out_dir.join(&relative);
        let file_type = mode & S_IFMT;

        match file_type {
            S_IFDIR => {
                fs::create_dir_all(&target)?;
                let _ = fs::set_permissions(&target, fs::Permissions::from_mode(mode & 0o7777));
                stats.dirs += 1;
            }
            S_IFREG => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                if nlink > 1 {
                    let key = (devmajor, devminor, ino);
                    if let Some(existing) = hardlinks.get(&key) {
                        let _ = fs::remove_file(&target);
                        if fs::hard_link(existing, &target).is_ok() {
                            stats.files += 1;
                            continue;
                        }
                    } else {
                        hardlinks.insert(key, target.clone());
                    }
                }
                fs::write(&target, file_data)?;
                let _ = fs::set_permissions(&target, fs::Permissions::from_mode(mode & 0o7777));
                stats.files += 1;
            }
            S_IFLNK => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                let link = String::from_utf8_lossy(file_data).into_owned();
                let _ = fs::remove_file(&target);
                symlink(link, &target)?;
                stats.symlinks += 1;
            }
            _ => {
                // Device nodes, fifos and sockets cannot be created without
                // privileges; count them so the caller can report them.
                stats.skipped += 1;
            }
        }
    }
    Ok(stats)
}

fn align4(value: usize) -> usize {
    (value + 3) & !3
}

fn safe_relative(name: &str) -> Option<PathBuf> {
    let name = name.trim_start_matches("./");
    if name.is_empty() {
        return None;
    }
    let path = Path::new(name);
    if path.is_absolute() {
        return None;
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(path.to_path_buf())
}

/// Check whether a byte slice is an uncompressed newc cpio archive.
pub fn is_cpio(data: &[u8]) -> bool {
    data.starts_with(b"070701") || data.starts_with(b"070702")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, mode: u32, data: &[u8], ino: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let namesize = name.len() + 1;
        out.extend_from_slice(b"070701");
        for value in [
            ino,
            mode,
            0,
            0,
            1,
            0,
            data.len() as u32,
            0,
            0,
            0,
            0,
            namesize as u32,
            0,
        ] {
            out.extend_from_slice(format!("{value:08x}").as_bytes());
        }
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out
    }

    #[test]
    fn extracts_files_and_symlinks() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/test-cpio");
        let _ = std::fs::remove_dir_all(&dir);
        let mut archive = Vec::new();
        archive.extend(entry("etc", S_IFDIR | 0o755, b"", 1));
        archive.extend(entry("etc/init.rc", S_IFREG | 0o644, b"on boot\n", 2));
        archive.extend(entry("init", S_IFLNK | 0o777, b"/system/bin/init", 3));
        archive.extend(entry("TRAILER!!!", 0, b"", 0));
        let stats = extract(&archive, &dir).unwrap();
        assert_eq!(stats.files, 1);
        assert_eq!(stats.dirs, 1);
        assert_eq!(stats.symlinks, 1);
        assert_eq!(
            std::fs::read_to_string(dir.join("etc/init.rc")).unwrap(),
            "on boot\n"
        );
        assert_eq!(
            std::fs::read_link(dir.join("init")).unwrap(),
            Path::new("/system/bin/init")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_path_traversal() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/test-cpio-escape");
        let _ = std::fs::remove_dir_all(&dir);
        let mut archive = Vec::new();
        archive.extend(entry("../evil", S_IFREG | 0o644, b"x", 1));
        archive.extend(entry("TRAILER!!!", 0, b"", 0));
        let stats = extract(&archive, &dir).unwrap();
        assert_eq!(stats.skipped, 1);
        assert!(!dir.parent().unwrap().join("evil").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
