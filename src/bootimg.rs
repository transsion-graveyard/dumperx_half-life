//! Android boot image parsing, including header v4.
//!
//! The old `utils/bootimg_info.py` only understood header v3.  This module
//! implements the full v0..v4 boot header and v3/v4 vendor_boot header
//! (vendor ramdisk table + bootconfig) directly from the AOSP layout in
//! `system/tools/mkbootimg/include/bootimg/bootimg.h`.

use anyhow::{bail, Context, Result};
use std::fmt::Write as _;
use std::fs;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const BOOT_MAGIC: &[u8; 8] = b"ANDROID!";
pub const VENDOR_BOOT_MAGIC: &[u8; 8] = b"VNDRBOOT";

pub const BOOT_IMAGE_HEADER_V3_PAGESIZE: u32 = 4096;
pub const VENDOR_RAMDISK_TYPE_NONE: u32 = 0;
pub const VENDOR_RAMDISK_TYPE_PLATFORM: u32 = 1;
pub const VENDOR_RAMDISK_TYPE_RECOVERY: u32 = 2;
pub const VENDOR_RAMDISK_TYPE_DLKM: u32 = 3;

#[derive(Debug, Clone)]
pub struct BootHeader {
    pub magic: [u8; 8],
    pub header_version: u32,
    pub kernel_size: u32,
    pub ramdisk_size: u32,
    pub page_size: u32,
    pub os_version_raw: u32,
    pub os_version: Option<String>,
    pub os_patch_level: Option<String>,
    pub header_size: u32,
    pub kernel_load_address: u32,
    pub ramdisk_load_address: u32,
    pub second_size: u32,
    pub second_load_address: u32,
    pub tags_load_address: u32,
    pub product_name: String,
    pub cmdline: String,
    pub extra_cmdline: String,
    pub recovery_dtbo_size: u32,
    pub recovery_dtbo_offset: u64,
    pub dtb_size: u32,
    pub dtb_load_address: u64,
    pub signature_size: u32,
}

#[derive(Debug, Clone)]
pub struct VendorRamdiskEntry {
    pub size: u32,
    pub offset: u32,
    pub ramdisk_type: u32,
    pub name: String,
    pub board_id: [u32; 16],
}

#[derive(Debug, Clone)]
pub struct VendorBootHeader {
    pub magic: [u8; 8],
    pub header_version: u32,
    pub page_size: u32,
    pub kernel_load_address: u32,
    pub ramdisk_load_address: u32,
    pub vendor_ramdisk_size: u32,
    pub cmdline: String,
    pub tags_load_address: u32,
    pub product_name: String,
    pub header_size: u32,
    pub dtb_size: u32,
    pub dtb_load_address: u64,
    pub vendor_ramdisk_table_size: u32,
    pub vendor_ramdisk_table_entry_num: u32,
    pub vendor_ramdisk_table_entry_size: u32,
    pub vendor_bootconfig_size: u32,
    pub ramdisk_table: Vec<VendorRamdiskEntry>,
}

#[derive(Debug, Clone)]
pub struct ExtractedFile {
    pub name: String,
    pub size: u64,
    pub offset: u64,
}

#[derive(Debug, Clone)]
pub enum BootImage {
    Boot {
        header: Box<BootHeader>,
        kernel: Vec<u8>,
        ramdisk: Vec<u8>,
        second: Vec<u8>,
        recovery_dtbo: Vec<u8>,
        dtb: Vec<u8>,
        signature: Vec<u8>,
    },
    VendorBoot {
        header: Box<VendorBootHeader>,
        ramdisks: Vec<Vec<u8>>,
        dtb: Vec<u8>,
        bootconfig: Vec<u8>,
    },
}

impl BootImage {
    pub fn parse(path: &Path) -> Result<Self> {
        let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse_bytes(&data).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn parse_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < 8 {
            bail!("image is shorter than the 8-byte magic");
        }
        let magic: [u8; 8] = data[..8].try_into().unwrap();
        if &magic == BOOT_MAGIC {
            parse_boot(data)
        } else if &magic == VENDOR_BOOT_MAGIC {
            parse_vendor_boot(data)
        } else {
            bail!(
                "not an Android boot image: magic {:?}",
                String::from_utf8_lossy(&magic)
            );
        }
    }

    pub fn header_version(&self) -> u32 {
        match self {
            BootImage::Boot { header, .. } => header.header_version,
            BootImage::VendorBoot { header, .. } => header.header_version,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            BootImage::Boot { .. } => "boot",
            BootImage::VendorBoot { .. } => "vendor_boot",
        }
    }

