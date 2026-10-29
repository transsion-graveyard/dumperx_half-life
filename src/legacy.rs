//! Bridge for vendor-specific firmware formats that are not ported to Rust
//! yet.  Every tool used here is vendored in the repository; nothing is
//! downloaded at runtime.  These paths will be replaced by native parsers
//! incrementally.

use crate::util::{self, Runner};
use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyFormat {
    Ozip,
    Ofp,
    Ops,
    Kdz,
    Ruu,
    Pac,
    Nb0,
    Sin,
    UpdateApp,
    Rockchip,
    Amlogic,
}

impl LegacyFormat {
    pub fn name(self) -> &'static str {
        match self {
            LegacyFormat::Ozip => "oppo ozip",
            LegacyFormat::Ofp => "oppo ofp",
            LegacyFormat::Ops => "oppo/oneplus ops",
            LegacyFormat::Kdz => "LG kdz",
            LegacyFormat::Ruu => "HTC RUU",
            LegacyFormat::Pac => "spreadtrum pac",
            LegacyFormat::Nb0 => "nb0",
            LegacyFormat::Sin => "sony sin",
            LegacyFormat::UpdateApp => "huawei UPDATE.APP",
            LegacyFormat::Rockchip => "rockchip",
            LegacyFormat::Amlogic => "amlogic",
        }
    }
}

pub fn detect(path: &Path, head: &[u8]) -> Option<LegacyFormat> {
    let name = path.file_name()?.to_string_lossy();
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    if lower.ends_with(".ozip") || head.starts_with(b"OPPOENCRYPT!") {
        return Some(LegacyFormat::Ozip);
    }
    if lower.ends_with(".ofp") {
        return Some(LegacyFormat::Ofp);
    }
    if lower.ends_with(".ops") {
        return Some(LegacyFormat::Ops);
    }
    if lower.ends_with(".kdz") {
        return Some(LegacyFormat::Kdz);
    }
    if lower.starts_with("ruu_") && ext == "exe" {
        return Some(LegacyFormat::Ruu);
    }
    if lower.ends_with(".pac") {
        return Some(LegacyFormat::Pac);
    }
    if lower.ends_with(".nb0") {
        return Some(LegacyFormat::Nb0);
    }
    if lower.ends_with(".sin") {
        return Some(LegacyFormat::Sin);
    }
    if name == "UPDATE.APP" {
        return Some(LegacyFormat::UpdateApp);
    }
    if name.to_ascii_lowercase().contains("rockchip") {
        return Some(LegacyFormat::Rockchip);
    }
    if name.to_ascii_lowercase().contains("aml") && util::is_image_name(&name) {
        return Some(LegacyFormat::Amlogic);
    }
    None
}

fn python() -> Result<PathBuf> {
    util::find_tool("python3").context(
        "python3 is required for legacy vendor firmware formats (it is already installed by setup.sh)",
    )
}

fn script(relative: &str) -> Result<PathBuf> {
    let path = util::utils_dir().join(relative);
    if !path.is_file() {
        bail!("vendored legacy script is missing: {}", path.display());
    }
    Ok(path)
}

fn run_python(script_rel: &str, args: &[&OsStr], cwd: &Path) -> Result<()> {
    let python = python()?;
    let script = script(script_rel)?;
    let runner = Runner::new(false);
    let mut all = vec![script.as_os_str()];
    all.extend_from_slice(args);
    runner
        .run(&python, &all, Some(cwd))
        .with_context(|| format!("running legacy script {}", script.display()))
}

/// Extract a legacy firmware file into `out`.  The caller should recursively
/// process the produced images.
pub fn extract(format: LegacyFormat, file: &Path, out: &Path, tmp: &Path) -> Result<()> {
    util::ensure_dir(out)?;
    util::info(format!(
        "legacy {} extractor: {}",
        format.name(),
        file.display()
    ));
    match format {
        LegacyFormat::Ozip => extract_ozip(file, out, tmp),
        LegacyFormat::Ofp => extract_ofp(file, out, tmp),
        LegacyFormat::Ops => extract_ops(file, out, tmp),
        LegacyFormat::Kdz => extract_kdz(file, out, tmp),
        LegacyFormat::Ruu => extract_ruu(file, out, tmp),
        LegacyFormat::Pac => extract_pac(file, out, tmp),
        LegacyFormat::Nb0 => extract_nb0(file, out, tmp),
        LegacyFormat::Sin => extract_sin(file, out, tmp),
        LegacyFormat::UpdateApp => extract_update_app(file, out, tmp),
        LegacyFormat::Rockchip => extract_rockchip(file, out, tmp),
        LegacyFormat::Amlogic => extract_aml(file, out, tmp),
    }
}

