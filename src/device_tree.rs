//! Native device-tree generation (TWRP and AOSP/LineageOS style).
//!
//! This replaces the Python `twrpdtgen`/`aospdtgen` invocation.  The
//! important behavioural difference is image selection: every available
//! image (boot / vendor_boot / init_boot / recovery / vendor_kernel_boot) is
//! inspected, and the generator reports exactly which one supplied the
//! kernel, ramdisk and DTB.  It never asserts on a missing ramdisk and it
//! never silently skips a missing fstab.

use crate::bootimg::BootImage;
use crate::fstab::{FstabEntry, FstabFile, FstabSet};
use crate::props::{self, DeviceMetadata, PropStore};
use crate::util;
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct DeviceTreeOptions {
    pub twrp: bool,
    pub aosp: bool,
}

#[derive(Debug, Clone, Default)]
struct SelectedImages {
    kernel: Option<PathBuf>,
    kernel_source: String,
    ramdisk_dirs: Vec<(String, PathBuf)>,
    dtb: Option<PathBuf>,
    dtb_source: String,
    dtbo: Option<PathBuf>,
    header_version: u32,
    base: u64,
    pagesize: u32,
    cmdline: String,
    os_version: String,
    boot_size: u64,
    recovery_size: u64,
    init_boot_size: u64,
    vendor_boot_size: u64,
    vendor_kernel_boot_size: u64,
    dtbo_size: u64,
    super_size: u64,
    super_group: String,
    super_group_size: u64,
    dynamic_partitions: Vec<String>,
}

pub fn generate(dump_root: &Path, opts: &DeviceTreeOptions) -> Result<()> {
    if !opts.twrp && !opts.aosp {
        return Ok(());
    }
    let props = PropStore::load(dump_root)?;
    let kernel_version = kernel_version(dump_root);
    let meta = props::extract_metadata(&props, kernel_version.as_deref());
    if meta.codename.is_empty() || meta.manufacturer.is_empty() {
        util::warn(
            "device-tree: codename/manufacturer missing from build.prop; using dump directory name",
        );
    }
    let manufacturer = if meta.manufacturer.is_empty() {
        "unknown".to_string()
    } else {
        meta.manufacturer.clone()
    };
    let detected_codename = if meta.codename.is_empty() {
        dump_root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "device".to_string())
    } else {
        meta.codename.clone()
    };
    // AOSP device names cannot contain '-'.  If the detected value has one
    // (for example TECNO-CM6 -> tecno-cm6), use the part after the last '-'
    // as the device identifier.  Values without a dash are kept unchanged.
    let codename = aosp_device_name(&detected_codename);
    if codename != detected_codename {
        util::info(format!(
            "device-tree: using AOSP device name '{}' instead of detected '{}'",
            codename, detected_codename
        ));
    }

    let fstab_set = FstabSet::scan(dump_root)?;
    if fstab_set.files.is_empty() {
        util::warn("device-tree: no fstab found in the dump or ramdisks");
    }
    let fstab = pick_fstab(&fstab_set);
    let images = select_images(dump_root, &fstab_set)?;
    report_selection(&images);

    let device = DeviceContext {
        dump_root,
        meta: &meta,
        manufacturer: &manufacturer,
        codename: &codename,
        detected_codename: &detected_codename,
        fstab,
        fstab_set: &fstab_set,
        images: &images,
    };

    if opts.aosp {
        let root = dump_root.join("aosp-device-tree");
        util::remove_dir_all(&root);
        generate_aosp(&root, &device)?;
        util::info(format!("AOSP device tree: {}", root.display()));
    }
    if opts.twrp {
        let root = dump_root
            .join("twrp-device-tree")
            .join(&manufacturer)
            .join(&codename);
        util::remove_dir_all(&root);
        generate_twrp(&root, &device)?;
        util::info(format!("TWRP device tree: {}", root.display()));
    }
    Ok(())
}

struct DeviceContext<'a> {
    dump_root: &'a Path,
    meta: &'a DeviceMetadata,
    manufacturer: &'a str,
    codename: &'a str,
    detected_codename: &'a str,
    fstab: Option<&'a FstabFile>,
    fstab_set: &'a FstabSet,
    images: &'a SelectedImages,
}

fn report_selection(images: &SelectedImages) {
    util::info("device-tree image selection:");
    match &images.kernel {
        Some(path) => util::info(format!(
            "  kernel: {} ({})",
            path.display(),
            images.kernel_source
        )),
        None => util::warn("  kernel: not found in boot or vendor_kernel_boot"),
    }
    if images.ramdisk_dirs.is_empty() {
        util::warn("  ramdisk: not found in boot/init_boot/vendor_boot/recovery");
    } else {
        for (source, path) in &images.ramdisk_dirs {
            util::info(format!("  ramdisk: {} ({})", path.display(), source));
        }
    }
    match &images.dtb {
        Some(path) => util::info(format!("  dtb: {} ({})", path.display(), images.dtb_source)),
        None => util::info("  dtb: not present in boot/vendor_boot"),
    }
    match &images.dtbo {
        Some(path) => util::info(format!("  dtbo: {}", path.display())),
        None => util::info("  dtbo: not present"),
    }
    util::info(format!(
        "  header version: {}, base: {:#x}, pagesize: {}, cmdline: '{}'",
        images.header_version, images.base, images.pagesize, images.cmdline
    ));
    if !images.dynamic_partitions.is_empty() {
        util::info(format!(
            "  dynamic partitions: {}",
            images.dynamic_partitions.join(", ")
        ));
    }
}

fn kernel_version(dump_root: &Path) -> Option<String> {
    let text = fs::read_to_string(dump_root.join("ikconfig")).ok()?;
    text.lines()
        .find(|l| l.contains("Kernel Configuration"))
        .and_then(|l| l.split_whitespace().nth(2).map(str::to_string))
}

