<div align="center">

  <h1>DumprX</h1>

  <h4>Offline Android firmware dumper — Rust rewrite</h4>

</div>

DumprX extracts Android firmware (super images, boot/vendor_boot/init_boot,
dtbo, OTA payloads, block-based OTA `*.new.dat`, archives and several
vendor-specific formats) into a dump tree suitable for a device-tree/ROM
repository.

This tree is a **Rust conversion** of the original shell/Python DumprX. The
conversion is incremental: the core dump path is native Rust, while a few
vendor-specific decryptors are still invoked through vendored scripts (see
[Legacy bridge](#legacy-bridge)). Nothing is downloaded at runtime, and
`cargo build --offline` works with the vendored crates in `vendor/`.

## What changed in the Rust port

### 1. Fully offline

The old `dumper.sh` cloned four GitHub repositories, curled
`extract-ikconfig`, and used `uv`/`uvx` to fetch Python packages and tools
from the network on every run. All of that is gone:

- Every helper script and binary used at runtime is vendored in `utils/`.
- Cargo dependencies are vendored in `vendor/` with `.cargo/config.toml`.
- `dumper.sh` is a thin wrapper around `target/release/dumprx`; it builds
  with `cargo build --offline` and executes the Rust binary.
- `setup.sh` only checks the host tools and builds; it never downloads.
- `dumprx` itself has no downloader code path. URL inputs and the old
  GitHub/GitLab/Telegram push helpers were removed; pass a local file,
  archive, or extracted directory and DumprX writes a local dump.

### 2. Boot image header v4 support

The old `utils/bootimg_info.py` only implemented header v3. The Rust
`bootimg` module implements the full AOSP layout for:

- boot/recovery/init_boot headers **v0, v1, v2, v3 and v4**
- vendor_boot headers **v3 and v4**, including:
  - the v4 vendor ramdisk table (`vendor_ramdisk00`, `vendor_ramdisk01`, …)
  - ramdisk type/name/board_id fields
  - the v4 `bootconfig` section
  - the v4 boot signature size and `boot_signature` payload
- OS version / patch level decoding
- `info` output with all v4 fields, and `unpack-boot` extraction

`dumprx info <image>` and `dumprx unpack-boot <image> -o <dir>` are the
replacements for `utils/bootimg_info.py`.

### 3. Fstab layouts are parsed strictly, never silently

The old pipeline could drop entries when an fstab used a layout it did not
expect. The Rust fstab parser accepts:

- Android 5-field entries: `source mount_point type mount_flags fs_mgr_flags`
- 4-field entries that omit the optional `fs_mgr_flags` column
- Linux 6-field entries: `source mount_point type options dump pass`
- 5-field lines with a trailing pass number
- tabs, CRLF line endings, comments, blank lines
- escaped whitespace (`\040`, `\011`, `\\`)
- duplicate entries for alternative filesystems (erofs + ext4), which are
  kept as candidates instead of being collapsed

Malformed lines are reported with `file:line` in the dump log and in
`out/fstab-report.json` (the process prints every error). `dumprx fstab
<path…>` can be used standalone; it exits non-zero on parse errors.

## Build

```bash
cargo build --offline --release
```

or:

```bash
./setup.sh
```

`setup.sh` checks for `cargo`, `python3` (legacy bridge), `dtc`, `lz4`, `xz`,
`bzip2`, `zstd` and `brotli`, and builds the binary. The 7-Zip, EROFS,
F2FS, magiskboot, RUU, nb0, unsin and Amlogic tools are already in
`utils/`. `payload-dumper-go` is kept only as a fallback for payload
manifests the native parser does not understand yet.

## Usage

```bash
# Firmware file, archive, or an already-extracted directory
./dumper.sh /path/to/firmware

# or directly
./target/release/dumprx dump /path/to/firmware -o out

# Only press the super partitions you need (faster testing)
./target/release/dumprx dump firmware/ -o out -p vendor -p odm
```

Useful subcommands:

```bash
dumprx info boot.img
dumprx unpack-boot vendor_boot.img -o out/vendor_boot
dumprx split-dtbo dtbo.img -o out/dtbo
dumprx super-list super.img
dumprx super-extract super.img -o out -p vendor
dumprx unsparse system.img
dumprx fstab out/vendor/etc/fstab.mt6789
dumprx device-tree out              # generate both TWRP and AOSP trees
dumprx device-tree out --twrp       # only TWRP
dumprx device-tree out --aosp       # only AOSP/LineageOS
dumprx proprietary out              # regenerate proprietary-files.{txt,sha1}
```

`dumprx dump` options:

```text
-o, --out <DIR>          output directory (default: out)
-P, --partition <NAME>   only extract these super partitions (repeatable)
    --keep-images        keep partition images after filesystem extraction
    --no-device-tree     skip TWRP/AOSP device-tree generation
    --twrp               generate the TWRP tree
    --aosp               generate the AOSP/LineageOS tree
    --strict-fstab       make fstab parse errors fatal
-v, --verbose            log external commands
```

## Dump output

```text
out/
├── README.md
├── all_files.txt
├── all_files.sha1
├── proprietary-files.txt
├── proprietary-files.sha1
├── fstab-report.json
├── super-info.json              # liblp metadata summary (when a super exists)
├── ikconfig                    # when the kernel embeds CONFIG_IKCONFIG
├── boot.img / boot/raw/...     # kernel, ramdisk, dtb, bootconfig
├── vendor_boot.img / vendor_boot/ramdisk/<fragment>
├── init_boot.img / init_boot/ramdisk
├── recovery.img / recovery/...
├── dtbo.img / dtbo/dtb / dtbo/dts
├── system/ system_ext/ vendor/ odm/ product/ ...
└── aosp-device-tree/  twrp-device-tree/
```

## Device trees and image selection

The Rust device-tree generator replaces `twrpdtgen`/`aospdtgen`. It inspects
**all** available images and reports exactly which one supplied each piece:

```text
[*] device-tree image selection:
[*]   kernel: out/boot/raw/kernel (boot.img)
[*]   ramdisk: out/vendor_boot/ramdisk/vendor_ramdisk00 (vendor_boot.img:vendor_ramdisk00)
[*]   ramdisk: out/init_boot/ramdisk (init_boot.img)
[*]   dtb: out/vendor_boot/raw/dtb (vendor_boot.img)
[*]   dtbo: out/dtbo.img
```

- Kernel is taken from `boot.img`, falling back to `vendor_kernel_boot.img`.
- Ramdisks are collected from boot, init_boot, vendor_boot fragments and
  recovery, whichever exist.
- DTB/DTBO are taken from boot/vendor_boot/dtbo when present.
- Fstab is selected from `vendor/etc`, `odm/etc`, `system/etc`, or the first
  stage ramdisk, in that preference order.
- If a ramdisk or kernel is missing, the generator emits a warning, records
  it in the tree README, and still produces the tree. It never asserts or
  exits silently.
- TWRP `recovery.fstab` is derived from the dump fstab; rootdir `*.rc` files
  and `default.prop` are copied from the available ramdisks.

AOSP tree output: `BoardConfig.mk`, `device.mk`, `Android.bp`,
`Android.mk`, `AndroidProducts.mk`, `lineage_<device>.mk`, `rootdir/etc/…`,
`prebuilts/{kernel,dtb.img,dtbo.img}` and partition `.prop` files.

TWRP tree output: `BoardConfig.mk`, `device.mk`, `Android.bp`, `Android.mk`,
`AndroidProducts.mk`, `omni_<device>.mk`, `recovery.fstab`, `recovery/root`,
`prebuilt/{kernel,dtb.img,dtbo.img}` and `vendorsetup.sh`.

## Supported formats

Native Rust:

| Format | How |
| --- | --- |
| Android sparse images | `sparse.rs` (raw/fill/dont-care chunks) |
| `super.img` / liblp | `superimg.rs` (metadata v10.x, virtual A/B) |
| boot/recovery/init_boot v0–v4 | `bootimg.rs` |
| vendor_boot v3/v4 | `bootimg.rs` (ramdisk table + bootconfig) |
| dtbo / DTB | `bootimg::split_dtbo` + `dtc` |
| ramdisk cpio | `cpio.rs` (newc/crc, symlinks, hardlinks, traversal guard) |
| ramdisk compression | gzip, lz4 legacy/frame, xz, lzma, bzip2, zstd |
| EROFS / ext4 / F2FS | `fsimage.rs` + bundled extractors |
| A-only `*.new.dat` OTA | `sdat.rs` (`sdat2img` replacement) |
| `payload.bin` OTA | `payload.rs` (native manifest parser, Go fallback) |
| `UPDATE.APP` | `uapp.rs` |
| Spreadtrum `*.pac` | `pac.rs` |
| zip/rar/7z/tar | bundled `7zz` |
| build.prop / metadata | `props.rs` |
| proprietary-files | `proprietary.rs` + generated rules |
| SHA-1 manifests | `sha1sum.rs` |
| fstab | `fstab.rs` |
| device trees | `device_tree.rs` |

## Legacy bridge

A few vendor-specific decryptors are still invoked through their vendored
scripts because they have not been ported yet. They run **offline** and only
use the Python packages installed by the host:

- `ozip` → `utils/vendor/oppo_ozip_decrypt`
- `ofp`, `ops` → `utils/vendor/oppo_decrypt`
- `kdz`/`dz` → `utils/kdztools`
- `nb0`, `sin`, RUU, Rockchip, Amlogic → bundled binaries

These paths report a clear error if a required helper is missing; they never
download one. Porting them to Rust is the next milestone.

## Repository layout

```text
Cargo.toml              Rust package
src/
  main.rs               CLI (clap)
  pipeline.rs           dump orchestration
  bootimg.rs            boot/vendor_boot v0–v4
  superimg.rs           liblp parser/extractor
  sparse.rs             simg2img replacement
  fsimage.rs            filesystem detection/extraction
  ramdisk.rs, cpio.rs   ramdisk decompression + cpio
  fstab.rs              strict fstab parser
  props.rs              build.prop/metadata
  device_tree.rs        TWRP/AOSP tree generation
  proprietary.rs        blob classifier
  legacy.rs             offline bridge for vendor formats
  sdat.rs, ikconfig.rs, sha1sum.rs, archive.rs, payload.rs, uapp.rs, pac.rs
utils/                  vendored tools/scripts
vendor/                 vendored Cargo crates
```

## Credits

DumprX is based on Phoenix Firmware Dumper from DroidDumps and dumpyara from
AndroidDumps. The original shell/Python implementation and its third-party
helpers are credited below.

- sdat2img.py — @xpirt, @luxi78, @howellzhu
- simg2img — @anestisb
- unsin — @IgorEisberg
- extract_android_ota_payload / payload-dumper-go — @cyxx, @ssut
- extract-dtb.py — @PabloCastellano
- dtc — kernel.org
- vmlinux-to-elf — @marin-m
- ozipdecrypt / ofp / ops tools — @bkerler
- lpunpack — @LonelyFool
- splituapp — @superr
- pacextractor — @HemanthJabalpuri
- nb0-extract — @Eddie07 / FIH mobile
- kdztools — IOMonster, @ehem, @steadfasterX
- RUU_Decrypt_Tool — @nkk71, @CaptainThrowback
- extract-ikconfig — @torvalds / Linux kernel
- android_tools proprietary-files rules — @ShivamKumarJha
- twrpdtgen / aospdtgen ideas — @SebaUbuntu (the Python tools are replaced by
  the native generator in this tree)
- Original DumprX — @Sushrut1101 and contributors

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