    /// Human readable info, including all v4-only fields.
    pub fn info(&self) -> String {
        let mut out = String::new();
        match self {
            BootImage::Boot {
                header,
                kernel,
                ramdisk,
                second,
                recovery_dtbo,
                dtb,
                signature,
            } => {
                let _ = writeln!(
                    out,
                    "boot magic: {}",
                    String::from_utf8_lossy(&header.magic)
                );
                let _ = writeln!(out, "header version: {}", header.header_version);
                let _ = writeln!(out, "kernel_size: {}", kernel.len());
                let _ = writeln!(out, "ramdisk size: {}", ramdisk.len());
                if let Some(v) = &header.os_version {
                    let _ = writeln!(out, "os_version: {v}");
                }
                if let Some(v) = &header.os_patch_level {
                    let _ = writeln!(out, "os_patch_level: {v}");
                }
                let _ = writeln!(out, "boot image header size: {}", header.header_size);
                if header.header_version < 3 {
                    let base = header.kernel_load_address.wrapping_sub(0x0000_8000);
                    let _ = writeln!(out, "pagesize: {}", header.page_size);
                    let _ = writeln!(out, "base: {base:#010x}");
                    let _ = writeln!(
                        out,
                        "kernel_offset: {:#010x}",
                        header.kernel_load_address.wrapping_sub(base)
                    );
                    let _ = writeln!(
                        out,
                        "ramdisk_offset: {:#010x}",
                        header.ramdisk_load_address.wrapping_sub(base)
                    );
                    if header.second_size > 0 {
                        let _ = writeln!(
                            out,
                            "second_offset: {:#010x}",
                            header.second_load_address.wrapping_sub(base)
                        );
                    }
                    let _ = writeln!(
                        out,
                        "tags_offset: {:#010x}",
                        header.tags_load_address.wrapping_sub(base)
                    );
                    if header.header_version == 2 {
                        let _ = writeln!(out, "dtb_offset: {:#010x}", header.dtb_load_address);
                    }
                    let _ = writeln!(out, "board: '{}'", header.product_name);
                    let _ = writeln!(
                        out,
                        "cmdline: '{}{}'",
                        header.cmdline, header.extra_cmdline
                    );
                } else {
                    let _ = writeln!(out, "pagesize: {BOOT_IMAGE_HEADER_V3_PAGESIZE}");
                    let _ = writeln!(out, "cmdline: '{}'", header.cmdline);
                }
                if header.header_version >= 4 {
                    let _ = writeln!(out, "boot.img signature size: {}", signature.len());
                }
                if !second.is_empty() {
                    let _ = writeln!(out, "second size: {}", second.len());
                }
                if !recovery_dtbo.is_empty() {
                    let _ = writeln!(out, "recovery dtbo size: {}", recovery_dtbo.len());
                }
                if !dtb.is_empty() {
                    let _ = writeln!(out, "dtb size: {}", dtb.len());
                }
            }
            BootImage::VendorBoot {
                header,
                ramdisks,
                dtb,
                bootconfig,
            } => {
                let _ = writeln!(
                    out,
                    "boot magic: {}",
                    String::from_utf8_lossy(&header.magic)
                );
                let _ = writeln!(
                    out,
                    "vendor boot image header version: {}",
                    header.header_version
                );
                let _ = writeln!(out, "pagesize: {:#010x}", header.page_size);
                let _ = writeln!(out, "kernel load address: {:#010x}", header.kernel_load_address);
                let _ = writeln!(
                    out,
                    "ramdisk load address: {:#010x}",
                    header.ramdisk_load_address
                );
                if header.header_version > 3 {
                    let _ = writeln!(
                        out,
                        "vendor ramdisk total size: {}",
                        header.vendor_ramdisk_size
                    );
                } else {
                    let _ = writeln!(out, "vendor ramdisk size: {}", header.vendor_ramdisk_size);
                }
                let _ = writeln!(out, "vendor command line args: '{}'", header.cmdline);
                let _ = writeln!(
                    out,
                    "kernel tags load address: {:#010x}",
                    header.tags_load_address
                );
                let _ = writeln!(out, "product name: '{}'", header.product_name);
                let _ = writeln!(out, "vendor boot image header size: {}", header.header_size);
                let _ = writeln!(out, "dtb size: {}", header.dtb_size);
                let _ = writeln!(out, "dtb address: {:#018x}", header.dtb_load_address);
                if header.header_version > 3 {
                    let _ = writeln!(
                        out,
                        "vendor ramdisk table size: {}",
                        header.vendor_ramdisk_table_size
                    );
                    let _ = writeln!(out, "vendor ramdisk table: [");
                    for (i, entry) in header.ramdisk_table.iter().enumerate() {
                        let _ = writeln!(out, "    vendor_ramdisk{i:02}: {{");
                        let _ = writeln!(out, "        size: {}", entry.size);
                        let _ = writeln!(out, "        offset: {}", entry.offset);
                        let _ = writeln!(out, "        type: {:#x}", entry.ramdisk_type);
                        let _ = writeln!(out, "        name: '{}'", entry.name);
                        let _ = write!(out, "        board_id: [");
                        for (j, id) in entry.board_id.iter().enumerate() {
                            if j % 4 == 0 {
                                let _ = write!(out, "\n            ");
                            }
                            let _ = write!(out, "{id:#010x}, ");
                        }
                        let _ = writeln!(out, "\n        ]");
                        let _ = writeln!(out, "    }}");
                        let _ = writeln!(
                            out,
                            "    # extracted vendor_ramdisk{i:02} size: {}",
                            ramdisks.get(i).map(Vec::len).unwrap_or(0)
                        );
                    }
                    let _ = writeln!(out, "]");
                    let _ = writeln!(
                        out,
                        "vendor bootconfig size: {}",
                        header.vendor_bootconfig_size
                    );
                    if !bootconfig.is_empty() {
                        let _ = writeln!(out, "vendor bootconfig:");
                        for line in String::from_utf8_lossy(bootconfig).lines() {
                            let _ = writeln!(out, "    {line}");
                        }
                    }
                } else if !dtb.is_empty() {
                    let _ = writeln!(out, "dtb extracted: {} bytes", dtb.len());
                }
            }
        }
        out
    }