fn pick_fstab(set: &FstabSet) -> Option<&FstabFile> {
    let mut best: Option<(&FstabFile, i32)> = None;
    for file in &set.files {
        let path = file.path.to_string_lossy();
        let name = file
            .path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut score = file.entries.len() as i32;
        if path.contains("/vendor/etc/") {
            score += 100;
        } else if path.contains("/odm/etc/") {
            score += 80;
        } else if path.contains("/system/etc/") {
            score += 60;
        }
        if path.contains("first_stage_ramdisk") {
            score -= 10;
        }
        if name == "fstab.emmc" || name == "fstab.enableswap" {
            score -= 50;
        }
        if best.map(|(_, s)| score > s).unwrap_or(true) {
            best = Some((file, score));
        }
    }
    best.map(|(f, _)| f)
}

fn select_images(dump_root: &Path, fstab_set: &FstabSet) -> Result<SelectedImages> {
    let mut out = SelectedImages {
        pagesize: 4096,
        ..Default::default()
    };
    let mut base_candidates: Vec<(u32, u32)> = Vec::new(); // (priority, base)

    for (priority, name, kind) in [
        (0u32, "boot", "boot"),
        (1, "init_boot", "init_boot"),
        (2, "vendor_boot", "vendor_boot"),
        (3, "recovery", "recovery"),
        (4, "vendor_kernel_boot", "vendor_kernel_boot"),
    ] {
        let image_path = dump_root.join(format!("{name}.img"));
        if !image_path.is_file() {
            continue;
        }
        let size = fs::metadata(&image_path)?.len();
        match name {
            "boot" => out.boot_size = size,
            "recovery" => out.recovery_size = size,
            "init_boot" => out.init_boot_size = size,
            "vendor_boot" => out.vendor_boot_size = size,
            "vendor_kernel_boot" => out.vendor_kernel_boot_size = size,
            _ => {}
        }
        let parsed = match BootImage::parse(&image_path) {
            Ok(p) => p,
            Err(e) => {
                util::warn(format!(
                    "device-tree: cannot parse {}: {e:#}",
                    image_path.display()
                ));
                continue;
            }
        };
        out.header_version = out.header_version.max(parsed.header_version());
        match &parsed {
            BootImage::Boot { header, .. } => {
                if header.header_version < 3 {
                    out.pagesize = header.page_size;
                    out.base = header.kernel_load_address.saturating_sub(0x8000) as u64;
                    if out.cmdline.is_empty() {
                        out.cmdline = format!("{}{}", header.cmdline, header.extra_cmdline);
                    }
                } else {
                    out.pagesize = 4096;
                    if out.cmdline.is_empty() {
                        out.cmdline = header.cmdline.clone();
                    }
                }
                if let Some(v) = &header.os_version {
                    out.os_version = v.clone();
                }
                let _ = (priority, kind);
            }
            BootImage::VendorBoot { header, .. } => {
                out.pagesize = header.page_size;
                if header.kernel_load_address >= 0x8000 {
                    base_candidates.push((priority, header.kernel_load_address - 0x8000));
                }
                if out.cmdline.is_empty() {
                    out.cmdline = header.cmdline.clone();
                }
            }
        }

        let raw_kernel = dump_root.join(name).join("raw").join("kernel");
        if out.kernel.is_none() && raw_kernel.is_file() && fs::metadata(&raw_kernel)?.len() > 0 {
            out.kernel = Some(raw_kernel);
            out.kernel_source = format!("{name}.img");
        }
        let raw_dtb = dump_root.join(name).join("raw").join("dtb");
        if out.dtb.is_none() && raw_dtb.is_file() && fs::metadata(&raw_dtb)?.len() > 0 {
            out.dtb = Some(raw_dtb);
            out.dtb_source = format!("{name}.img");
        }
        let ramdisk_dir = dump_root.join(name).join("ramdisk");
        if ramdisk_dir.is_dir() {
            // A boot/init_boot/recovery ramdisk is one cpio tree even though
            // it contains top-level directories (apex, system, ...).
            // vendor_boot v4 is the exception: DumprX extracts one
            // directory per table fragment (vendor_ramdiskNN, recovery,
            // dlkm, ...).  Only expand when every child is such a fragment.
            let mut subdirs = Vec::new();
            let mut has_plain_file = false;
            if let Ok(entries) = fs::read_dir(&ramdisk_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        subdirs.push(path);
                    } else {
                        has_plain_file = true;
                    }
                }
            }
            subdirs.sort();
            let fragment_names = subdirs.iter().all(|path| {
                let label = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_ascii_lowercase();
                label.starts_with("vendor_ramdisk")
                    || matches!(label.as_str(), "recovery" | "platform" | "dlkm")
            });
            if name == "vendor_boot" && !subdirs.is_empty() && !has_plain_file && fragment_names {
                for sub in subdirs {
                    let label = sub
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    out.ramdisk_dirs.push((format!("{name}.img:{label}"), sub));
                }
            } else {
                out.ramdisk_dirs.push((format!("{name}.img"), ramdisk_dir));
            }
        }
    }

    if out.kernel.is_none() {
        // Some vendors put the kernel only in vendor_kernel_boot; the loop
        // above already checked it.  Report clearly rather than assert.
        util::warn("device-tree: no kernel found; TARGET_PREBUILT_KERNEL will be omitted");
    }

    if out.base == 0 {
        if let Some((_, base)) = base_candidates.iter().min_by_key(|(p, _)| *p) {
            out.base = *base as u64;
        }
    }

    let dtbo_img = dump_root.join("dtbo.img");
    if dtbo_img.is_file() {
        out.dtbo_size = fs::metadata(&dtbo_img)?.len();
        out.dtbo = Some(dtbo_img);
    }

    // super metadata (for BOARD_SUPER_PARTITION_* values).
    if let Ok(text) = fs::read_to_string(dump_root.join("super-info.json")) {
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            out.super_size = value.get("size").and_then(Value::as_u64).unwrap_or(0);
            if let Some(groups) = value.get("groups").and_then(Value::as_array) {
                for group in groups {
                    let name = group
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let max = group
                        .get("maximum_size")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    if max > 0 {
                        out.super_group = name.to_string();
                        out.super_group_size = max;
                        break;
                    }
                }
            }
            if let Some(parts) = value.get("partitions").and_then(Value::as_array) {
                let group = out.super_group.clone();
                for part in parts {
                    let name = part.get("name").and_then(Value::as_str).unwrap_or_default();
                    let part_group = part
                        .get("group")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if group.is_empty() || part_group == group {
                        let base = util::base_partition(name).to_string();
                        if !out.dynamic_partitions.contains(&base) {
                            out.dynamic_partitions.push(base);
                        }
                    }
                }
            }
        }
    }
    if out.dynamic_partitions.is_empty() {
        out.dynamic_partitions = fstab_set
            .fs_candidates()
            .keys()
            .map(|k| util::base_partition(k).to_string())
            .collect();
    }
    out.dynamic_partitions.sort();
    out.dynamic_partitions.dedup();
    Ok(out)
}

