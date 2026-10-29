//! Archive listing/extraction through the bundled 7-Zip binary.

use crate::util::{self, Runner};
use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ArchiveEntry {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    pub encrypted: bool,
}

pub fn is_archive_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        ".zip", ".rar", ".7z", ".tar", ".tar.gz", ".tgz", ".tar.md5",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

pub fn list(archive: &Path) -> Result<Vec<ArchiveEntry>> {
    let tool = util::require_tool("7zz").or_else(|_| util::require_tool("7z"))?;
    let runner = Runner::new(false);
    let output = runner.capture_stdout(
        &tool,
        &[
            OsStr::new("l"),
            OsStr::new("-ba"),
            OsStr::new("-slt"),
            archive.as_os_str(),
        ],
        None,
    )?;
    let mut entries = Vec::new();
    let mut path = None;
    let mut size = 0u64;
    let mut is_dir = false;
    let mut encrypted = false;
    let flush = |entries: &mut Vec<ArchiveEntry>,
                 path: &mut Option<String>,
                 size: u64,
                 is_dir: bool,
                 encrypted: bool| {
        if let Some(path) = path.take() {
            entries.push(ArchiveEntry {
                path,
                size,
                is_dir,
                encrypted,
            });
        }
    };
    for line in output.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            flush(&mut entries, &mut path, size, is_dir, encrypted);
            size = 0;
            is_dir = false;
            encrypted = false;
            continue;
        }
        let Some((key, value)) = line.split_once(" = ") else {
            continue;
        };
        match key {
            "Path" => {
                flush(&mut entries, &mut path, size, is_dir, encrypted);
                path = Some(value.to_string());
                size = 0;
                is_dir = false;
                encrypted = false;
            }
            "Size" => size = value.trim().parse().unwrap_or(0),
            "Folder" => is_dir = value.trim() == "+",
            "Encrypted" => encrypted = value.trim() == "+",
            _ => {}
        }
    }
    flush(&mut entries, &mut path, size, is_dir, encrypted);
    Ok(entries)
}

pub fn extract(archive: &Path, out_dir: &Path) -> Result<()> {
    util::ensure_dir(out_dir)?;
    let tool = util::require_tool("7zz").or_else(|_| util::require_tool("7z"))?;
    let runner = Runner::new(false);
    let out_arg = format!("-o{}", out_dir.display());
    runner.run(
        &tool,
        &[
            OsStr::new("x"),
            OsStr::new("-y"),
            OsStr::new("-p"),
            OsStr::new(&out_arg),
            archive.as_os_str(),
        ],
        None,
    )?;
    Ok(())
}

/// Find the single top-level directory of an extracted archive, if any.
pub fn single_root(dir: &Path) -> Result<Option<PathBuf>> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.retain(|p| {
        p.file_name()
            .map(|n| n != "__MACOSX" && n != ".DS_Store")
            .unwrap_or(true)
    });
    if entries.len() == 1 && entries[0].is_dir() {
        Ok(Some(entries[0].clone()))
    } else {
        Ok(None)
    }
}

pub fn ensure_archive_support() -> Result<()> {
    if util::find_tool("7zz").is_none() && util::find_tool("7z").is_none() {
        bail!(
            "no 7-Zip binary found (expected {} or 7zz in PATH)",
            util::utils_dir().join("bin/7zz").display()
        );
    }
    Ok(())
}