    /// Unpack the image to `dir`.  Returns the list of files written.
    pub fn unpack(&self, dir: &Path) -> Result<Vec<ExtractedFile>> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut files = Vec::new();
        let mut write = |name: &str, bytes: &[u8]| -> Result<()> {
            if bytes.is_empty() {
                return Ok(());
            }
            let path = dir.join(name);
            fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
            files.push(ExtractedFile {
                name: name.to_owned(),
                size: bytes.len() as u64,
                offset: 0,
            });
            Ok(())
        };
        match self {
            BootImage::Boot {
                kernel,
                ramdisk,
                second,
                recovery_dtbo,
                dtb,
                signature,
                ..
            } => {
                write("kernel", kernel)?;
                write("ramdisk", ramdisk)?;
                write("second", second)?;
                write("recovery_dtbo", recovery_dtbo)?;
                write("dtb", dtb)?;
                write("boot_signature", signature)?;
            }
            BootImage::VendorBoot {
                header,
                ramdisks,
                dtb,
                bootconfig,
                ..
            } => {
                if header.header_version > 3 {
                    for (i, bytes) in ramdisks.iter().enumerate() {
                        write(&format!("vendor_ramdisk{i:02}"), bytes)?;
                    }
                    write("bootconfig", bootconfig)?;
                    // Named symlinks are handy for inspection, but keep the
                    // dump portable: write a small manifest instead.
                    let mut manifest = String::new();
                    for (i, entry) in header.ramdisk_table.iter().enumerate() {
                        let _ = writeln!(
                            manifest,
                            "vendor_ramdisk{i:02}\t{}\t{}\t{}",
                            entry.ramdisk_type, entry.name, entry.offset
                        );
                    }
                    write("vendor_ramdisk_table.txt", manifest.as_bytes())?;
                } else {
                    write("vendor_ramdisk", ramdisks.first().map(Vec::as_slice).unwrap_or(&[]))?;
                }
                write("dtb", dtb)?;
            }
        }
        Ok(files)
    }
}