fn arch_block(meta: &DeviceMetadata) -> String {
    let abilist = &meta.abilist;
    let mut out = String::new();
    if abilist.contains("arm64-v8a") || abilist.is_empty() {
        out.push_str(
            "\
# Architecture
TARGET_ARCH := arm64
TARGET_ARCH_VARIANT := armv8-a
TARGET_CPU_ABI := arm64-v8a
TARGET_CPU_ABI2 :=
TARGET_CPU_VARIANT := generic
TARGET_CPU_VARIANT_RUNTIME := generic
",
        );
        if abilist.contains("armeabi-v7a") {
            out.push_str(
                "\
TARGET_2ND_ARCH := arm
TARGET_2ND_ARCH_VARIANT := armv8-a
TARGET_2ND_CPU_ABI := armeabi-v7a
TARGET_2ND_CPU_ABI2 := armeabi
TARGET_2ND_CPU_VARIANT := generic
TARGET_2ND_CPU_VARIANT_RUNTIME := generic
",
            );
        }
    } else if abilist.contains("x86_64") {
        out.push_str(
            "\
# Architecture
TARGET_ARCH := x86_64
TARGET_ARCH_VARIANT := x86_64
TARGET_CPU_ABI := x86_64
TARGET_CPU_ABI2 :=
TARGET_CPU_VARIANT := generic
TARGET_CPU_VARIANT_RUNTIME := generic
",
        );
    } else {
        out.push_str(
            "\
# Architecture
TARGET_ARCH := arm
TARGET_ARCH_VARIANT := armv7-a-neon
TARGET_CPU_ABI := armeabi-v7a
TARGET_CPU_ABI2 := armeabi
TARGET_CPU_VARIANT := generic
TARGET_CPU_VARIANT_RUNTIME := generic
",
        );
    }
    out
}

fn kernel_block(dev: &DeviceContext, twrp: bool) -> String {
    let img = dev.images;
    let mut out = String::new();
    out.push_str("# Kernel\n");
    if img.header_version != 0 {
        if twrp {
            out.push_str(&format!(
                "BOARD_BOOTIMG_HEADER_VERSION := {}\n",
                img.header_version
            ));
        } else {
            out.push_str(&format!(
                "BOARD_BOOT_HEADER_VERSION := {}\n",
                img.header_version
            ));
        }
    }
    if img.base != 0 {
        out.push_str(&format!("BOARD_KERNEL_BASE := {:#x}\n", img.base));
    }
    if !img.cmdline.is_empty() {
        out.push_str(&format!("BOARD_KERNEL_CMDLINE := {}\n", img.cmdline));
    }
    if img.pagesize != 0 {
        out.push_str(&format!("BOARD_KERNEL_PAGESIZE := {}\n", img.pagesize));
    }
    if img.header_version != 0 {
        if twrp {
            out.push_str(
                "BOARD_MKBOOTIMG_ARGS += --header_version $(BOARD_BOOTIMG_HEADER_VERSION)\n",
            );
        } else {
            out.push_str("BOARD_MKBOOTIMG_ARGS += --header_version $(BOARD_BOOT_HEADER_VERSION)\n");
        }
    }
    out.push_str("BOARD_KERNEL_IMAGE_NAME := Image\n");
    if img.dtb.is_some() {
        out.push_str("BOARD_INCLUDE_DTB_IN_BOOTIMG := true\n");
    }
    if img.dtbo.is_some() {
        out.push_str("BOARD_KERNEL_SEPARATED_DTBO := true\n");
    }
    out.push_str(&format!(
        "TARGET_KERNEL_CONFIG := {}_defconfig\n",
        dev.codename
    ));
    out.push_str(&format!(
        "TARGET_KERNEL_SOURCE := kernel/{}/{}\n\n",
        dev.manufacturer, dev.codename
    ));
    out.push_str("# Kernel - prebuilt\nTARGET_FORCE_PREBUILT_KERNEL := true\n");
    out.push_str("ifeq ($(TARGET_FORCE_PREBUILT_KERNEL),true)\n");
    if img.kernel.is_some() {
        out.push_str(if twrp {
            "TARGET_PREBUILT_KERNEL := $(DEVICE_PATH)/prebuilt/kernel\n"
        } else {
            "TARGET_PREBUILT_KERNEL := $(DEVICE_PATH)/prebuilts/kernel\n"
        });
    } else {
        out.push_str(
            "# NOTE: no kernel found in boot/vendor_kernel_boot; prebuilt kernel omitted\n",
        );
    }
    if img.dtb.is_some() {
        if twrp {
            out.push_str("TARGET_PREBUILT_DTB := $(DEVICE_PATH)/prebuilt/dtb.img\n");
        } else {
            out.push_str("TARGET_PREBUILT_DTB := $(DEVICE_PATH)/prebuilts/dtb.img\n");
        }
        out.push_str("BOARD_MKBOOTIMG_ARGS += --dtb $(TARGET_PREBUILT_DTB)\n");
        out.push_str("BOARD_INCLUDE_DTB_IN_BOOTIMG :=\n");
    }
    if img.dtbo.is_some() {
        if twrp {
            out.push_str("BOARD_PREBUILT_DTBOIMAGE := $(DEVICE_PATH)/prebuilt/dtbo.img\n");
        } else {
            out.push_str("BOARD_PREBUILT_DTBOIMAGE := $(DEVICE_PATH)/prebuilts/dtbo.img\n");
        }
        out.push_str("BOARD_KERNEL_SEPARATED_DTBO :=\n");
    }
    out.push_str("endif\n\n");
    out
}