fn stage_copy(file: &Path, tmp: &Path) -> Result<PathBuf> {
    util::ensure_dir(tmp)?;
    let dest = tmp.join(file.file_name().unwrap_or_default());
    util::copy_file(file, &dest)?;
    Ok(dest)
}

fn move_images(from: &Path, to: &Path) -> Result<usize> {
    util::ensure_dir(to)?;
    let mut count = 0;
    for entry in walkdir::WalkDir::new(from).follow_links(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if !(name.ends_with(".img") || name.ends_with(".bin") || name.ends_with(".image")) {
            continue;
        }
        let mut target_name = entry.file_name().to_string_lossy().into_owned();
        if target_name.ends_with(".image") {
            target_name = target_name.replace(".image", ".img");
        }
        let base = util::base_partition(&util::stem(Path::new(&target_name))).to_string();
        let dest = to.join(format!("{base}.img"));
        if dest.exists() {
            continue;
        }
        util::copy_file(entry.path(), &dest)?;
        count += 1;
    }
    Ok(count)
}

fn extract_ozip(file: &Path, out: &Path, tmp: &Path) -> Result<()> {
    let work = tmp.join("ozip");
    let staged = stage_copy(file, &work)?;
    // ozipdecrypt writes <name>.zip in the current directory.
    let python = python()?;
    let script = script("vendor/oppo_ozip_decrypt/ozipdecrypt.py")?;
    Runner::new(false)
        .run(
            &python,
            &[script.as_os_str(), staged.as_os_str()],
            Some(&work),
        )
        .context("ozip decryption failed")?;
    let zip = staged.with_extension("zip");
    if zip.is_file() {
        crate::archive::extract(&zip, out)?;
        return Ok(());
    }
    for candidate in ["out", "extract"] {
        let dir = work.join(candidate);
        if dir.is_dir() {
            move_images(&dir, out)?;
            return Ok(());
        }
    }
    bail!("ozip decryption produced no zip/extracted directory")
}

fn extract_ofp(file: &Path, out: &Path, tmp: &Path) -> Result<()> {
    let work = tmp.join("ofp");
    let staged = stage_copy(file, &work)?;
    let target = work.join("out");
    util::ensure_dir(&target)?;
    let python = python()?;
    let qc = script("vendor/oppo_decrypt/ofp_qc_decrypt.py")?;
    let mtk = script("vendor/oppo_decrypt/ofp_mtk_decrypt.py")?;
    let runner = Runner::new(false);
    if runner
        .run(
            &python,
            &[qc.as_os_str(), staged.as_os_str(), target.as_os_str()],
            Some(&work),
        )
        .is_err()
        || !target.join("boot.img").is_file()
    {
        util::warn("OFP QC decryption failed; trying MTK");
        runner.run(
            &python,
            &[mtk.as_os_str(), staged.as_os_str(), target.as_os_str()],
            Some(&work),
        )?;
    }
    if move_images(&target, out)? == 0 {
        bail!("OFP decryption produced no images");
    }
    Ok(())
}

fn extract_ops(file: &Path, out: &Path, tmp: &Path) -> Result<()> {
    let work = tmp.join("ops");
    let staged = stage_copy(file, &work)?;
    let python = python()?;
    let script = script("vendor/oppo_decrypt/opscrypto.py")?;
    Runner::new(false)
        .run(
            &python,
            &[
                script.as_os_str(),
                OsStr::new("decrypt"),
                staged.as_os_str(),
            ],
            Some(&work),
        )
        .context("ops decryption failed")?;
    let extract = work.join("extract");
    if move_images(&extract, out)? == 0 {
        bail!("ops decryption produced no images");
    }
    Ok(())
}