fn parse_boot(data: &[u8]) -> Result<BootImage> {
    if data.len() < 48 {
        bail!("boot image is too small for a header");
    }
    let magic: [u8; 8] = data[..8].try_into().unwrap();
    if &magic != BOOT_MAGIC {
        bail!("bad boot magic");
    }

    // header_version is always at offset 40 for every header version.
    let header_version = u32_at(data, 40)?;
    if header_version > 4 {
        bail!("unsupported boot image header version {header_version}");
    }

    let kernel_size;
    let mut kernel_load_address = 0;
    let ramdisk_size;
    let mut ramdisk_load_address = 0;
    let mut second_size = 0;
    let mut second_load_address = 0;
    let mut tags_load_address = 0;
    let page_size;
    let os_version_raw;
    let mut header_size = 0;
    let cmdline;
    let mut extra_cmdline = String::new();
    let mut product_name = String::new();
    let mut recovery_dtbo_size = 0;
    let mut recovery_dtbo_offset = 0;
    let mut dtb_size = 0;
    let mut dtb_load_address = 0;
    let mut signature_size = 0;

    if header_version < 3 {
        kernel_size = u32_at(data, 8)?;
        kernel_load_address = u32_at(data, 12)?;
        ramdisk_size = u32_at(data, 16)?;
        ramdisk_load_address = u32_at(data, 20)?;
        second_size = u32_at(data, 24)?;
        second_load_address = u32_at(data, 28)?;
        tags_load_address = u32_at(data, 32)?;
        page_size = u32_at(data, 36)?;
        os_version_raw = u32_at(data, 44)?;
        if page_size == 0 || !page_size.is_power_of_two() {
            bail!("invalid boot image page size {page_size}");
        }
        product_name = cstr_at(data, 48, 16)?;
        cmdline = cstr_at(data, 64, 512)?;
        extra_cmdline = cstr_at(data, 608, 1024)?;
        if header_version >= 1 {
            recovery_dtbo_size = u32_at(data, 1632)?;
            recovery_dtbo_offset = u64_at(data, 1636)?;
            header_size = u32_at(data, 1644)?;
        }
        if header_version == 2 {
            dtb_size = u32_at(data, 1648)?;
            dtb_load_address = u64_at(data, 1652)?;
        }
    } else {
        kernel_size = u32_at(data, 8)?;
        ramdisk_size = u32_at(data, 12)?;
        os_version_raw = u32_at(data, 16)?;
        header_size = u32_at(data, 20)?;
        page_size = BOOT_IMAGE_HEADER_V3_PAGESIZE;
        cmdline = cstr_at(data, 44, 1536)?;
        if header_version >= 4 {
            if header_size < 1584 {
                bail!(
                    "header v4 image claims header_size {header_size}, expected at least 1584"
                );
            }
            signature_size = u32_at(data, 1580)?;
        }
    }

    let (os_version, os_patch_level) = decode_os_version_patch_level(os_version_raw);
    let header = BootHeader {
        magic,
        header_version,
        kernel_size,
        ramdisk_size,
        page_size,
        os_version_raw,
        os_version,
        os_patch_level,
        header_size,
        kernel_load_address,
        ramdisk_load_address,
        second_size,
        second_load_address,
        tags_load_address,
        product_name,
        cmdline,
        extra_cmdline,
        recovery_dtbo_size,
        recovery_dtbo_offset,
        dtb_size,
        dtb_load_address,
        signature_size,
    };

    let page = page_size as u64;
    let kernel_pages = pages(kernel_size as u64, page);
    let ramdisk_pages = pages(ramdisk_size as u64, page);
    let second_pages = pages(second_size as u64, page);
    let header_pages = 1u64;

    let kernel = read_at(data, page * header_pages, kernel_size as u64)?;
    let ramdisk = read_at(data, page * (header_pages + kernel_pages), ramdisk_size as u64)?;
    let second = if second_size > 0 {
        read_at(
            data,
            page * (header_pages + kernel_pages + ramdisk_pages),
            second_size as u64,
        )?
    } else {
        Vec::new()
    };
    let recovery_dtbo = if recovery_dtbo_size > 0 {
        read_at(data, recovery_dtbo_offset, recovery_dtbo_size as u64)?
    } else {
        Vec::new()
    };
    let dtb = if dtb_size > 0 && header_version == 2 {
        let dtb_offset = page
            * (header_pages
                + kernel_pages
                + ramdisk_pages
                + second_pages
                + pages(recovery_dtbo_size as u64, page));
        read_at(data, dtb_offset, dtb_size as u64)?
    } else {
        Vec::new()
    };
    let signature = if signature_size > 0 {
        let off = page * (header_pages + kernel_pages + ramdisk_pages);
        read_at(data, off, signature_size as u64)?
    } else {
        Vec::new()
    };

    Ok(BootImage::Boot {
        header: Box::new(header),
        kernel,
        ramdisk,
        second,
        recovery_dtbo,
        dtb,
        signature,
    })
}