fn partition_block(dev: &DeviceContext, twrp: bool) -> String {
    let img = dev.images;
    let mut out = String::new();
    out.push_str("# Partitions\n");
    if img.pagesize != 0 {
        out.push_str(&format!(
            "BOARD_FLASH_BLOCK_SIZE := {} # (BOARD_KERNEL_PAGESIZE * 64)\n",
            img.pagesize * 64
        ));
    }
    if img.boot_size > 0 {
        out.push_str(&format!(
            "BOARD_BOOTIMAGE_PARTITION_SIZE := {}\n",
            img.boot_size
        ));
    }
    if img.recovery_size > 0 {
        out.push_str(&format!(
            "BOARD_RECOVERYIMAGE_PARTITION_SIZE := {}\n",
            img.recovery_size
        ));
    }
    if img.init_boot_size > 0 {
        out.push_str(&format!(
            "BOARD_INIT_BOOT_IMAGE_PARTITION_SIZE := {}\n",
            img.init_boot_size
        ));
    }
    if img.vendor_boot_size > 0 {
        out.push_str(&format!(
            "BOARD_VENDOR_BOOTIMAGE_PARTITION_SIZE := {}\n",
            img.vendor_boot_size
        ));
    }
    if img.vendor_kernel_boot_size > 0 {
        out.push_str(&format!(
            "BOARD_VENDOR_KERNEL_BOOTIMAGE_PARTITION_SIZE := {}\n",
            img.vendor_kernel_boot_size
        ));
    }
    if img.dtbo_size > 0 {
        out.push_str(&format!(
            "BOARD_DTBOIMG_PARTITION_SIZE := {}\n",
            img.dtbo_size
        ));
    }
    if twrp {
        out.push_str("BOARD_HAS_LARGE_FILESYSTEM := true\n");
        out.push_str("BOARD_SYSTEMIMAGE_PARTITION_TYPE := ext4\n");
        out.push_str("BOARD_USERDATAIMAGE_FILE_SYSTEM_TYPE := ext4\n");
        out.push_str("BOARD_VENDORIMAGE_FILE_SYSTEM_TYPE := ext4\n");
        out.push_str("TARGET_COPY_OUT_VENDOR := vendor\n");
    }
    if !img.dynamic_partitions.is_empty() {
        let platform = dev.meta.platform.to_lowercase();
        let is_mtk = platform.starts_with("mt") || img.super_group.to_lowercase().contains("mtk");
        let group = if is_mtk {
            "mtk_dynamic_partitions".to_string()
        } else {
            format!("{}_dynamic_partitions", dev.manufacturer)
        };
        let token = if is_mtk {
            "MTK".to_string()
        } else {
            dev.manufacturer.to_uppercase()
        };

        // The super partition contains the liblp metadata (two geometry
        // copies + metadata slots).  Device trees conventionally reserve
        // 4 MiB for it when deriving the usable dynamic-partition size.
        let super_size = if img.super_size > 0 {
            img.super_size
        } else if img.super_group_size > 0 {
            img.super_group_size + 4 * 1024 * 1024
        } else {
            0
        };
        if super_size > 0 {
            out.push_str(&format!("BOARD_SUPER_PARTITION_SIZE := {super_size}\n"));
            out.push_str(&format!(
                "# partition-size:super: {super_size:x} (from liblp metadata)\n"
            ));
        } else {
            out.push_str(
                "BOARD_SUPER_PARTITION_SIZE := 9126805504 # TODO: verify from super metadata\n",
            );
        }
        out.push_str(&format!("BOARD_SUPER_PARTITION_GROUPS := {group}\n"));
        out.push_str(&format!(
            "BOARD_{token}_DYNAMIC_PARTITIONS_PARTITION_LIST := $(ALL_PARTITIONS)\n"
        ));
        out.push_str(&format!(
            "# $(ALL_PARTITIONS) currently expands to: {}\n",
            img.dynamic_partitions.join(" ")
        ));
        let dynamic_size = if super_size > 4 * 1024 * 1024 {
            super_size - 4 * 1024 * 1024
        } else if img.super_group_size > 0 {
            img.super_group_size
        } else {
            0
        };
        if dynamic_size > 0 {
            out.push_str(&format!(
                "BOARD_{token}_DYNAMIC_PARTITIONS_SIZE := {dynamic_size}\n"
            ));
            out.push_str(&format!(
                "# {super_size} - {} (4 MiB) = {dynamic_size}\n",
                4 * 1024 * 1024
            ));
        } else {
            out.push_str(&format!(
                "BOARD_{token}_DYNAMIC_PARTITIONS_SIZE := 9122611200 # TODO: verify from super metadata\n"
            ));
        }
    }
    out.push('\n');
    out
}

fn recovery_block(dev: &DeviceContext, twrp: bool) -> String {
    let mut out = String::new();
    out.push_str("# Recovery\n");
    if let Some(fstab) = dev.fstab {
        let name = fstab.path.file_name().unwrap_or_default().to_string_lossy();
        if twrp {
            out.push_str("TARGET_RECOVERY_FSTAB := $(DEVICE_PATH)/recovery.fstab\n");
        } else {
            out.push_str(&format!(
                "TARGET_RECOVERY_FSTAB := $(DEVICE_PATH)/rootdir/etc/{name}\n"
            ));
        }
    } else {
        out.push_str("# NOTE: no fstab found; TARGET_RECOVERY_FSTAB not set\n");
    }
    out.push_str("TARGET_USERIMAGES_USE_EXT4 := true\n");
    out.push_str("TARGET_USERIMAGES_USE_F2FS := true\n");
    if dev.meta.density != "undefined" && !dev.meta.density.is_empty() {
        out.push_str(&format!("TARGET_SCREEN_DENSITY := {}\n", dev.meta.density));
    }
    if twrp {
        out.push_str("TW_THEME := portrait_hdpi\n");
        out.push_str("TW_EXTRA_LANGUAGES := true\n");
        out.push_str("TW_SCREEN_BLANK_ON_BOOT := true\n");
        out.push_str("TW_INCLUDE_CRYPTO := true\n");
        out.push_str("TW_INCLUDE_FUSE_EXFAT := true\n");
        out.push_str("TW_INCLUDE_FUSE_NTFS := true\n");
        out.push_str("TW_USE_TOOLBOX := true\n");
    }
    out.push('\n');
    out
}

