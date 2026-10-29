# DumprX Rust port — status and roadmap

## Completed

- [x] Rust workspace, offline Cargo vendoring (`vendor/`), no runtime downloads.
- [x] Native boot/recovery/init_boot headers v0–v4.
- [x] Native vendor_boot v3/v4 (ramdisk fragments, bootconfig, signature size).
- [x] Native liblp `super.img` metadata parser and partition extractor.
- [x] Native Android sparse image extractor (`simg2img` replacement).
- [x] Native cpio ramdisk extraction + gzip/lz4/xz/lzma/bzip2/zstd decompression.
- [x] Strict fstab parser (4/5/6-field, CRLF, tabs, escapes, duplicates) with
      `fstab-report.json` and file/line diagnostics.
- [x] build.prop parsing and README metadata.
- [x] Native `ikconfig` extraction.
- [x] Native `sdat2img` for A-only block OTAs.
- [x] Native `payload.bin` parser (REPLACE/BZ/XZ/ZSTD/ZERO/DISCARD).
- [x] Native `UPDATE.APP` extractor.
- [x] Native Spreadtrum `.pac` extractor.
- [x] Native proprietary-files classifier (validated against android_tools).
- [x] Native SHA-1 manifests.
- [x] Native TWRP/AOSP device-tree generation with multi-image selection.
- [x] Offline dispatcher for the legacy vendor formats listed below.
- [x] `dumper.sh` compatibility wrapper; `setup.sh` never downloads.

## Legacy bridge still using vendored scripts

These run offline against the host's installed Python packages. They are the
next porting targets:

- [ ] `ozip` (oppo_ozip_decrypt)
- [ ] `ofp` (oppo_decrypt QC/MTK)
- [ ] `ops` (opscrypto)
- [ ] `kdz` / `dz` (kdztools)
- [ ] `sin` (unsin binary)
- [ ] `nb0` (nb0-extract binary)
- [ ] HTC RUU (RUU_Decrypt_Tool binary)
- [ ] Rockchip (rkImageMaker/afptool binaries)
- [ ] Amlogic (aml-upgrade-package-extract binary)

## Other follow-ups

- [ ] Optional GitHub/GitLab/Telegram publishing (the online shell path was
      intentionally removed; DumprX currently writes a local dump only).
- [ ] Native `dtc` replacement (currently host `dtc`).
- [ ] Optional kernel symbol recovery (`kallsyms.txt` / `boot.elf`).
- [ ] Repack support (boot/vendor_boot/super) on top of the native parsers.
- [ ] Integration tests with synthetic boot/super/fstab fixtures in CI.
- [ ] Reduce the remaining vendored crates (hand-rolled CLI/JSON if desired).
