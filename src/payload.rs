//! OTA payload extraction through the bundled payload-dumper-go.

use crate::util::{self, Runner};
use anyhow::{Context, Result};
use std::path::Path;

pub fn is_payload(head: &[u8]) -> bool {
    head.len() >= 4 && &head[..4] == b"CrAU"
}

pub fn extract(payload: &Path, out_dir: &Path) -> Result<()> {
    util::ensure_dir(out_dir)?;
    let tool = util::require_tool("payload-dumper-go").with_context(|| {
        format!(
            "payload-dumper-go is required to unpack {}",
            payload.display()
        )
    })?;
    let jobs = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .to_string();
    let runner = Runner::new(false);
    runner.run(
        &tool,
        &[
            std::ffi::OsStr::new("-c"),
            std::ffi::OsStr::new(&jobs),
            std::ffi::OsStr::new("-o"),
            out_dir.as_os_str(),
            payload.as_os_str(),
        ],
        None,
    )?;
    Ok(())
}