fn generate_aosp(root: &Path, dev: &DeviceContext) -> Result<()> {
    fs::create_dir_all(root)?;
    fs::create_dir_all(root.join("prebuilts"))?;
    fs::create_dir_all(root.join("rootdir/etc"))?;
    fs::create_dir_all(root.join("configs/properties"))?;
    fs::create_dir_all(root.join("configs/vintf"))?;
    let vintf = find_vintf(dev);

    let aosp_board = format!(
        "\
DEVICE_PATH := device/{manufacturer}/{codename}
CONFIGS_PATH := $(DEVICE_PATH)/configs

{arch}
# Bootloader
TARGET_BOOTLOADER_BOARD_NAME := {board}
TARGET_NO_BOOTLOADER := true

{kernel}
{partitions}
# Platform
TARGET_BOARD_PLATFORM := {platform}

# Properties
PROPS_PATH := $(CONFIGS_PATH)/properties
{props}
{vintf}
{recovery}
",
        manufacturer = dev.manufacturer,
        codename = dev.codename,
        arch = arch_block(dev.meta),
        board = board_name(dev.meta),
        kernel = kernel_block(dev, false),
        partitions = partition_block(dev, false),
        platform = dev.meta.platform,
        props = prop_block(dev),
        vintf = vintf_block(&vintf),
        recovery = recovery_block(dev, false),
    );
    fs::write(root.join("BoardConfig.mk"), aosp_board)?;

    let ab = dev.meta.is_ab == "true";
    let mut board_extra = String::new();
    if ab {
        board_extra.push_str("# A/B\nAB_OTA_UPDATER := true\nAB_OTA_PARTITIONS +=");
        for part in &dev.images.dynamic_partitions {
            board_extra.push_str(&format!(" \\\n    {part}"));
        }
        board_extra.push_str("\n\nBOARD_USES_RECOVERY_AS_BOOT := true\n\n");
    }
    if !board_extra.is_empty() {
        // Keep the A/B block near the top, like the old templates.
        let current = fs::read_to_string(root.join("BoardConfig.mk"))?;
        fs::write(
            root.join("BoardConfig.mk"),
            format!("{board_extra}{current}"),
        )?;
    }

    fs::write(root.join("Android.bp"), "soong_namespace {\n}\n")?;
    fs::write(
        root.join("Android.mk"),
        format!(
            "\
LOCAL_PATH := $(call my-dir)
ifneq ($(filter {codename},$(TARGET_DEVICE)),)
include $(call all-makefiles-under,$(LOCAL_PATH))
endif
",
            codename = dev.codename
        ),
    )?;
    fs::write(
        root.join("AndroidProducts.mk"),
        format!(
            "\
PRODUCT_MAKEFILES := \\
    $(LOCAL_DIR)/lineage_{codename}.mk

COMMON_LUNCH_CHOICES := \\
    lineage_{codename}-user \\
    lineage_{codename}-userdebug \\
    lineage_{codename}-eng
",
            codename = dev.codename
        ),
    )?;
    fs::write(
        root.join(format!("lineage_{}.mk", dev.codename)),
        format!(
            "\
$(call inherit-product, $(SRC_TARGET_DIR)/product/core_64_bit.mk)
$(call inherit-product, $(SRC_TARGET_DIR)/product/full_base_telephony.mk)

$(call inherit-product, device/{manufacturer}/{codename}/device.mk)

PRODUCT_DEVICE := {codename}
PRODUCT_NAME := lineage_{codename}
PRODUCT_BRAND := {brand}
PRODUCT_MODEL := {model}
PRODUCT_MANUFACTURER := {manufacturer}

PRODUCT_BUILD_PROP_OVERRIDES += \\
    PRIVATE_BUILD_DESC=\"{description}\"

BUILD_FINGERPRINT := {fingerprint}
",
            manufacturer = dev.manufacturer,
            codename = dev.codename,
            brand = dev.meta.brand,
            model = model_name(dev.meta),
            description = dev.meta.description,
            fingerprint = dev.meta.fingerprint,
        ),
    )?;
    fs::write(
        root.join("device.mk"),
        format!(
            "\
LOCAL_PATH := device/{manufacturer}/{codename}

PRODUCT_COPY_FILES += \\
    $(LOCAL_PATH)/rootdir/etc/{fstab}:$(TARGET_COPY_OUT_VENDOR)/etc/{fstab}

$(call inherit-product-if-exists, vendor/{manufacturer}/{codename}/{codename}-vendor.mk)
",
            manufacturer = dev.manufacturer,
            codename = dev.codename,
            fstab = dev
                .fstab
                .map(|f| f
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned())
                .unwrap_or_else(|| "fstab".to_string()),
        ),
    )?;

    copy_prebuilts(dev, &root.join("prebuilts"), false)?;
    copy_props(dev, root)?;
    copy_vintf(root, &vintf)?;
    copy_proprietary_files(dev, root)?;
    copy_rootdir(dev, root)?;
    fs::write(root.join("README.md"), tree_readme(dev, "AOSP/LineageOS"))?;
    Ok(())
}