fn parse_vendor_boot(data: &[u8]) -> Result<BootImage> {
    let mut cur = Cursor::new(data);
    let magic = read_array::<8>(&mut cur)?;
    if &magic != VENDOR_BOOT_MAGIC {
        bail!("bad vendor_boot magic");
    }
    let header_version = read_u32(&mut cur)?;
    let page_size = read_u32(&mut cur)?;
    let kernel_load_address = read_u32(&mut cur)?;
    let ramdisk_load_address = read_u32(&mut cur)?;
    let vendor_ramdisk_size = read_u32(&mut cur)?;
    let cmdline = read_cstr(&mut cur, 2048)?;
    let tags_load_address = read_u32(&mut cur)?;
    let product_name = read_cstr(&mut cur, 16)?;
    let header_size = read_u32(&mut cur)?;
    let dtb_size = read_u32(&mut cur)?;
    let dtb_load_address = read_u64(&mut cur)?;
    if page_size == 0 || !page_size.is_power_of_two() {
        bail!("invalid vendor_boot page size {page_size}");
    }

    let mut vendor_ramdisk_table_size = 0;
    let mut vendor_ramdisk_table_entry_num = 0;
    let mut vendor_ramdisk_table_entry_size = 0;
    let mut vendor_bootconfig_size = 0;
    let mut ramdisk_table = Vec::new();

    if header_version > 3 {
        vendor_ramdisk_table_size = read_u32(&mut cur)?;
        vendor_ramdisk_table_entry_num = read_u32(&mut cur)?;
        vendor_ramdisk_table_entry_size = read_u32(&mut cur)?;
        vendor_bootconfig_size = read_u32(&mut cur)?;

        if vendor_ramdisk_table_entry_size < 108 {
            bail!(
                "v4 vendor ramdisk table entry size {vendor_ramdisk_table_entry_size} is smaller than the 108-byte layout"
            );
        }

        let page = page_size as u64;
        let header_pages = pages(header_size as u64, page);
        let ramdisk_base = page * header_pages;
        let dtb_offset = ramdisk_base + page * pages(vendor_ramdisk_size as u64, page);
        let table_offset = dtb_offset + page * pages(dtb_size as u64, page);
        if vendor_ramdisk_table_entry_num > 0 {
            let need = vendor_ramdisk_table_entry_num as u64
                * vendor_ramdisk_table_entry_size as u64;
            if table_offset + need > data.len() as u64 {
                bail!(
                    "vendor ramdisk table extends past end of image (offset {table_offset}, need {need}, image {})",
                    data.len()
                );
            }
        }
        cur.seek(SeekFrom::Start(table_offset))?;
        for _ in 0..vendor_ramdisk_table_entry_num {
            let entry_start = cur.position();
            let size = read_u32(&mut cur)?;
            let offset = read_u32(&mut cur)?;
            let ramdisk_type = read_u32(&mut cur)?;
            let name = read_cstr(&mut cur, 32)?;
            let mut board_id = [0u32; 16];
            for id in &mut board_id {
                *id = read_u32(&mut cur)?;
            }
            let consumed = cur.position() - entry_start;
            if consumed < vendor_ramdisk_table_entry_size as u64 {
                cur.seek(SeekFrom::Start(
                    entry_start + vendor_ramdisk_table_entry_size as u64,
                ))?;
            }
            ramdisk_table.push(VendorRamdiskEntry {
                size,
                offset,
                ramdisk_type,
                name,
                board_id,
            });
        }
    }

    let page = page_size as u64;
    let header_pages = pages(header_size as u64, page);
    let ramdisk_base = page * header_pages;

    let mut ramdisks = Vec::new();
    if header_version > 3 {
        for entry in &ramdisk_table {
            ramdisks.push(read_at(
                data,
                ramdisk_base + entry.offset as u64,
                entry.size as u64,
            )?);
        }
    } else {
        ramdisks.push(read_at(data, ramdisk_base, vendor_ramdisk_size as u64)?);
    }

    let dtb_offset = ramdisk_base + page * pages(vendor_ramdisk_size as u64, page);
    let dtb = read_at(data, dtb_offset, dtb_size as u64)?;

    let bootconfig = if header_version > 3 {
        let table_offset = dtb_offset + page * pages(dtb_size as u64, page);
        let table_pages = pages(vendor_ramdisk_table_size as u64, page);
        let bootconfig_offset = table_offset + page * table_pages;
        read_at(data, bootconfig_offset, vendor_bootconfig_size as u64)?
    } else {
        Vec::new()
    };

    Ok(BootImage::VendorBoot {
        header: Box::new(VendorBootHeader {
            magic,
            header_version,
            page_size,
            kernel_load_address,
            ramdisk_load_address,
            vendor_ramdisk_size,
            cmdline,
            tags_load_address,
            product_name,
            header_size,
            dtb_size,
            dtb_load_address,
            vendor_ramdisk_table_size,
            vendor_ramdisk_table_entry_num,
            vendor_ramdisk_table_entry_size,
            vendor_bootconfig_size,
            ramdisk_table,
        }),
        ramdisks,
        dtb,
        bootconfig,
    })
}

