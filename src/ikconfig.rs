//! Kernel `.config` extraction (native `extract-ikconfig` replacement).
//!
//! The kernel stores its config as `IKCFG_ST` + a compressed blob +
//! `IKCFG_ED`.  The kernel image itself may also be compressed (LZ4, gzip,
//! xz, ...), so both layers are handled.

use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;

const START: &[u8] = b"IKCFG_ST";
const END: &[u8] = b"IKCFG_ED";

const MAGICS: &[(&[u8], &[&str])] = &[
    (&[0x1f, 0x8b, 0x08], &["gzip", "-dc"]),
    (&[0xfd, b'7', b'z', b'X', b'Z', 0x00], &["xz", "-dc"]),
    (&[0x42, 0x5a, 0x68], &["bzip2", "-dc"]),
    (&[0x02, 0x21, 0x4c, 0x18], &["lz4", "-dc"]),
    (&[0x04, 0x22, 0x4d, 0x18], &["lz4", "-dc"]),
    (&[0x5d, 0x00, 0x00], &["xz", "--format=lzma", "-dc"]),
    (&[0x28, 0xb5, 0x2f, 0xfd], &["zstd", "-dc"]),
];

pub fn extract(kernel: &[u8]) -> Result<Option<String>> {
    // 1. Uncompressed kernel, or a config whose compression type is known.
    if let Some(config) = try_markers(kernel) {
        return Ok(Some(config));
    }

    // 2. Compressed kernel image.  Try the outermost magic first (offset 0),
    //    then every magic occurrence (older extract-ikconfig behaviour).
    let mut tried = std::collections::HashSet::new();
    for (magic, args) in MAGICS {
        let mut start = 0usize;
        while let Some(pos) = find(&kernel[start..], magic) {
            let offset = start + pos;
            if tried.insert((offset, magic[0])) {
                let candidate = &kernel[offset..];
                if let Ok(plain) =
                    run_filter(args[0], args[1..].iter().copied().collect(), candidate)
                {
                    if let Some(config) = try_markers(&plain) {
                        return Ok(Some(config));
                    }
                }
            }
            start = offset + magic.len().max(1);
            if start >= kernel.len() {
                break;
            }
        }
    }
    Ok(None)
}

fn try_markers(data: &[u8]) -> Option<String> {
    let start = find(data, START)? + START.len();
    let end = find(&data[start..], END)? + start;
    let blob = &data[start..end];
    if blob.is_empty() {
        return Some(String::new());
    }
    // The blob between the markers is normally compressed.
    for (magic, args) in MAGICS {
        if blob.starts_with(magic) {
            if let Ok(plain) = run_filter(args[0], args[1..].iter().copied().collect(), blob) {
                return Some(String::from_utf8_lossy(&plain).into_owned());
            }
        }
    }
    Some(String::from_utf8_lossy(blob).into_owned())
}

fn run_filter(program: &str, args: Vec<&str>, input: &[u8]) -> Result<Vec<u8>> {
    let tool = crate::util::find_tool(program)
        .with_context(|| format!("decompressor '{program}' not found"))?;
    let mut child = crate::util::tool_command(&tool)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning {}", tool.display()))?;
    let mut stdin = child
        .stdin
        .take()
        .context("decompressor stdin unavailable")?;
    let data = input.to_vec();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        // gzip exits with status 2 on trailing garbage, but the data it
        // produced before that is still valid.  Broken pipe is expected when
        // the decompressor stops early; it is not an error here.
        let _ = stdin.write_all(&data);
        Ok(())
    });
    let output = child.wait_with_output()?;
    let _ = writer.join();
    if output.stdout.is_empty() && !output.status.success() {
        anyhow::bail!(
            "{} failed: {}",
            tool.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Extract `ikconfig` from a kernel file and write it to `output`.
pub fn extract_to_file(kernel_path: &Path, output: &Path) -> Result<bool> {
    let data =
        fs::read(kernel_path).with_context(|| format!("reading {}", kernel_path.display()))?;
    match extract(&data)? {
        Some(config) => {
            fs::write(output, config).with_context(|| format!("writing {}", output.display()))?;
            Ok(true)
        }
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_markers_from_plain_blob() {
        let blob = b"junkIKCFG_ST# config\nCONFIG_X=y\nIKCFG_EDmore";
        let out = try_markers(blob).unwrap();
        assert_eq!(out, "# config\nCONFIG_X=y\n");
    }
}