fn generate_twrp(root: &Path, dev: &DeviceContext) -> Result<()> {
    fs::create_dir_all(root)?;
    fs::create_dir_all(root.join("prebuilt"))?;
    fs::create_dir_all(root.join("recovery/root"))?;

    let mut board = format!(
        "\
DEVICE_PATH := device/{manufacturer}/{codename}

# For building with minimal manifest
ALLOW_MISSING_DEPENDENCIES := true

{arch}
# Bootloader
TARGET_BOOTLOADER_BOARD_NAME := {board}
TARGET_NO_BOOTLOADER := true

{kernel}
{partitions}
# Platform
TARGET_BOARD_PLATFORM := {platform}

{recovery}
",
        manufacturer = dev.manufacturer,
        codename = dev.codename,
        arch = arch_block(dev.meta),
        board = board_name(dev.meta),
        kernel = kernel_block(dev, true),
        partitions = partition_block(dev, true),
        platform = dev.meta.platform,
        recovery = recovery_block(dev, true),
    );
    if dev.meta.is_ab == "true" {
        let mut ab = "# A/B\nAB_OTA_UPDATER := true\nAB_OTA_PARTITIONS +=".to_string();
        for part in &dev.images.dynamic_partitions {
            ab.push_str(&format!(" \\\n    {part}"));
        }
        ab.push_str("\n\nBOARD_USES_RECOVERY_AS_BOOT := true\n\n");
        board = format!("{ab}{board}");
    }
    fs::write(root.join("BoardConfig.mk"), board)?;
    fs::write(
        root.join("Android.mk"),
        format!(
            "LOCAL_PATH := $(call my-dir)\nifneq ($(filter {codename},$(TARGET_DEVICE)),)\ninclude $(call all-makefiles-under,$(LOCAL_PATH))\nendif\n",
            codename = dev.codename
        ),
    )?;
    fs::write(root.join("Android.bp"), "soong_namespace {\n}\n")?;
    fs::write(
        root.join("AndroidProducts.mk"),
        format!(
            "PRODUCT_MAKEFILES := \\\n    $(LOCAL_DIR)/omni_{codename}.mk\n\nCOMMON_LUNCH_CHOICES := \\\n    omni_{codename}-userdebug\n    omni_{codename}-eng\n",
            codename = dev.codename
        ),
    )?;
    fs::write(
        root.join(format!("omni_{}.mk", dev.codename)),
        format!(
            "\
$(call inherit-product, $(SRC_TARGET_DIR)/product/core_64_bit.mk)
$(call inherit-product, $(SRC_TARGET_DIR)/product/full_base_telephony.mk)

$(call inherit-product, vendor/omni/config/common.mk)
$(call inherit-product, device/{manufacturer}/{codename}/device.mk)

PRODUCT_DEVICE := {codename}
PRODUCT_NAME := omni_{codename}
PRODUCT_BRAND := {brand}
PRODUCT_MODEL := {model}
PRODUCT_MANUFACTURER := {manufacturer}

PRODUCT_BUILD_PROP_OVERRIDES += \\
    PRIVATE_BUILD_DESC=\"{description}\"

BUILD_FINGERPRINT := {fingerprint}
",
            manufacturer = dev.manufacturer,
            codename = dev.codename,
            brand = dev.meta.brand,
            model = model_name(dev.meta),
            description = dev.meta.description,
            fingerprint = dev.meta.fingerprint,
        ),
    )?;
    fs::write(
        root.join("device.mk"),
        format!(
            "\
LOCAL_PATH := device/{manufacturer}/{codename}

PRODUCT_COPY_FILES += \\
    $(LOCAL_PATH)/recovery.fstab:$(TARGET_COPY_OUT_RECOVERY)/root/system/etc/recovery.fstab
",
            manufacturer = dev.manufacturer,
            codename = dev.codename
        ),
    )?;
    fs::write(
        root.join("vendorsetup.sh"),
        format!("add_lunch_combo omni_{}-userdebug\n", dev.codename),
    )?;
    fs::write(
        root.join("recovery.fstab"),
        dev.fstab
            .map(twrp_fstab)
            .unwrap_or_else(|| "# NOTE: no fstab found in dump\n".to_string()),
    )?;
    copy_prebuilts(dev, &root.join("prebuilt"), true)?;
    copy_recovery_root(dev, &root.join("recovery/root"))?;
    fs::write(root.join("README.md"), tree_readme(dev, "TWRP"))?;
    Ok(())
}

fn board_name(meta: &DeviceMetadata) -> String {
    if !meta.platform.is_empty() {
        meta.platform.clone()
    } else {
        "qcom".to_string()
    }
}

fn aosp_device_name(codename: &str) -> String {
    let codename = codename.trim();
    if let Some((_, after)) = codename.rsplit_once('-') {
        if !after.is_empty() {
            return after.to_string();
        }
    }
    codename.to_string()
}

fn model_name(meta: &DeviceMetadata) -> String {
    if meta.codename.is_empty() {
        meta.brand.clone()
    } else {
        meta.codename.clone()
    }
}

#[derive(Debug, Default)]
struct VintfFiles {
    manifest: Option<PathBuf>,
    compatibility_matrix: Option<PathBuf>,
    device_compatibility_matrix: Option<PathBuf>,
}

fn existing(root: &Path, candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(|candidate| root.join(candidate))
        .find(|path| path.is_file())
}

fn find_vintf(dev: &DeviceContext) -> VintfFiles {
    let root = dev.dump_root;
    let manifest = existing(
        root,
        &[
            "vendor/etc/vintf/manifest.xml",
            "vendor/vendor/etc/vintf/manifest.xml",
            "odm/etc/vintf/manifest.xml",
            "odm/odm/etc/vintf/manifest.xml",
        ],
    );
    let compatibility_matrix = existing(
        root,
        &[
            "vendor/etc/vintf/compatibility_matrix.xml",
            "vendor/vendor/etc/vintf/compatibility_matrix.xml",
            "odm/etc/vintf/compatibility_matrix.xml",
            "odm/odm/etc/vintf/compatibility_matrix.xml",
        ],
    );
    let device_compatibility_matrix = existing(
        root,
        &[
            "system/etc/vintf/compatibility_matrix.device.xml",
            "system/system/etc/vintf/compatibility_matrix.device.xml",
            "system_ext/etc/vintf/compatibility_matrix.device.xml",
            "system_ext/system_ext/etc/vintf/compatibility_matrix.device.xml",
        ],
    );
    VintfFiles {
        manifest,
        compatibility_matrix,
        device_compatibility_matrix,
    }
}

