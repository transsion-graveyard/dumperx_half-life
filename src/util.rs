//! Shared helpers: tool discovery, process execution, logging.

use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn project_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("DUMPRX_PROJECT_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(root) = crate::runtime::root() {
        return root.to_path_buf();
    }

    // Prefer the directory layout around the binary so a release tarball
    // containing `dumprx` plus `utils/` works after being moved anywhere.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bin_dir) = exe.parent() {
            let candidates = [
                Some(bin_dir.to_path_buf()),
                bin_dir.parent().map(Path::to_path_buf),
                bin_dir
                    .parent()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf),
            ];
            for candidate in candidates.into_iter().flatten() {
                if candidate.join("utils").is_dir() {
                    return candidate;
                }
            }
        }
    }

    // Development fallback when the binary is run from a build directory
    // outside the expected target/{debug,release} layout.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
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

/// Build a Command for a helper, using the embedded glibc loader and
/// libraries when running a dynamically linked runtime helper.
pub fn tool_command(program: &Path) -> Command {
    let mut cmd;
    if let Some(root) = crate::runtime::root() {
        let libdir = root.join("lib");
        let loader = libdir.join("ld-linux-x86-64.so.2");
        if program.starts_with(root.join("utils")) && loader.is_file() && elf_has_interp(program) {
            cmd = Command::new(&loader);
            cmd.arg("--inhibit-cache");
            cmd.arg("--library-path").arg(&libdir).arg(program);
        } else {
            cmd = Command::new(program);
        }
        cmd.env("LD_LIBRARY_PATH", &libdir);
        // Let tools spawned by scripts (for example Python's KDZ zstd
        // fallback) find the bundled utilities too.
        let bundled_bin = root.join("utils/bin");
        let mut paths = vec![bundled_bin];
        if let Some(existing) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        if let Ok(joined) = std::env::join_paths(paths) {
            cmd.env("PATH", joined);
        }

        let name = program
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if name.starts_with("python3") {
            cmd.env("PYTHONHOME", root.join("python"));
            let python_lib = root.join("python/lib");
            if let Some(version_dir) = fs::read_dir(&python_lib).ok().and_then(|entries| {
                entries.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().starts_with("python3."))
                        .unwrap_or(false)
                })
            }) {
                let paths = [
                    version_dir.clone(),
                    version_dir.join("lib-dynload"),
                    version_dir.join("site-packages"),
                ];
                let joined = std::env::join_paths(paths).unwrap_or_default();
                cmd.env("PYTHONPATH", joined);
            }
        }
    } else {
        cmd = Command::new(program);
    }
    cmd
}

fn elf_has_interp(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut header = [0u8; 64];
    if file.read_exact(&mut header).is_err() {
        return false;
    }
    if &header[0..4] != b"\x7fELF" || header[4] != 2 {
        return false;
    }
    let phoff = u64::from_le_bytes(header[32..40].try_into().unwrap());
    let phentsize = u16::from_le_bytes(header[54..56].try_into().unwrap()) as usize;
    let phnum = u16::from_le_bytes(header[56..58].try_into().unwrap()) as usize;
    if phentsize < 4 || phnum == 0 || phnum > 1024 {
        return false;
    }
    if file.seek(SeekFrom::Start(phoff)).is_err() {
        return false;
    }
    let mut ph = vec![0u8; phentsize];
    for _ in 0..phnum {
        if file.read_exact(&mut ph).is_err() {
            return false;
        }
        if u32::from_le_bytes(ph[0..4].try_into().unwrap()) == 3 {
            return true;
        }
    }
    false
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
        let mut cmd = tool_command(program);
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
                    let mut stdin = child.stdin.take().context("child stdin unavailable")?;
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
        let mut cmd = tool_command(program);
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
        let mut cmd = tool_command(program);
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

/// Recursively mirror a directory using hard links (falling back to copies
/// for cross-device files).  Used when the input is an already-extracted
/// dump; this keeps the original tree untouched while giving the pipeline a
/// writable output root without duplicating gigabytes of data.
pub fn link_tree(src: &Path, dst: &Path) -> Result<()> {
    ensure_dir(dst)?;
    for entry in fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            link_tree(&path, &target)?;
        } else if file_type.is_symlink() {
            let link = fs::read_link(&path)?;
            let _ = fs::remove_file(&target);
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link, &target)?;
            #[cfg(not(unix))]
            copy_file(&path, &target)?;
        } else if file_type.is_file() {
            if target.exists() {
                continue;
            }
            if fs::hard_link(&path, &target).is_err() {
                copy_file(&path, &target)?;
            }
        }
    }
    Ok(())
}

/// Run a decompressor with a file as stdin and write its stdout directly to
/// `output`.  Avoids buffering multi-gigabyte images in memory.
pub fn stream_to_file(program: &Path, args: &[&OsStr], input: &Path, output: &Path) -> Result<()> {
    let stdin = fs::File::open(input).with_context(|| format!("opening {}", input.display()))?;
    let stdout =
        fs::File::create(output).with_context(|| format!("creating {}", output.display()))?;
    let status = tool_command(program)
        .args(args)
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("running {}", program.display()))?;
    if !status.success() {
        bail!("{} exited with {:?}", program.display(), status.code());
    }
    Ok(())
}
