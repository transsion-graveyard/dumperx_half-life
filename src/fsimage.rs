//! Filesystem image detection and extraction (ext4 / EROFS / F2FS).

use crate::util::{self, Runner};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsType {
    Ext4,
    Erofs,
    F2fs,
    Unknown,
}

impl FsType {
    pub fn name(self) -> &'static str {
        match self {
            FsType::Ext4 => "ext4",
            FsType::Erofs => "erofs",
            FsType::F2fs => "f2fs",
            FsType::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FsInfo {
    pub fs_type: FsType,
    /// Number of bytes of vendor header to strip before the filesystem
    /// superblock (MOTO/ASUS images have one).
    pub header_offset: u64,
}

const EROFS_MAGIC: [u8; 4] = [0xe2, 0xe1, 0xf5, 0xe0];
const F2FS_MAGIC: [u8; 4] = [0x10, 0x20, 0xf5, 0xf2];
const EXT4_MAGIC: [u8; 2] = [0x53, 0xef];
const EXT4_SB_OFFSET: usize = 1024;
const EXT4_MAGIC_OFFSET: usize = 0x38;

pub fn detect(path: &Path) -> Result<FsInfo> {
    let head = util::read_head(path, 2 * 1024 * 1024)?;
    if head.len() < 1080 {
        return Ok(FsInfo {
            fs_type: FsType::Unknown,
            header_offset: 0,
        });
    }
    if head.len() >= 1028 && head[1024..1028] == EROFS_MAGIC {
        return Ok(FsInfo {
            fs_type: FsType::Erofs,
            header_offset: 0,
        });
    }
    if head.len() >= 1028 && head[1024..1028] == F2FS_MAGIC {
        return Ok(FsInfo {
            fs_type: FsType::F2fs,
            header_offset: 0,
        });
    }
    if head.len() >= EXT4_SB_OFFSET + EXT4_MAGIC_OFFSET + 2
        && head[EXT4_SB_OFFSET + EXT4_MAGIC_OFFSET..EXT4_SB_OFFSET + EXT4_MAGIC_OFFSET + 2]
            == EXT4_MAGIC
    {
        return Ok(FsInfo {
            fs_type: FsType::Ext4,
            header_offset: 0,
        });
    }

    // MOTO/ASUS style images: the filesystem starts after a vendor header.
    let starts_with_moto = head.len() >= 4 && &head[..4] == b"MOTO";
    let starts_with_asus = head.len() >= 4 && &head[..4] == b"ASUS";
    if starts_with_moto || starts_with_asus {
        if let Some(pos) = find_subslice(&head, &EXT4_MAGIC) {
            let offset = pos as u64 - EXT4_MAGIC_OFFSET as u64;
            let offset = if offset == 128_055 { 131_072 } else { offset };
            if offset > 0 {
                return Ok(FsInfo {
                    fs_type: FsType::Ext4,
                    header_offset: offset,
                });
            }
        }
    }

    Ok(FsInfo {
        fs_type: FsType::Unknown,
        header_offset: 0,
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Extract a filesystem image into `out_dir`.  `hint` is the fstab-declared
/// filesystem type and is only used when the image itself is inconclusive.
pub fn extract(path: &Path, out_dir: &Path, hint: Option<FsType>) -> Result<FsType> {
    util::ensure_dir(out_dir)?;
    let info = detect(path)?;
    let fs_type = if info.fs_type == FsType::Unknown {
        hint.unwrap_or(FsType::Unknown)
    } else {
        info.fs_type
    };

    let stripped_image: Option<PathBuf> = if info.header_offset > 0 {
        let parent = out_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let stripped = parent.join(format!(".{}-stripped.img", util::stem(path)));
        util::info(format!(
            "stripping {}-byte vendor header from {}",
            info.header_offset,
            path.display()
        ));
        strip_header(path, &stripped, info.header_offset)?;
        Some(stripped)
    } else {
        None
    };
    let image = stripped_image.as_deref().unwrap_or(path);

    let runner = Runner::new(false);
    let mut attempts = Vec::new();
    match fs_type {
        FsType::Erofs => attempts.push(FsType::Erofs),
        FsType::Ext4 => attempts.push(FsType::Ext4),
        FsType::F2fs => attempts.push(FsType::F2fs),
        FsType::Unknown => {
            attempts.extend([FsType::Erofs, FsType::Ext4, FsType::F2fs]);
        }
    }
    if fs_type != FsType::Unknown && attempts.len() == 1 {
        // Keep the other backends as fallbacks for mislabelled images.
        for fs in [FsType::Erofs, FsType::Ext4, FsType::F2fs] {
            if fs != fs_type {
                attempts.push(fs);
            }
        }
    }

    let mut errors = Vec::new();
    for attempt in attempts {
        match attempt {
            FsType::Erofs => {
                if let Some(tool) = util::find_tool("fsck.erofs") {
                    let extract_arg = format!("--extract={}", out_dir.display());
                    let ok = runner.try_run(
                        &tool,
                        &[
                            std::ffi::OsStr::new(&extract_arg),
                            std::ffi::OsStr::new("--overwrite"),
                            image.as_os_str(),
                        ],
                        None,
                    );
                    if ok && dir_has_entries(out_dir) {
                        cleanup_stripped(stripped_image.as_deref());
                        return Ok(FsType::Erofs);
                    }
                    errors.push("fsck.erofs failed".to_string());
                } else {
                    errors.push("fsck.erofs not found".to_string());
                }
            }
            FsType::Ext4 => {
                if let Some(tool) = util::find_tool("7zz") {
                    let out_arg = format!("-o{}", out_dir.display());
                    let ok = runner.try_run(
                        &tool,
                        &[
                            std::ffi::OsStr::new("x"),
                            std::ffi::OsStr::new("-snld"),
                            std::ffi::OsStr::new("-y"),
                            std::ffi::OsStr::new(&out_arg),
                            image.as_os_str(),
                        ],
                        None,
                    );
                    if ok && dir_has_entries(out_dir) {
                        cleanup_stripped(stripped_image.as_deref());
                        return Ok(FsType::Ext4);
                    }
                    errors.push("7zz ext4 extraction failed".to_string());
                } else {
                    errors.push("7zz not found".to_string());
                }
            }
            FsType::F2fs => {
                if let Some(tool) = util::find_tool("f2fs-extractor") {
                    let ok = runner.try_run(
                        &tool,
                        &[
                            std::ffi::OsStr::new("extract"),
                            image.as_os_str(),
                            out_dir.as_os_str(),
                        ],
                        None,
                    );
                    if ok && dir_has_entries(out_dir) {
                        cleanup_stripped(stripped_image.as_deref());
                        return Ok(FsType::F2fs);
                    }
                    errors.push("f2fs-extractor failed".to_string());
                } else {
                    errors.push("f2fs-extractor not found".to_string());
                }
            }
            FsType::Unknown => {}
        }
    }

    cleanup_stripped(stripped_image.as_deref());
    bail!(
        "could not extract {} (detected {}): {}",
        path.display(),
        info.fs_type.name(),
        errors.join("; ")
    )
}

fn cleanup_stripped(path: Option<&Path>) {
    if let Some(path) = path {
        let _ = fs::remove_file(path);
    }
}

fn dir_has_entries(dir: &Path) -> bool {
    fs::read_dir(dir)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

fn strip_header(src: &Path, dst: &Path, offset: u64) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut input = fs::File::open(src)?;
    input.seek(SeekFrom::Start(offset))?;
    let mut output = fs::File::create(dst)?;
    std::io::copy(&mut input, &mut output).with_context(|| {
        format!(
            "stripping {} bytes from {}",
            offset,
            src.display()
        )
    })?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_erofs_and_f2fs_magic() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/test-fs");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut erofs = vec![0u8; 2048];
        erofs[1024..1028].copy_from_slice(&EROFS_MAGIC);
        std::fs::write(dir.join("erofs.img"), &erofs).unwrap();
        let mut f2fs = vec![0u8; 2048];
        f2fs[1024..1028].copy_from_slice(&F2FS_MAGIC);
        std::fs::write(dir.join("f2fs.img"), &f2fs).unwrap();
        assert_eq!(detect(&dir.join("erofs.img")).unwrap().fs_type, FsType::Erofs);
        assert_eq!(detect(&dir.join("f2fs.img")).unwrap().fs_type, FsType::F2fs);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