fn vintf_block(vintf: &VintfFiles) -> String {
    let mut out = String::new();
    out.push_str("# VINTF\n");
    out.push_str("VINTF_PATH := $(CONFIGS_PATH)/vintf\n");
    if vintf.manifest.is_some() {
        out.push_str("DEVICE_MANIFEST_FILE := $(VINTF_PATH)/manifest.xml\n");
    } else {
        out.push_str("# NOTE: vendor/etc/vintf/manifest.xml not found\n");
    }
    if vintf.compatibility_matrix.is_some() {
        out.push_str("DEVICE_MATRIX_FILE := $(VINTF_PATH)/compatibility_matrix.xml\n");
    } else {
        out.push_str("# NOTE: vendor/etc/vintf/compatibility_matrix.xml not found\n");
    }
    if vintf.device_compatibility_matrix.is_some() {
        out.push_str(
            "DEVICE_FRAMEWORK_COMPATIBILITY_MATRIX_FILE := $(VINTF_PATH)/compatibility_matrix.device.xml\n",
        );
    } else {
        out.push_str("# NOTE: system/etc/vintf/compatibility_matrix.device.xml not found\n");
    }
    out
}

fn copy_vintf(root: &Path, vintf: &VintfFiles) -> Result<()> {
    let dest = root.join("configs/vintf");
    util::ensure_dir(&dest)?;
    if let Some(src) = &vintf.manifest {
        util::copy_file(src, &dest.join("manifest.xml"))?;
    }
    if let Some(src) = &vintf.compatibility_matrix {
        util::copy_file(src, &dest.join("compatibility_matrix.xml"))?;
    }
    if let Some(src) = &vintf.device_compatibility_matrix {
        util::copy_file(src, &dest.join("compatibility_matrix.device.xml"))?;
    }
    Ok(())
}

fn prop_block(dev: &DeviceContext) -> String {
    let mut out = String::new();
    for partition in [
        "system",
        "system_ext",
        "vendor",
        "odm",
        "product",
        "vendor_dlkm",
        "odm_dlkm",
        "system_dlkm",
    ] {
        let candidates = [
            dev.dump_root.join(partition).join("build.prop"),
            dev.dump_root
                .join(partition)
                .join(partition)
                .join("build.prop"),
            dev.dump_root.join(partition).join("etc").join("build.prop"),
        ];
        if candidates.iter().any(|p| p.is_file()) {
            out.push_str(&format!(
                "TARGET_{}_PROP += $(PROPS_PATH)/{partition}.prop\n",
                partition.to_uppercase()
            ));
        }
    }
    if out.is_empty() {
        out.push_str("# NOTE: no partition build.prop files found\n");
    }
    out
}

fn copy_props(dev: &DeviceContext, root: &Path) -> Result<()> {
    util::ensure_dir(&root.join("configs/properties"))?;
    for partition in [
        "system",
        "system_ext",
        "vendor",
        "odm",
        "product",
        "vendor_dlkm",
        "odm_dlkm",
        "system_dlkm",
    ] {
        let candidates = [
            dev.dump_root.join(partition).join("build.prop"),
            dev.dump_root
                .join(partition)
                .join(partition)
                .join("build.prop"),
            dev.dump_root.join(partition).join("etc").join("build.prop"),
        ];
        if let Some(src) = candidates.iter().find(|p| p.is_file()) {
            util::copy_file(
                src,
                &root
                    .join("configs/properties")
                    .join(format!("{partition}.prop")),
            )?;
        }
    }
    Ok(())
}

fn copy_proprietary_files(dev: &DeviceContext, root: &Path) -> Result<()> {
    crate::proprietary::generate_aosp_tree(dev.dump_root, dev.meta, &dev.meta.description, root)?;
    Ok(())
}

fn copy_prebuilts(dev: &DeviceContext, dir: &Path, twrp: bool) -> Result<()> {
    util::ensure_dir(dir)?;
    if let Some(kernel) = &dev.images.kernel {
        util::copy_file(kernel, &dir.join("kernel"))?;
    }
    if let Some(dtb) = &dev.images.dtb {
        util::copy_file(dtb, &dir.join("dtb.img"))?;
    }
    if let Some(dtbo) = &dev.images.dtbo {
        util::copy_file(dtbo, &dir.join("dtbo.img"))?;
    }
    let _ = twrp;
    Ok(())
}

