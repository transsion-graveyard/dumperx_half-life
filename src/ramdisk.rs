//! Ramdisk decompression and extraction.

use crate::cpio;
use crate::fsimage;
use crate::util;
use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Cpio,
    Gzip,
    Lz4Legacy,
    Lz4Frame,
    Xz,
    Lzma,
    Bzip2,
    Zstd,
    Unknown,
}

impl Compression {
    pub fn name(self) -> &'static str {
        match self {
            Compression::Cpio => "cpio",
            Compression::Gzip => "gzip",
            Compression::Lz4Legacy => "lz4-legacy",
            Compression::Lz4Frame => "lz4-frame",
            Compression::Xz => "xz",
            Compression::Lzma => "lzma",
            Compression::Bzip2 => "bzip2",
            Compression::Zstd => "zstd",
            Compression::Unknown => "unknown",
        }
    }
}

pub fn detect(data: &[u8]) -> Compression {
    if cpio::is_cpio(data) {
        return Compression::Cpio;
    }
    if data.starts_with(&[0x1f, 0x8b]) {
        return Compression::Gzip;
    }
    if data.starts_with(&[0x02, 0x21, 0x4c, 0x18]) {
        return Compression::Lz4Legacy;
    }
    if data.starts_with(&[0x04, 0x22, 0x4d, 0x18]) {
        return Compression::Lz4Frame;
    }
    if data.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        return Compression::Xz;
    }
    if data.starts_with(&[0x5d, 0x00, 0x00]) {
        return Compression::Lzma;
    }
    if data.starts_with(b"BZh") {
        return Compression::Bzip2;
    }
    if data.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return Compression::Zstd;
    }
    Compression::Unknown
}

pub fn decompress(data: &[u8]) -> Result<(Compression, Vec<u8>)> {
    let kind = detect(data);
    let out = match kind {
        Compression::Cpio => data.to_vec(),
        Compression::Gzip => run_filter("gzip", &["-dc"], data)?,
        Compression::Lz4Legacy | Compression::Lz4Frame => {
            run_filter("lz4", &["-dc"], data)?
        }
        Compression::Xz => run_filter("xz", &["-dc"], data)?,
        Compression::Lzma => run_filter("xz", &["--format=lzma", "-dc"], data)?,
        Compression::Bzip2 => run_filter("bzip2", &["-dc"], data)?,
        Compression::Zstd => run_filter("zstd", &["-dc"], data)?,
        Compression::Unknown => {
            bail!("unsupported ramdisk compression (first bytes: {:02x?})", &data[..data.len().min(8)])
        }
    };
    Ok((kind, out))
}

fn run_filter(program: &str, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
    let tool = util::find_tool(program)
        .with_context(|| format!("decompressor '{program}' not found in utils/bin or PATH"))?;
    let mut child = Command::new(&tool)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {}", tool.display()))?;
    // Feed stdin from a separate thread: decompressors can fill the stdout
    // pipe before consuming all of stdin, which would deadlock a
    // write-then-read implementation on large ramdisks.
    let mut stdin = child
        .stdin
        .take()
        .context("decompressor stdin unavailable")?;
    let data = input.to_vec();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        stdin.write_all(&data)?;
        Ok(())
    });
    let output = child
        .wait_with_output()
        .with_context(|| format!("waiting for {}", tool.display()))?;
    let _ = writer.join();
    if !output.status.success() {
        bail!(
            "{} failed: {}",
            tool.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// Extract a (possibly compressed) ramdisk into `out_dir`.
pub fn extract(data: &[u8], out_dir: &Path) -> Result<cpio::ExtractStats> {
    let (kind, decompressed) = decompress(data)?;
    util::info(format!(
        "ramdisk compression: {}, {} bytes -> {} bytes",
        kind.name(),
        data.len(),
        decompressed.len()
    ));
    if cpio::is_cpio(&decompressed) {
        return cpio::extract(&decompressed, out_dir);
    }
    // Some old devices use an ext4 image as the ramdisk.
    let parent = out_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::temp_dir());
    util::ensure_dir(&parent)?;
    let temp = parent.join(format!(
        ".ramdisk-{}.img",
        std::process::id()
    ));
    std::fs::write(&temp, &decompressed)?;
    let result = fsimage::extract(&temp, out_dir, None);
    let _ = std::fs::remove_file(&temp);
    match result {
        Ok(_) => Ok(cpio::ExtractStats::default()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_compressions() {
        assert_eq!(detect(b"070701"), Compression::Cpio);
        assert_eq!(detect(&[0x1f, 0x8b, 0]), Compression::Gzip);
        assert_eq!(detect(&[0x02, 0x21, 0x4c, 0x18]), Compression::Lz4Legacy);
        assert_eq!(detect(&[0xfd, b'7', b'z', b'X', b'Z', 0]), Compression::Xz);
        assert_eq!(detect(b"BZh9"), Compression::Bzip2);
        assert_eq!(detect(&[0x28, 0xb5, 0x2f, 0xfd]), Compression::Zstd);
    }
}
