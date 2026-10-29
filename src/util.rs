//! Shared helpers: tool discovery, process execution, logging.

use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn project_dir() -> PathBuf {
    std::env::var_os("DUMPRX_PROJECT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        })
}

pub fn utils_dir() -> PathBuf {
    project_dir().join("utils")
}

pub fn info(msg: impl AsRef<str>) {
    println!("[*] {}", msg.as_ref());
}

pub fn step(msg: impl AsRef<str>) {
    println!("\n==> {}", msg.as_ref());
}

pub fn warn(msg: impl AsRef<str>) {
    eprintln!("[!] {}", msg.as_ref());
}

pub fn error(msg: impl AsRef<str>) {
    eprintln!("[ERROR] {}", msg.as_ref());
}

/// Find a tool.  Bundled tools in `utils/bin` win over PATH so the dump
/// behaves identically on every machine and never downloads anything.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let bundled = utils_dir().join("bin").join(name);
    if bundled.is_file() {
        return Some(bundled);
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

pub fn require_tool(name: &str) -> Result<PathBuf> {
    find_tool(name).with_context(|| {
        format!(
            "required tool '{name}' was not found in {} or PATH",
            utils_dir().join("bin").display()
        )
    })
}

/// A command runner that streams child output to our stdout/stderr.
pub struct Runner {
    verbose: bool,
}

impl Runner {
    pub fn new(verbose: bool) -> Self {
        Runner { verbose }
    }

    pub fn run(&self, program: &Path, args: &[&OsStr], cwd: Option<&Path>) -> Result<()> {
        self.run_with_stdin(program, args, cwd, None)
    }

    pub fn run_with_stdin(
        &self,
        program: &Path,
        args: &[&OsStr],
        cwd: Option<&Path>,
        stdin_data: Option<&[u8]>,
    ) -> Result<()> {
        if self.verbose {
            info(format!(
                "running: {} {}",
                program.display(),
                args.iter()
                    .map(|a| a.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        let mut cmd = Command::new(program);
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        match stdin_data {
            Some(data) => {
                cmd.stdin(Stdio::piped());
                cmd.stdout(Stdio::inherit());
                cmd.stderr(Stdio::inherit());
                let mut child = cmd
                    .spawn()
                    .with_context(|| format!("spawning {}", program.display()))?;
                {
                    let mut stdin = child
                        .stdin
                        .take()
                        .context("child stdin unavailable")?;
                    stdin.write_all(data)?;
                }
                let status = child.wait()?;
                if !status.success() {
                    bail!(
                        "{} exited with {}",
                        program.display(),
                        status
                            .code()
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "signal".into())
                    );
                }
            }
            None => {
                let status = cmd
                    .status()
                    .with_context(|| format!("running {}", program.display()))?;
                if !status.success() {
                    bail!(
                        "{} exited with {}",
                        program.display(),
                        status
                            .code()
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "signal".into())
                    );
                }
            }
        }
        Ok(())
    }

    pub fn capture(
        &self,
        program: &Path,
        args: &[&OsStr],
        cwd: Option<&Path>,
    ) -> Result<std::process::Output> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        cmd.output()
            .with_context(|| format!("running {}", program.display()))
    }

    pub fn capture_stdout(
        &self,
        program: &Path,
        args: &[&OsStr],
        cwd: Option<&Path>,
    ) -> Result<String> {
        let out = self.capture(program, args, cwd)?;
        if !out.status.success() {
            bail!(
                "{} failed: {}",
                program.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run a command and treat a non-zero exit as a soft failure.
    pub fn try_run(&self, program: &Path, args: &[&OsStr], cwd: Option<&Path>) -> bool {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        matches!(cmd.status(), Ok(s) if s.success())
    }
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        ensure_dir(parent)?;
    }
    fs::copy(src, dst)
        .with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
    Ok(())
}

pub fn move_or_copy(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        ensure_dir(parent)?;
    }
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => copy_file(src, dst),
    }
}

/// Remove a directory if it exists, ignoring errors.
pub fn remove_dir_all(path: &Path) {
    if path.exists() {
        let _ = fs::remove_dir_all(path);
    }
}

/// Read up to `n` bytes from the start of a file.
pub fn read_head(path: &Path, n: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut buf = vec![0u8; n];
    let mut read = 0;
    while read < n {
        match file.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(k) => read += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    buf.truncate(read);
    Ok(buf)
}

/// Return the basename without a trailing `.img` / `.bin` extension.
pub fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Normalize a partition name by removing the `_a`/`_b` slot suffix.
pub fn base_partition(name: &str) -> &str {
    name.strip_suffix("_a")
        .or_else(|| name.strip_suffix("_b"))
        .unwrap_or(name)
}

/// True when the file name looks like an Android partition image.
pub fn is_image_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".img") || lower.ends_with(".bin") || lower.ends_with(".image")
}

/// List regular files in a directory (non-recursive), sorted.
pub fn list_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    Ok(files)
}

/// List directories in a directory (non-recursive), sorted.
pub fn list_dirs(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    Ok(dirs)
}