fn copy_rootdir(dev: &DeviceContext, root: &Path) -> Result<()> {
    let etc = root.join("rootdir/etc");
    util::ensure_dir(&etc)?;
    if let Some(fstab) = dev.fstab {
        let name = fstab.path.file_name().unwrap_or_default();
        util::copy_file(&fstab.path, &etc.join(name))?;
    }
    let init_dir = etc.join("init");
    util::ensure_dir(&init_dir)?;
    for (_, ramdisk) in &dev.images.ramdisk_dirs {
        copy_rc_files(ramdisk, &init_dir)?;
    }
    // Vendor init scripts are part of the device rootdir in most trees.
    for sub in ["init", "init/hw"] {
        let dir = dev.dump_root.join("vendor/etc").join(sub);
        if dir.is_dir() {
            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file()
                        && path
                            .file_name()
                            .map(|n| n.to_string_lossy().ends_with(".rc"))
                            .unwrap_or(false)
                    {
                        let name = path.file_name().unwrap().to_string_lossy();
                        util::copy_file(&path, &init_dir.join(name.as_ref()))?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn copy_recovery_root(dev: &DeviceContext, root: &Path) -> Result<()> {
    util::ensure_dir(root)?;
    for (_, ramdisk) in &dev.images.ramdisk_dirs {
        copy_rc_files(ramdisk, root)?;
        for name in ["default.prop", "prop.default", "init.recovery.qcom.rc"] {
            let candidate = ramdisk.join(name);
            if candidate.is_file() {
                util::copy_file(&candidate, &root.join(name))?;
            }
        }
    }
    Ok(())
}

fn copy_rc_files(ramdisk: &Path, dest: &Path) -> Result<()> {
    util::ensure_dir(dest)?;
    let Ok(entries) = fs::read_dir(ramdisk) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.ends_with(".rc") && name != "init.rc" {
            util::copy_file(&path, &dest.join(name.as_ref()))?;
        }
    }
    Ok(())
}

fn twrp_fstab(fstab: &FstabFile) -> String {
    let mut best: BTreeMap<String, &FstabEntry> = BTreeMap::new();
    for entry in &fstab.entries {
        if !matches!(entry.fs_type.as_str(), "ext4" | "f2fs" | "erofs" | "vfat") {
            continue;
        }
        if entry.mount_point.starts_with("/mnt/") || entry.mount_point == "auto" {
            continue;
        }
        // Keep the first entry for each mount point, matching the order in
        // the vendor fstab (e.g. erofs before ext4).
        best.entry(entry.mount_point.clone()).or_insert(entry);
    }
    let mut out = format!(
        "# Generated by DumprX from {}\n# <mount point> <filesystem> <device> <flags>\n\n",
        fstab.path.display()
    );
    for (mount, entry) in best {
        let device = if entry.is_logical() {
            format!("/dev/block/mapper/{}", entry.source)
        } else {
            entry.source.clone()
        };
        let mut flags = entry.mount_flags.join(",");
        if flags.is_empty() {
            flags = "defaults".to_string();
        }
        let mut extra = format!("flags=display=\"{}\"", display_name(&mount));
        if entry.has_fs_mgr_flag("slotselect") {
            extra.push_str(";slotselect");
        }
        if mount == "/data" {
            extra.push_str(";wipeingui;wipeduringinstall;length=-16384");
        }
        out.push_str(&format!(
            "{mount} {} {device} {flags} {extra}\n",
            entry.fs_type
        ));
    }
    out
}

fn display_name(mount: &str) -> String {
    let name = mount.trim_matches('/');
    if name.is_empty() {
        return "Root".to_string();
    }
    let name = name.rsplit('/').next().unwrap_or(name);
    let mut chars = name.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => name.to_string(),
    }
}

fn tree_readme(dev: &DeviceContext, kind: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# {kind} device tree for {} ({})\n\n",
        dev.codename, dev.manufacturer
    ));
    if dev.detected_codename != dev.codename {
        out.push_str(&format!(
            "- Detected codename: `{}` (AOSP-safe name: `{}`)\n\n",
            dev.detected_codename, dev.codename
        ));
    }
    out.push_str("Generated by the Rust DumprX device-tree generator.\n\n## Images used\n\n");
    match &dev.images.kernel {
        Some(path) => out.push_str(&format!(
            "- kernel: `{}` (from {})\n",
            path.strip_prefix(dev.dump_root).unwrap_or(path).display(),
            dev.images.kernel_source
        )),
        None => out.push_str("- kernel: **not found** in boot/vendor_kernel_boot\n"),
    }
    if dev.images.ramdisk_dirs.is_empty() {
        out.push_str("- ramdisk: **not found** in boot/init_boot/vendor_boot/recovery\n");
    } else {
        for (source, path) in &dev.images.ramdisk_dirs {
            out.push_str(&format!(
                "- ramdisk: `{}` (from {})\n",
                path.strip_prefix(dev.dump_root).unwrap_or(path).display(),
                source
            ));
        }
    }
    match &dev.images.dtb {
        Some(path) => out.push_str(&format!(
            "- dtb: `{}` (from {})\n",
            path.strip_prefix(dev.dump_root).unwrap_or(path).display(),
            dev.images.dtb_source
        )),
        None => out.push_str("- dtb: not present\n"),
    }
    match &dev.images.dtbo {
        Some(path) => {
            let display = if dev.dump_root.join("dtbo").is_dir() {
                "dtbo/".to_string()
            } else {
                path.strip_prefix(dev.dump_root)
                    .unwrap_or(path)
                    .display()
                    .to_string()
            };
            out.push_str(&format!("- dtbo: `{display}`\n"));
        }
        None => out.push_str("- dtbo: not present\n"),
    }
    out.push_str("- proprietary files: `proprietary-files.txt` / `proprietary-files.sha1`\n");
    if let Some(fstab) = dev.fstab {
        out.push_str(&format!(
            "- fstab: `{}` ({} entries)\n",
            fstab.path.display(),
            fstab.entries.len()
        ));
    } else {
        out.push_str("- fstab: **not found** (recovery.fstab is a placeholder)\n");
    }
    out.push_str("\n## Metadata\n\n");
    out.push_str(&format!("- Fingerprint: `{}`\n", dev.meta.fingerprint));
    out.push_str(&format!("- Platform: `{}`\n", dev.meta.platform));
    out.push_str(&format!("- Android: `{}`\n", dev.meta.release));
    out.push_str(&format!("- A/B: `{}`\n", dev.meta.is_ab));
    out.push_str(&format!(
        "- Header version: `{}`\n- Kernel base: `{:#x}`\n- Pagesize: `{}`\n",
        dev.images.header_version, dev.images.base, dev.images.pagesize
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aosp_names_drop_the_dash_prefix() {
        assert_eq!(aosp_device_name("tecno-cm6"), "cm6");
        assert_eq!(aosp_device_name("TECNO-CM6"), "CM6");
        assert_eq!(
            aosp_device_name("mssi_64_64only_cn_armv82"),
            "mssi_64_64only_cn_armv82"
        );
        assert_eq!(aosp_device_name("foo-bar-baz"), "baz");
        assert_eq!(aosp_device_name("foo-"), "foo-");
    }
}