pub fn decode_os_version_patch_level(raw: u32) -> (Option<String>, Option<String>) {
    let os_version = raw >> 11;
    let os_patch_level = raw & ((1 << 11) - 1);
    let version = if os_version == 0 {
        None
    } else {
        let a = os_version >> 14;
        let b = (os_version >> 7) & ((1 << 7) - 1);
        let c = os_version & ((1 << 7) - 1);
        Some(format!("{a}.{b}.{c}"))
    };
    let patch = if os_patch_level == 0 {
        None
    } else {
        let y = (os_patch_level >> 4) + 2000;
        let m = os_patch_level & ((1 << 4) - 1);
        Some(format!("{y:04}-{m:02}"))
    };
    (version, patch)
}

fn pages(size: u64, page: u64) -> u64 {
    size.div_ceil(page)
}

fn read_at(data: &[u8], offset: u64, size: u64) -> Result<Vec<u8>> {
    let end = offset
        .checked_add(size)
        .context("offset overflow while reading boot image")?;
    if end > data.len() as u64 {
        bail!(
            "boot image section at offset {offset} with size {size} exceeds image size {}",
            data.len()
        );
    }
    Ok(data[offset as usize..end as usize].to_vec())
}

fn u32_at(data: &[u8], offset: usize) -> Result<u32> {
    let end = offset.checked_add(4).context("offset overflow")?;
    if end > data.len() {
        bail!("boot image field at offset {offset} is out of bounds");
    }
    Ok(u32::from_le_bytes(data[offset..end].try_into().unwrap()))
}

fn u64_at(data: &[u8], offset: usize) -> Result<u64> {
    let end = offset.checked_add(8).context("offset overflow")?;
    if end > data.len() {
        bail!("boot image field at offset {offset} is out of bounds");
    }
    Ok(u64::from_le_bytes(data[offset..end].try_into().unwrap()))
}

fn cstr_at(data: &[u8], offset: usize, size: usize) -> Result<String> {
    let end = offset.checked_add(size).context("offset overflow")?;
    if end > data.len() {
        bail!("boot image string at offset {offset} is out of bounds");
    }
    Ok(cstr(&data[offset..end]))
}

fn read_array<const N: usize>(cur: &mut Cursor<&[u8]>) -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    cur.read_exact(&mut buf).context("unexpected end of image")?;
    Ok(buf)
}

fn read_u32(cur: &mut Cursor<&[u8]>) -> Result<u32> {
    Ok(u32::from_le_bytes(read_array::<4>(cur)?))
}

fn read_u64(cur: &mut Cursor<&[u8]>) -> Result<u64> {
    Ok(u64::from_le_bytes(read_array::<8>(cur)?))
}

fn read_cstr(cur: &mut Cursor<&[u8]>, size: usize) -> Result<String> {
    let bytes = read_array_vec(cur, size)?;
    Ok(cstr(&bytes))
}

fn read_array_vec(cur: &mut Cursor<&[u8]>, size: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; size];
    cur.read_exact(&mut buf).context("unexpected end of image")?;
    Ok(buf)
}

fn cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Split a dtbo image (AOSP `mkdtboimg` format) into individual DTBs.
/// Falls back to scanning for FDT magic if the header is not the standard
/// layout, which is what many vendor dtbo images use.
pub fn split_dtbo(path: &Path, out_dir: &Path) -> Result<Vec<ExtractedFile>> {
    let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    fs::create_dir_all(out_dir)?;
    let mut files = Vec::new();
    let mut dtbs: Vec<(u64, u64)> = Vec::new();

    if data.len() >= 32 && data[0..4] == [0xd7, 0xb7, 0xab, 0x1e] {
        let total_size = u32::from_be_bytes(data[4..8].try_into().unwrap()) as u64;
        let header_size = u32::from_be_bytes(data[8..12].try_into().unwrap()) as u64;
        let entry_size = u32::from_be_bytes(data[12..16].try_into().unwrap()) as u64;
        let entry_count = u32::from_be_bytes(data[16..20].try_into().unwrap()) as u64;
        let entries_offset = u32::from_be_bytes(data[20..24].try_into().unwrap()) as u64;
        if header_size >= 32
            && entry_size >= 16
            && entries_offset >= 32
            && entries_offset + entry_count * entry_size <= data.len() as u64
        {
            for i in 0..entry_count {
                let off = entries_offset + i * entry_size;
                let size = u32::from_be_bytes(data[off as usize..off as usize + 4].try_into().unwrap())
                    as u64;
                let dt_off = u32::from_be_bytes(
                    data[off as usize + 4..off as usize + 8].try_into().unwrap(),
                ) as u64;
                if size > 0 && dt_off + size <= data.len() as u64 {
                    dtbs.push((dt_off, size));
                }
            }
        }
        let _ = total_size;
    }

    if dtbs.is_empty() {
        // Generic fallback: find big-endian FDT magic and read totalsize.
        let magic = [0xd0, 0x0d, 0xfe, 0xed];
        let mut i = 0usize;
        while i + 8 <= data.len() {
            if data[i..i + 4] == magic {
                let size = u32::from_be_bytes(data[i + 4..i + 8].try_into().unwrap()) as u64;
                if size >= 8 && i as u64 + size <= data.len() as u64 {
                    dtbs.push((i as u64, size));
                    i += size as usize;
                    continue;
                }
            }
            i += 4;
        }
    }

    for (i, (off, size)) in dtbs.iter().enumerate() {
        let name = format!("{i:02}_dtb.dtb");
        let bytes = &data[*off as usize..(*off + *size) as usize];
        fs::write(out_dir.join(&name), bytes)?;
        files.push(ExtractedFile {
            name,
            size: *size,
            offset: *off,
        });
    }
    Ok(files)
}

