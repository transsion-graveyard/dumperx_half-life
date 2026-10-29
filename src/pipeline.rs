//! End-to-end firmware dump pipeline.

use crate::archive;
use crate::bootimg::{self, BootImage};
use crate::device_tree::{self, DeviceTreeOptions};
use crate::fsimage::{self, FsType};
use crate::fstab::FstabSet;
use crate::ikconfig;
use crate::legacy;
use crate::payload;
use crate::proprietary;
use crate::props::{self, DeviceMetadata, PropStore};
use crate::ramdisk;
use crate::sdat;
use crate::sha1sum;
use crate::sparse;
use crate::superimg;
use crate::util::{self, Runner};
use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DumpOptions {
    pub input: PathBuf,
    pub out: PathBuf,
    pub keep_images: bool,
    pub verbose: bool,
    pub device_tree: bool,
    pub twrp: bool,
    pub aosp: bool,
    /// Restrict super partition extraction to these partition names.
    pub partitions: Vec<String>,
    /// Treat fstab parse errors as fatal.
    pub strict_fstab: bool,
}

const BOOT_IMAGES: &[&str] = &[
    "boot",
    "recovery",
    "vendor_boot",
    "init_boot",
    "vendor_kernel_boot",
];
const KEEP_IMAGES: &[&str] = &[
    "boot",
    "recovery",
    "vendor_boot",
    "init_boot",
    "vendor_kernel_boot",
    "dtbo",
    "tz",
    "modem",
    "vbmeta",
    "vbmeta_system",
    "vbmeta_vendor",
];

pub struct Pipeline {
    opts: DumpOptions,
    tmp: PathBuf,
    runner: Runner,
    extracted: Vec<String>,
}

impl Pipeline {
    pub fn new(opts: DumpOptions) -> Result<Self> {
        util::ensure_dir(&opts.out)?;
        let tmp = opts.out.join("tmp");
        util::remove_dir_all(&tmp);
        util::ensure_dir(&tmp)?;
        Ok(Pipeline {
            runner: Runner::new(opts.verbose),
            opts,
            tmp,
            extracted: Vec::new(),
        })
    }

    pub fn run(mut self) -> Result<()> {
        util::step(format!(
            "Dumping {} into {}",
            self.opts.input.display(),
            self.opts.out.display()
        ));
        archive::ensure_archive_support().ok();

        let staging = self.tmp.join("stage");
        util::ensure_dir(&staging)?;
        let source_root = self.import_input(&staging)?;

        let mut depth = 0;
        self.process_tree(&source_root, &staging, &mut depth)?;
        self.import_extracted_partitions(&source_root)?;

        self.finalize()?;
        util::remove_dir_all(&self.tmp);
        util::step("Dump complete");
        Ok(())
    }

    /// Get the input into a directory we can walk without modifying the
    /// user's files.  Large raw images are symlinked, not copied.
    fn import_input(&mut self, staging: &Path) -> Result<PathBuf> {
        let input = &self.opts.input;
        if !input.exists() {
            bail!("input does not exist: {}", input.display());
        }
        if input.is_dir() {
            return Ok(input.to_path_buf());
        }
        let head = util::read_head(input, 4096).unwrap_or_default();
        let name = input
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        if sparse::is_sparse(&head) {
            let raw = staging.join(format!("{}.raw", util::stem(input)));
            util::info(format!("converting sparse image {}", input.display()));
            sparse::unsparse(input, &raw)?;
            return Ok(staging.to_path_buf());
        }
        if payload::is_payload(&head) {
            util::info("OTA payload detected");
            let out = staging.join("payload");
            payload::extract(input, &out)?;
            return Ok(out);
        }
        if archive::is_archive_name(&name)
            || (name.ends_with(".tar.md5"))
            || (head.len() > 2 && head[0] == 0x1f && head[1] == 0x8b && name.contains(".tar"))
        {
            util::info(format!("extracting archive {}", input.display()));
            let out = staging.join(format!("archive-{}", util::stem(input)));
            archive::extract(input, &out)?;
            if let Some(root) = archive::single_root(&out)? {
                return Ok(root);
            }
            return Ok(out);
        }

        // Single image / firmware file: expose it through a staging symlink.
        let link = staging.join(&name);
        #[cfg(unix)]
        {
            // Prefer a hard link (no extra space).  Fall back to a symlink
            // for cross-device inputs; the walker follows file symlinks.
            if std::fs::hard_link(input, &link).is_err()
                && std::os::unix::fs::symlink(input, &link).is_err()
            {
                util::copy_file(input, &link)?;
            }
        }
        #[cfg(not(unix))]
        {
            util::copy_file(input, &link)?;
        }
        Ok(staging.to_path_buf())
    }