fn extract_kdz(file: &Path, out: &Path, tmp: &Path) -> Result<()> {
    let work = tmp.join("kdz");
    util::ensure_dir(&work)?;
    let staged = stage_copy(file, &work)?;
    let python = python()?;
    let unkdz = script("kdztools/unkdz.py")?;
    let undz = script("kdztools/undz.py")?;
    let runner = Runner::new(false);
    runner.run(
        &python,
        &[
            unkdz.as_os_str(),
            OsStr::new("-f"),
            staged.as_os_str(),
            OsStr::new("-x"),
            OsStr::new("-o"),
            work.as_os_str(),
        ],
        Some(&work),
    )?;
    let mut dz = None;
    if let Ok(entries) = fs::read_dir(&work) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "dz").unwrap_or(false) {
                dz = Some(path);
                break;
            }
        }
    }
    let dz = dz.context("unkdz produced no .dz file")?;
    runner.run(
        &python,
        &[
            undz.as_os_str(),
            OsStr::new("-f"),
            dz.as_os_str(),
            OsStr::new("-s"),
            OsStr::new("-o"),
            work.as_os_str(),
        ],
        Some(&work),
    )?;
    move_images(&work, out)?;
    Ok(())
}

fn extract_ruu(file: &Path, out: &Path, tmp: &Path) -> Result<()> {
    let work = tmp.join("ruu");
    let staged = stage_copy(file, &work)?;
    let tool = script_bin("RUU_Decrypt_Tool")?;
    let runner = Runner::new(false);
    let cwd = work.clone();
    runner.run(&tool, &[OsStr::new("-s"), staged.as_os_str()], Some(&cwd))?;
    runner.run(&tool, &[OsStr::new("-f"), staged.as_os_str()], Some(&cwd))?;
    move_images(&work, out)?;
    Ok(())
}

fn extract_pac(file: &Path, out: &Path, _tmp: &Path) -> Result<()> {
    crate::pac::extract(file, out)?;
    Ok(())
}

fn extract_nb0(file: &Path, out: &Path, tmp: &Path) -> Result<()> {
    let work = tmp.join("nb0");
    let staged = stage_copy(file, &work)?;
    let tool = script_bin("nb0-extract")?;
    Runner::new(false).run(&tool, &[staged.as_os_str(), out.as_os_str()], Some(&work))?;
    Ok(())
}

fn extract_sin(file: &Path, out: &Path, tmp: &Path) -> Result<()> {
    let work = tmp.join("sin");
    util::ensure_dir(&work)?;
    let staged = stage_copy(file, &work)?;
    // unsin expects the .sin files to be renamed to <partition>.sin and
    // unpacked in one directory.
    let name = staged.file_name().unwrap_or_default().to_string_lossy();
    let partition = name.split('_').next().unwrap_or("system");
    let renamed = work.join(format!("{partition}.sin"));
    if renamed != staged {
        util::move_or_copy(&staged, &renamed)?;
    }
    let tool = script_bin("unsin")?;
    Runner::new(false).run(&tool, &[OsStr::new("-d"), work.as_os_str()], Some(&work))?;
    move_images(&work, out)?;
    Ok(())
}

fn extract_update_app(file: &Path, out: &Path, _tmp: &Path) -> Result<()> {
    crate::uapp::extract(file, out, &[])?;
    Ok(())
}

fn extract_rockchip(file: &Path, out: &Path, tmp: &Path) -> Result<()> {
    let work = tmp.join("rockchip");
    util::ensure_dir(&work)?;
    let rk = util::require_tool("rkImageMaker")?;
    let afp = util::require_tool("afptool")?;
    let runner = Runner::new(false);
    runner.run(
        &rk,
        &[OsStr::new("-unpack"), file.as_os_str(), work.as_os_str()],
        Some(&work),
    )?;
    let firmware = work.join("firmware.img");
    if firmware.is_file() {
        runner.run(
            &afp,
            &[
                OsStr::new("-unpack"),
                firmware.as_os_str(),
                work.as_os_str(),
            ],
            Some(&work),
        )?;
    }
    move_images(&work, out)?;
    Ok(())
}

fn extract_aml(file: &Path, out: &Path, tmp: &Path) -> Result<()> {
    let work = tmp.join("aml");
    util::ensure_dir(&work)?;
    let tool = script_bin("aml-upgrade-package-extract")?;
    Runner::new(false).run(&tool, &[file.as_os_str()], Some(&work))?;
    move_images(&work, out)?;
    Ok(())
}

fn script_bin(name: &str) -> Result<PathBuf> {
    let path = util::utils_dir().join(name);
    if path.is_file() {
        Ok(path)
    } else {
        util::require_tool(name)
    }
}