pub fn is_boot_image(data: &[u8]) -> bool {
    data.len() >= 8 && (&data[..8] == BOOT_MAGIC || &data[..8] == VENDOR_BOOT_MAGIC)
}

pub fn is_dtbo(data: &[u8]) -> bool {
    data.len() >= 8 && (data[0..4] == [0xd7, 0xb7, 0xab, 0x1e] || data[4..8] == [0xd0, 0x0d, 0xfe, 0xed])
}

pub fn vendored_dtc() -> Option<PathBuf> {
    let candidates = ["dtc", "/usr/bin/dtc"];
    for c in candidates {
        if let Ok(path) = which(c) {
            return Some(path);
        }
    }
    None
}

fn which(name: &str) -> Result<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        if p.exists() {
            return Ok(p);
        }
        bail!("{name} not found");
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        if p.is_file() {
            return Ok(p);
        }
    }
    bail!("{name} not found in PATH")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_u32(buf: &mut Vec<u8>, v: u32) {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    fn put_u64(buf: &mut Vec<u8>, v: u64) {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    fn build_boot_v4(kernel: &[u8], ramdisk: &[u8], signature: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(BOOT_MAGIC);
        put_u32(&mut buf, kernel.len() as u32);
        put_u32(&mut buf, ramdisk.len() as u32);
        put_u32(&mut buf, 0);
        put_u32(&mut buf, 1584);
        for _ in 0..4 {
            put_u32(&mut buf, 0);
        }
        put_u32(&mut buf, 4);
        let mut cmdline = [0u8; 1536];
        cmdline[..4].copy_from_slice(b"test");
        buf.extend_from_slice(&cmdline);
        put_u32(&mut buf, signature.len() as u32);
        buf.resize(4096, 0);
        let kp = pages(kernel.len() as u64, 4096) as usize;
        buf.extend_from_slice(kernel);
        buf.resize(4096 * (1 + kp), 0);
        buf.extend_from_slice(ramdisk);
        let rp = pages(ramdisk.len() as u64, 4096) as usize;
        buf.resize(4096 * (1 + kp + rp), 0);
        buf.extend_from_slice(signature);
        buf
    }

    #[test]
    fn parses_boot_v4_with_signature() {
        let img = build_boot_v4(b"kernel-data", b"ramdisk-data", b"sig");
        let parsed = BootImage::parse_bytes(&img).unwrap();
        assert_eq!(parsed.header_version(), 4);
        match parsed {
            BootImage::Boot { header, kernel, ramdisk, signature, .. } => {
                assert_eq!(header.header_size, 1584);
                assert_eq!(header.signature_size, 3);
                assert_eq!(kernel, b"kernel-data");
                assert_eq!(ramdisk, b"ramdisk-data");
                assert_eq!(signature, b"sig");
                assert!(header.cmdline.starts_with("test"));
            }
            _ => panic!("wrong kind"),
        }
    }

    #[test]
    fn parses_legacy_v0_header() {
        let page = 2048u32;
        let mut buf = Vec::new();
        buf.extend_from_slice(BOOT_MAGIC);
        put_u32(&mut buf, 4); // kernel
        put_u32(&mut buf, 0x10008000);
        put_u32(&mut buf, 3); // ramdisk
        put_u32(&mut buf, 0x11000000);
        put_u32(&mut buf, 0);
        put_u32(&mut buf, 0);
        put_u32(&mut buf, 0x10000100);
        put_u32(&mut buf, page);
        put_u32(&mut buf, 0); // v0
        put_u32(&mut buf, 0); // os version
        let mut name = [0u8; 16];
        name[..4].copy_from_slice(b"test");
        buf.extend_from_slice(&name);
        let mut cmd = [0u8; 512];
        cmd[..2].copy_from_slice(b"-c");
        buf.extend_from_slice(&cmd);
        buf.resize(buf.len() + 32 + 1024, 0);
        buf.resize(page as usize, 0);
        buf.extend_from_slice(b"kern");
        buf.resize(page as usize * 2, 0);
        buf.extend_from_slice(b"ram");
        let parsed = BootImage::parse_bytes(&buf).unwrap();
        match parsed {
            BootImage::Boot { header, kernel, ramdisk, .. } => {
                assert_eq!(header.header_version, 0);
                assert_eq!(kernel, b"kern");
                assert_eq!(ramdisk, b"ram");
            }
            _ => panic!("wrong kind"),
        }
    }

    fn build_vendor_v4() -> Vec<u8> {
        let page = 4096usize;
        let r0 = b"platform-ramdisk";
        let r1 = b"recovery-ramdisk";
        let dtb = b"dtb-data";
        let bootconfig = b"kernel.rcu_nocbs=all\n";
        let table_size = 108 * 2;
        let total_ramdisk = r0.len() + r1.len();
        let mut header = Vec::new();
        header.extend_from_slice(VENDOR_BOOT_MAGIC);
        put_u32(&mut header, 4);
        put_u32(&mut header, page as u32);
        put_u32(&mut header, 0x40000000);
        put_u32(&mut header, 0x66f00000);
        put_u32(&mut header, total_ramdisk as u32);
        header.resize(8 + 4 * 5 + 2048, 0);
        put_u32(&mut header, 0x47c80000);
        header.resize(8 + 4 * 5 + 2048 + 4 + 16, 0);
        put_u32(&mut header, 2128);
        put_u32(&mut header, dtb.len() as u32);
        put_u64(&mut header, 0x47c80000);
        put_u32(&mut header, table_size as u32);
        put_u32(&mut header, 2);
        put_u32(&mut header, 108);
        put_u32(&mut header, bootconfig.len() as u32);
        header.resize(page, 0);

        let mut table = Vec::new();
        for (size, off, ty, name) in [
            (r0.len() as u32, 0u32, 1u32, "platform"),
            (r1.len() as u32, r0.len() as u32, 2u32, "recovery"),
        ] {
            put_u32(&mut table, size);
            put_u32(&mut table, off);
            put_u32(&mut table, ty);
            let mut n = [0u8; 32];
            n[..name.len()].copy_from_slice(name.as_bytes());
            table.extend_from_slice(&n);
            for _ in 0..16 {
                put_u32(&mut table, 0);
            }
        }

        let mut buf = header;
        buf.extend_from_slice(r0);
        buf.extend_from_slice(r1);
        buf.resize(page + page * pages(total_ramdisk as u64, page as u64) as usize, 0);
        buf.extend_from_slice(dtb);
        buf.resize(buf.len() + (page - (buf.len() % page)) % page, 0);
        buf.extend_from_slice(&table);
        buf.resize(buf.len() + (page - (buf.len() % page)) % page, 0);
        buf.extend_from_slice(bootconfig);
        buf
    }

    #[test]
    fn parses_vendor_boot_v4_fragments_and_bootconfig() {
        let img = build_vendor_v4();
        let parsed = BootImage::parse_bytes(&img).unwrap();
        match parsed {
            BootImage::VendorBoot { header, ramdisks, bootconfig, .. } => {
                assert_eq!(header.header_size, 2128);
                assert_eq!(header.vendor_ramdisk_table_entry_num, 2);
                assert_eq!(ramdisks.len(), 2);
                assert_eq!(ramdisks[0], b"platform-ramdisk");
                assert_eq!(ramdisks[1], b"recovery-ramdisk");
                assert_eq!(header.ramdisk_table[1].name, "recovery");
                assert_eq!(bootconfig, b"kernel.rcu_nocbs=all\n");
            }
            _ => panic!("wrong kind"),
        }
    }

    #[test]
    fn os_version_decoding() {
        let (v, p) = decode_os_version_patch_level(0);
        assert_eq!(v, None);
        assert_eq!(p, None);
        let raw = (15u32 << 11) | ((2024 - 2000) << 4) | 10;
        let (v, p) = decode_os_version_patch_level(raw);
        assert_eq!(v.as_deref(), Some("0.0.15"));
        assert_eq!(p.as_deref(), Some("2024-10"));
    }
}