    /// If the input is an already-extracted dump, mirror its partition
    /// directories into the output root with hard links so metadata, fstab
    /// and proprietary-files generation work without re-extracting images.
    fn import_extracted_partitions(&self, root: &Path) -> Result<()> {
        if root == self.opts.out {
            return Ok(());
        }
        const PARTITIONS: &[&str] = &[
            "system",
            "system_ext",
            "system_other",
            "vendor",
            "odm",
            "product",
            "vendor_dlkm",
            "odm_dlkm",
            "system_dlkm",
            "my_product",
            "my_manifest",
            "my_stock",
            "my_heytap",
            "my_custom",
            "my_carrier",
            "my_region",
            "my_bigball",
            "special_preload",
            "oppo_product",
            "preload_common",
            "opproduct",
        ];
        for partition in PARTITIONS {
            let src = root.join(partition);
            let dst = self.opts.out.join(partition);
            if src.is_dir() && !dst.exists() {
                util::info(format!(
                    "mirroring extracted partition directory {}",
                    src.display()
                ));
                util::link_tree(&src, &dst)?;
            }
        }
        Ok(())
    }

    fn process_tree(&mut self, root: &Path, staging: &Path, depth: &mut usize) -> Result<()> {
        if *depth > 6 {
            bail!("input recursion too deep at {}", root.display());
        }
        *depth += 1;

        let mut files = Vec::new();
        for entry in walkdir::WalkDir::new(root).max_depth(3).follow_links(false) {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            // Follow file symlinks (a direct single-file input is exposed
            // through a staging link); do not descend into directory links.
            if entry.path().is_file() {
                files.push(entry.path().to_path_buf());
            }
        }
        files.sort();

        // Super images first: their partitions feed the rest of the stages.
        let super_files: Vec<PathBuf> = files
            .iter()
            .filter(|p| {
                let n = p.file_name().unwrap_or_default().to_string_lossy();
                n == "super.img"
                    || (n.starts_with("super") && n.ends_with(".img"))
                    || (n.contains("super") && n.contains("chunk"))
            })
            .cloned()
            .collect();
        if !super_files.is_empty() {
            self.handle_super_group(&super_files, staging)?;
        }

        // Payload and nested archives.
        for file in &files {
            let name = file
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let head = util::read_head(file, 8).unwrap_or_default();
            let is_super = super_files.iter().any(|p| p == file);
            if is_super {
                continue;
            }
            if payload::is_payload(&head) {
                let out = staging.join(format!("payload-{}", util::stem(file)));
                util::info(format!("extracting OTA payload {}", file.display()));
                if let Err(e) = payload::extract(file, &out) {
                    util::warn(format!(
                        "native payload parser failed ({e:#}); falling back to bundled payload-dumper-go"
                    ));
                    util::remove_dir_all(&out);
                    payload::extract_legacy(file, &out)?;
                }
                self.process_tree(&out, staging, depth)?;
                continue;
            }
            if archive::is_archive_name(&name) && !name.contains("super") {
                let out = staging.join(format!("archive-{}", util::stem(file)));
                util::info(format!("extracting nested archive {}", file.display()));
                archive::extract(file, &out)?;
                let next = archive::single_root(&out)?.unwrap_or(out);
                self.process_tree(&next, staging, depth)?;
            }
        }

        // A-only block OTA pairs.
        self.handle_sdat_pairs(&files, staging)?;

        // Boot images.
        for file in &files {
            let head = util::read_head(file, 8).unwrap_or_default();
            if bootimg::is_boot_image(&head) {
                let base = util::base_partition(&util::stem(file)).to_string();
                if BOOT_IMAGES.contains(&base.as_str()) {
                    self.handle_boot(file, &base)?;
                } else {
                    self.handle_boot(file, &base)?;
                }
            }
        }

        // Vendor-specific formats (vendored offline tools; Rust ports follow).
        for file in &files {
            let head = util::read_head(file, 16).unwrap_or_default();
            if let Some(format) = legacy::detect(file, &head) {
                let sub = staging.join(format!("legacy-{}", util::stem(file)));
                util::remove_dir_all(&sub);
                legacy::extract(format, file, &sub, staging)?;
                self.process_tree(&sub, staging, depth)?;
            }
        }

        // dtbo.
        for file in &files {
            if util::base_partition(&util::stem(file)) == "dtbo" {
                self.handle_dtbo(file)?;
            }
        }

        // Raw partition images.
        for file in &files {
            if !file.is_file() {
                continue;
            }
            let name = file
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            if !util::is_image_name(&name) {
                continue;
            }
            let base = util::base_partition(&util::stem(file)).to_string();
            if base == "super" || base == "dtbo" || BOOT_IMAGES.contains(&base.as_str()) {
                continue;
            }
            if super_files.iter().any(|p| p == file) {
                continue;
            }
            // Skip files that were already imported into out.
            let dest = self.opts.out.join(format!("{base}.img"));
            if file != &dest {
                if dest.exists() {
                    continue;
                }
                util::copy_file(file, &dest)?;
            }
        }
        *depth -= 1;
        Ok(())
    }

