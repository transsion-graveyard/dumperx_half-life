//! Embedded runtime for standalone builds.
//!
//! The release binary carries a gzip-compressed tar archive containing the
//! helper tools, glibc loader/shared libraries and a portable Python.  On
//! first use it is extracted to a per-user cache directory; after that the
//! binary does not need the source tree or host tools.

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use sha1::{Digest, Sha1};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const RUNTIME_ARCHIVE: &[u8] = include_bytes!("../assets/runtime.tar.gz");
static ROOT: OnceLock<PathBuf> = OnceLock::new();

pub fn root() -> Option<&'static Path> {
    ROOT.get().map(PathBuf::as_path)
}

pub fn ensure() -> Result<PathBuf> {
    if let Some(root) = ROOT.get() {
        return Ok(root.clone());
    }
    if let Some(dir) = std::env::var_os("DUMPRX_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        if !dir.is_dir() {
            bail!(
                "DUMPRX_RUNTIME_DIR is set to {}, which is not a directory",
                dir.display()
            );
        }
        let _ = ROOT.set(dir.clone());
        return Ok(dir);
    }

    let cache = prepare_cache()?;
    let mut hasher = Sha1::new();
    hasher.update(RUNTIME_ARCHIVE);
    let digest = format!("{:x}", hasher.finalize());
    let version = format!("{}-{}", env!("CARGO_PKG_VERSION"), &digest[..16]);
    let dir = cache.join(format!("runtime-{version}"));
    let marker = dir.join(".complete");

    if !marker.is_file() {
        let temp = cache.join(format!("runtime-{version}.tmp-{}", std::process::id()));
        if temp.exists() {
            fs::remove_dir_all(&temp)?;
        }
        fs::create_dir_all(&temp)?;
        extract(&temp)
            .with_context(|| format!("extracting embedded runtime into {}", temp.display()))?;
        fs::write(temp.join(".complete"), b"ok\n")?;
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::rename(&temp, &dir)
            .with_context(|| format!("installing runtime cache {}", dir.display()))?;
    }

    let _ = ROOT.set(dir.clone());
    Ok(dir)
}

fn cache_root() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("dumprx");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".cache").join("dumprx");
    }
    std::env::temp_dir().join("dumprx-cache")
}

/// Pick the first writable cache location.  A read-only HOME should not stop
/// the standalone binary from running; fall back to the temporary directory.
fn prepare_cache() -> Result<PathBuf> {
    let candidates = [cache_root(), std::env::temp_dir().join("dumprx-cache")];
    let mut errors = Vec::new();
    for candidate in candidates {
        if let Err(e) = fs::create_dir_all(&candidate) {
            errors.push(format!("{}: {e}", candidate.display()));
            continue;
        }
        let probe = candidate.join(".write-test");
        match fs::write(&probe, b"ok") {
            Ok(()) => {
                let _ = fs::remove_file(&probe);
                return Ok(candidate);
            }
            Err(e) => errors.push(format!("{}: {e}", candidate.display())),
        }
    }
    bail!(
        "no writable runtime cache directory (tried {})",
        errors.join(", ")
    )
}

fn extract(dest: &Path) -> Result<()> {
    let decoder = GzDecoder::new(Cursor::new(RUNTIME_ARCHIVE));
    let mut archive = tar::Archive::new(decoder);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    archive.unpack(dest)?;
    Ok(())
}
