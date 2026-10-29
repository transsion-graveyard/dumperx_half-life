//! SHA-1 manifest generation (native replacement for the shell helpers).

use anyhow::{Context, Result};
use sha1::{Digest, Sha1};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub fn sha1_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Write `all_files.txt` containing every file in `root` (relative path,
/// sorted), excluding git metadata and the manifests themselves.
pub fn write_all_files(root: &Path) -> Result<PathBuf> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        if rel == "all_files.txt" || rel == "all_files.sha1" {
            continue;
        }
        if rel.starts_with(".git/") {
            continue;
        }
        files.push(rel);
    }
    files.sort();
    let out = root.join("all_files.txt");
    fs::write(&out, files.join("\n") + "\n")?;
    Ok(out)
}

/// Resolve a blob path inside the dump, trying the same fallbacks as the old
/// `write_sha1sum` helper.
pub fn resolve_blob(root: &Path, blob: &str) -> Option<PathBuf> {
    let blob = blob.trim();
    if blob.is_empty() || blob.starts_with('#') {
        return None;
    }
    let candidates = [
        root.join(blob),
        root.join("system").join(blob),
        root.join("system/system").join(blob),
    ];
    candidates.into_iter().find(|p| p.exists())
}

/// Write `path|sha1` for every resolvable entry in `list_path`.
pub fn write_sha1_manifest(root: &Path, list_path: &Path, dest: &Path) -> Result<()> {
    let text = fs::read_to_string(list_path)
        .with_context(|| format!("reading {}", list_path.display()))?;
    let mut out = String::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        match resolve_blob(root, line) {
            Some(path) => match sha1_file(&path) {
                Ok(hash) => {
                    out.push_str(line);
                    out.push('|');
                    out.push_str(&hash);
                    out.push('\n');
                }
                Err(_) => {
                    out.push_str(line);
                    out.push('\n');
                }
            },
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    fs::write(dest, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_known_value() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/test-sha1");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha1_file(&path).unwrap(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