    fn handle_super_group(&mut self, files: &[PathBuf], staging: &Path) -> Result<()> {
        util::step("Extracting super image");
        let primary = files
            .iter()
            .find(|p| p.file_name().map(|n| n == "super.img").unwrap_or(false))
            .cloned()
            .unwrap_or_else(|| files[0].clone());

        // Super split into sparse chunks: concatenate then unsparse.
        let mut raw = primary.clone();
        let head = util::read_head(&primary, 4096).unwrap_or_default();
        if files.len() > 1
            && files.iter().all(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().contains("chunk"))
                    .unwrap_or(false)
            })
        {
            let combined = staging.join("super.chunks");
            util::info(format!("concatenating {} super chunks", files.len()));
            concat_files(files, &combined)?;
            raw = staging.join("super.raw");
            sparse::unsparse(&combined, &raw)?;
            let _ = fs::remove_file(&combined);
        } else if sparse::is_sparse(&head) {
            raw = staging.join("super.raw");
            sparse::unsparse(&primary, &raw)?;
        }

        let mut file = fs::File::open(&raw)?;
        if !superimg::is_super_image(&util::read_head(&raw, 8192)?) {
            // Not actually a super image; let the normal image path handle it.
            let dest = self.opts.out.join("super.img");
            if raw != dest {
                util::copy_file(&raw, &dest)?;
            }
            return Ok(());
        }
        let meta = superimg::read_metadata(&mut file)?;
        util::info(format!(
            "liblp metadata {}.{}: {} partitions",
            meta.major,
            meta.minor,
            meta.partitions.len()
        ));
        let results = superimg::extract_filtered(
            &raw,
            &self.opts.out,
            superimg::SlotMode::Auto,
            &self.opts.partitions,
            false,
        )?;
        let partition_infos = superimg::partition_infos(&meta);
        let super_info = serde_json::json!({
            "version": format!("{}.{}", meta.major, meta.minor),
            "size": fs::metadata(&raw).map(|m| m.len()).unwrap_or(0),
            "groups": meta.groups,
            "partitions": partition_infos,
        });
        fs::write(
            self.opts.out.join("super-info.json"),
            serde_json::to_string_pretty(&super_info)?,
        )?;
        for (name, path, size) in &results {
            util::info(format!(
                "  {name} -> {} ({})",
                path.display(),
                util::human_size(*size)
            ));
        }
        // Clean up temporary raw image; never delete a user-provided file.
        if raw.starts_with(staging) {
            let _ = fs::remove_file(&raw);
        }
        Ok(())
    }

    fn handle_sdat_pairs(&mut self, files: &[PathBuf], staging: &Path) -> Result<()> {
        for transfer in files {
            let name = transfer.file_name().unwrap_or_default().to_string_lossy();
            if !name.ends_with(".transfer.list") {
                continue;
            }
            let partition = name.trim_end_matches(".transfer.list");
            let dir = transfer.parent().unwrap_or(Path::new("."));
            let dat = ["new.dat", "new.dat.br", "new.dat.xz", "new.dat.bz2"]
                .iter()
                .map(|suffix| dir.join(format!("{partition}.{suffix}")))
                .find(|p| p.exists());
            let Some(dat) = dat else {
                util::warn(format!(
                    "{} has no matching .new.dat file; skipping",
                    transfer.display()
                ));
                continue;
            };
            util::step(format!("sdat2img: {partition}"));
            let plain_dat = if dat.extension().map(|e| e == "dat").unwrap_or(false) {
                dat.clone()
            } else {
                let out = staging.join(format!("{partition}.new.dat"));
                decompress_file(&dat, &out)?;
                out
            };
            let image = self.opts.out.join(format!("{partition}.img"));
            sdat::sdat2img(transfer, &plain_dat, &image)?;
            if plain_dat != dat && plain_dat.starts_with(staging) {
                let _ = fs::remove_file(&plain_dat);
            }
        }
        Ok(())
    }

    fn handle_boot(&mut self, file: &Path, base: &str) -> Result<()> {
        let dest = self.opts.out.join(format!("{base}.img"));
        if file != dest {
            util::copy_file(file, &dest)?;
        }
        util::step(format!("Unpacking {base} image"));
        let image = match BootImage::parse(&dest) {
            Ok(image) => image,
            Err(e) => {
                util::warn(format!(
                    "{}: cannot parse boot header ({e:#}); keeping the image",
                    dest.display()
                ));
                return Ok(());
            }
        };
        let image_dir = self.opts.out.join(base);
        let raw_dir = image_dir.join("raw");
        util::remove_dir_all(&image_dir);
        util::ensure_dir(&raw_dir)?;
        let files = image.unpack(&raw_dir)?;
        let info = image.info();

        // Ramdisk(s).
        let mut ramdisk_note = String::new();
        match &image {
            BootImage::Boot { .. } => {
                let ramdisk_file = raw_dir.join("ramdisk");
                if ramdisk_file.is_file() && fs::metadata(&ramdisk_file)?.len() > 0 {
                    let bytes = fs::read(&ramdisk_file)?;
                    match ramdisk::extract(&bytes, &image_dir.join("ramdisk")) {
                        Ok(stats) => {
                            ramdisk_note = format!(
                                "ramdisk: {} files, {} dirs, {} symlinks, {} skipped\n",
                                stats.files, stats.dirs, stats.symlinks, stats.skipped
                            );
                        }
                        Err(e) => {
                            ramdisk_note = format!("ramdisk extraction failed: {e:#}\n");
                            util::warn(&ramdisk_note);
                        }
                    }
                } else {
                    ramdisk_note =
                        "ramdisk: none in this image (ramdisk may live in vendor_boot/init_boot)\n"
                            .to_string();
                    util::info(format!("{base}.img has no ramdisk"));
                }
            }
            BootImage::VendorBoot { header, .. } => {
                if header.header_version > 3 {
                    for i in 0..header.ramdisk_table.len() {
                        let file = raw_dir.join(format!("vendor_ramdisk{i:02}"));
                        if !file.is_file() {
                            continue;
                        }
                        let label = if header.ramdisk_table[i].name.is_empty() {
                            format!("vendor_ramdisk{i:02}")
                        } else {
                            header.ramdisk_table[i].name.clone()
                        };
                        let out = image_dir.join("ramdisk").join(&label);
                        let bytes = fs::read(&file)?;
                        match ramdisk::extract(&bytes, &out) {
                            Ok(stats) => {
                                ramdisk_note.push_str(&format!(
                                    "ramdisk {label}: {} files, {} dirs, {} symlinks\n",
                                    stats.files, stats.dirs, stats.symlinks
                                ));
                            }
                            Err(e) => {
                                ramdisk_note.push_str(&format!(
                                    "ramdisk {label} extraction failed: {e:#}\n"
                                ));
                                util::warn(format!("ramdisk {label}: {e:#}"));
                            }
                        }
                    }
                } else {
                    let file = raw_dir.join("vendor_ramdisk");
                    if file.is_file() {
                        let bytes = fs::read(&file)?;
                        match ramdisk::extract(&bytes, &image_dir.join("ramdisk")) {
                            Ok(stats) => {
                                ramdisk_note = format!(
                                    "ramdisk: {} files, {} dirs, {} symlinks\n",
                                    stats.files, stats.dirs, stats.symlinks
                                );
                            }
                            Err(e) => {
                                ramdisk_note = format!("ramdisk extraction failed: {e:#}\n");
                                util::warn(&ramdisk_note);
                            }
                        }
                    }
                }
            }
        }

        // Device tree blobs inside the boot image.
        self.handle_dtb_file(&raw_dir.join("dtb"), &image_dir.join("dtb"))?;

        let mut info_text = info;
        info_text.push_str(&ramdisk_note);
        info_text.push_str("\nextracted files:\n");
        for f in &files {
            info_text.push_str(&format!("  {} ({} bytes)\n", f.name, f.size));
        }
        if let Ok(entries) = fs::read_dir(&raw_dir) {
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            for name in &names {
                info_text.push_str(&format!("raw/{name}\n"));
            }
        }
        fs::write(image_dir.join("info.txt"), info_text)?;
        Ok(())
    }

    fn handle_dtbo(&mut self, file: &Path) -> Result<()> {
        let dest = self.opts.out.join("dtbo.img");
        if file != dest {
            util::copy_file(file, &dest)?;
        }
        util::step("Splitting dtbo image");
        let dtb_dir = self.opts.out.join("dtbo").join("dtb");
        util::remove_dir_all(&self.opts.out.join("dtbo"));
        let files = bootimg::split_dtbo(&dest, &dtb_dir)?;
        util::info(format!("extracted {} device tree blob(s)", files.len()));
        self.decompile_dtbs(&dtb_dir, &self.opts.out.join("dtbo").join("dts"))?;
        Ok(())
    }

    fn handle_dtb_file(&mut self, dtb: &Path, out_dir: &Path) -> Result<()> {
        if !dtb.is_file() || fs::metadata(dtb)?.len() == 0 {
            return Ok(());
        }
        util::ensure_dir(out_dir)?;
        let files = bootimg::split_dtbo(dtb, out_dir)?;
        if !files.is_empty() {
            util::info(format!(
                "extracted {} device tree blob(s) from {}",
                files.len(),
                dtb.display()
            ));
            let dts = out_dir.parent().unwrap_or(out_dir).join("dts");
            self.decompile_dtbs(out_dir, &dts)?;
        }
        Ok(())
    }

    fn decompile_dtbs(&self, dtb_dir: &Path, dts_dir: &Path) -> Result<()> {
        let Some(dtc) = bootimg::vendored_dtc() else {
            util::warn("dtc not found; keeping .dtb files without .dts");
            return Ok(());
        };
        util::ensure_dir(dts_dir)?;
        let mut dtbs = Vec::new();
        if let Ok(entries) = fs::read_dir(dtb_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "dtb").unwrap_or(false) {
                    dtbs.push(path);
                }
            }
        }
        dtbs.sort();
        for dtb in dtbs {
            let name = dtb
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .replace(".dtb", ".dts");
            let out = dts_dir.join(name);
            let output = self.runner.capture(
                &dtc,
                &[
                    OsStr::new("-q"),
                    OsStr::new("-I"),
                    OsStr::new("dtb"),
                    OsStr::new("-O"),
                    OsStr::new("dts"),
                    dtb.as_os_str(),
                ],
                None,
            )?;
            if output.status.success() {
                fs::write(&out, &output.stdout)?;
            } else {
                util::warn(format!(
                    "dtc failed for {}: {}",
                    dtb.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }
        Ok(())
    }

    fn extract_filesystems(&mut self) -> Result<()> {
        util::step("Extracting filesystems");
        let fstab = FstabSet::scan(&self.opts.out).ok();
        let hints = fstab
            .as_ref()
            .map(|f| f.fs_candidates())
            .unwrap_or_default();
        let mut images = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.opts.out) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "img").unwrap_or(false) {
                    images.push(path);
                }
            }
        }
        images.sort();
        for image in images {
            let name = util::base_partition(&util::stem(&image)).to_string();
            if KEEP_IMAGES.contains(&name.as_str()) || name == "super" {
                continue;
            }
            let out_dir = self.opts.out.join(&name);
            let hint = hints.get(&name).and_then(|v| {
                v.iter().find_map(|fs| match fs.as_str() {
                    "erofs" => Some(FsType::Erofs),
                    "ext4" => Some(FsType::Ext4),
                    "f2fs" => Some(FsType::F2fs),
                    _ => None,
                })
            });
            let info = fsimage::detect(&image).unwrap_or(fsimage::FsInfo {
                fs_type: FsType::Unknown,
                header_offset: 0,
            });
            if info.fs_type == FsType::Unknown && hint.is_none() {
                util::warn(format!(
                    "{}: no known filesystem detected; keeping image",
                    image.display()
                ));
                continue;
            }
            util::info(format!(
                "{}: extracting {} ({})",
                image.display(),
                info.fs_type.name(),
                util::human_size(fs::metadata(&image)?.len())
            ));
            // Avoid mixing files from a previous run into a re-extracted
            // partition.
            util::remove_dir_all(&out_dir);
            match fsimage::extract(&image, &out_dir, hint) {
                Ok(fs_type) => {
                    self.extracted.push(name.clone());
                    util::info(format!("  extracted as {}", fs_type.name()));
                    if !self.opts.keep_images {
                        let _ = fs::remove_file(&image);
                    }
                }
                Err(e) => {
                    util::warn(format!("{}: {e:#}", image.display()));
                }
            }
        }
        Ok(())
    }

    fn check_fstab(&self) -> Result<()> {
        util::step("Parsing fstab files");
        let set = FstabSet::scan(&self.opts.out)?;
        if set.files.is_empty() {
            util::warn("no fstab files found in the dump");
            return Ok(());
        }
        let report = self.opts.out.join("fstab-report.json");
        fs::write(&report, set.json()?)?;
        let entries: usize = set.files.iter().map(|f| f.entries.len()).sum();
        let warnings: usize = set.files.iter().map(|f| f.warnings.len()).sum();
        let errors: Vec<String> = set
            .errors()
            .map(|e| format!("line {}: {}", e.line, e.message))
            .collect();
        util::info(format!(
            "{} fstab file(s), {entries} entries, {warnings} warning(s), {} error(s)",
            set.files.len(),
            errors.len()
        ));
        for e in &errors {
            util::error(format!("fstab: {e}"));
        }
        if !errors.is_empty() {
            util::warn(
                "fstab parsing errors were reported above (they are no longer silently ignored)",
            );
            if self.opts.strict_fstab {
                bail!(
                    "fstab parsing failed with {} error(s) and --strict-fstab was requested",
                    errors.len()
                );
            }
        }
        // Cross-check logical partitions declared by fstab against what we
        // actually extracted.
        for (partition, candidates) in set.fs_candidates() {
            let dir = self.opts.out.join(&partition);
            let image = self.opts.out.join(format!("{partition}.img"));
            if !dir.exists() && !image.exists() {
                util::warn(format!(
                    "fstab declares logical partition '{partition}' ({}) but it is not in the dump",
                    candidates.join("/")
                ));
            }
        }
        Ok(())
    }

    fn write_metadata(&self) -> Result<DeviceMetadata> {
        util::step("Reading build properties");
        let store = PropStore::load(&self.opts.out)?;
        if store.files.is_empty() {
            util::warn("no build.prop files found; metadata will be incomplete");
        }
        let kernel_version = kernel_version_from(&self.opts.out);
        let meta = props::extract_metadata(&store, kernel_version.as_deref());
        util::info(format!(
            "device: {} ({}) / {} / Android {}",
            meta.codename, meta.brand, meta.platform, meta.release
        ));
        let readme = format!(
            "## {}\n- Manufacturer: {}\n- Platform: {}\n- Codename: {}\n- Brand: {}\n- Flavor: {}\n- Release Version: {}\n- Kernel Version: {}\n- Id: {}\n- Incremental: {}\n- Tags: {}\n- CPU Abilist: {}\n- A/B Device: {}\n- Treble Device: {}\n- Locale: {}\n- Screen Density: {}\n- Fingerprint: {}\n- OTA version: {}\n- Branch: {}\n- Repo: {}\n",
            meta.description,
            meta.manufacturer,
            meta.platform,
            meta.codename,
            meta.brand,
            meta.flavor,
            meta.release,
            meta.kernel_version,
            meta.id,
            meta.incremental,
            meta.tags,
            meta.abilist,
            meta.is_ab,
            meta.treble_support,
            meta.locale,
            meta.density,
            meta.fingerprint,
            meta.otaver,
            meta.branch,
            meta.repo
        );
        fs::write(self.opts.out.join("README.md"), &readme)?;
        print!("{readme}");
        Ok(meta)
    }

    fn finalize(&mut self) -> Result<()> {
        self.extract_filesystems()?;
        self.check_fstab()?;

        // ikconfig from the kernel inside boot (or vendor_kernel_boot).
        let kernel_candidates = [
            self.opts.out.join("boot/raw/kernel"),
            self.opts.out.join("vendor_kernel_boot/raw/kernel"),
        ];
        for kernel in &kernel_candidates {
            if kernel.is_file() {
                let dest = self.opts.out.join("ikconfig");
                match ikconfig::extract_to_file(kernel, &dest) {
                    Ok(true) => {
                        util::info("extracted ikconfig");
                        break;
                    }
                    Ok(false) => {
                        util::info("kernel has no embedded IKCONFIG");
                    }
                    Err(e) => util::warn(format!("ikconfig extraction failed: {e:#}")),
                }
            }
        }

        let meta = self.write_metadata()?;
        proprietary::generate(&self.opts.out, &meta, &meta.description)?;

        if self.opts.device_tree {
            util::step("Generating device trees");
            let do_twrp = self.opts.twrp || !self.opts.aosp;
            let do_aosp = self.opts.aosp || !self.opts.twrp;
            device_tree::generate(
                &self.opts.out,
                &DeviceTreeOptions {
                    twrp: do_twrp,
                    aosp: do_aosp,
                },
            )?;
        }

        util::step("Generating manifests");
        sha1sum::write_all_files(&self.opts.out)?;
        let all_files = self.opts.out.join("all_files.txt");
        let all_sha1 = self.opts.out.join("all_files.sha1");
        sha1sum::write_sha1_manifest(&self.opts.out, &all_files, &all_sha1)?;
        util::info(format!(
            "wrote {} and {}",
            all_files.display(),
            all_sha1.display()
        ));
        Ok(())
    }
}

fn concat_files(files: &[PathBuf], dest: &Path) -> Result<()> {
    use std::io::Write;
    let mut out = fs::File::create(dest)?;
    for file in files {
        let mut input = fs::File::open(file)?;
        std::io::copy(&mut input, &mut out)?;
    }
    out.flush()?;
    Ok(())
}

fn decompress_file(input: &Path, output: &Path) -> Result<()> {
    let data = fs::read(input)?;
    let name = input.file_name().unwrap_or_default().to_string_lossy();
    let (_, plain) = if name.ends_with(".br") {
        (ramdisk::Compression::Unknown, run_brotli(&data)?)
    } else {
        ramdisk::decompress(&data)?
    };
    fs::write(output, plain)?;
    Ok(())
}

fn run_brotli(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let tool = util::find_tool("brotli").context("brotli not found in PATH")?;
    let mut child = Command::new(tool)
        .args(["-d", "-c"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().context("brotli stdin unavailable")?;
    let input = data.to_vec();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        stdin.write_all(&input)?;
        Ok(())
    });
    let out = child.wait_with_output()?;
    let _ = writer.join();
    if !out.status.success() {
        bail!(
            "brotli failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn kernel_version_from(out: &Path) -> Option<String> {
    let ikconfig = out.join("ikconfig");
    if let Ok(text) = fs::read_to_string(&ikconfig) {
        for line in text.lines() {
            if line.contains("Kernel Configuration") {
                // e.g. "# Linux/arm64 6.1.0 Kernel Configuration"
                return line.split_whitespace().nth(2).map(str::to_string);
            }
        }
    }
    None
}
