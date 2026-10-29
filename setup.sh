#!/bin/bash

# DumprX environment check.  Everything used by the Rust binary is either
# vendored in this repository or installed here.  This script never downloads
# anything.

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null && pwd)"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NORMAL='\033[0m'

missing=0
check() {
	local name="$1" hint="$2"
	if command -v "${name}" >/dev/null 2>&1; then
		printf "  [ok]   %s\n" "${name}"
	else
		printf "  ${RED}[miss]${NORMAL} %-12s %s\n" "${name}" "${hint}"
		missing=1
	fi
}

check_file() {
	local path="$1"
	if [[ -x "${PROJECT_DIR}/${path}" ]]; then
		printf "  [ok]   %s\n" "${path}"
	else
		printf "  ${RED}[miss]${NORMAL} %s\n" "${path}"
		missing=1
	fi
}

echo "DumprX (Rust) environment check"
echo

echo "Build / runtime:"
check cargo "install the Rust toolchain (rustup.rs)"
check python3 "required only for the legacy vendor-format bridge"

echo
echo "Compression / filesystem helpers:"
check dtc "device tree compiler (device-tree dts output)"
check lz4 "LZ4 ramdisk decompression"
check xz "XZ/LZMA ramdisk and sdat decompression"
check bzip2 "bzip2 ramdisk decompression"
check zstd "zstd ramdisk decompression"
check brotli "brotli .new.dat decompression"

echo
echo "Bundled tools:"
check_file "utils/bin/7zz"
check_file "utils/bin/fsck.erofs"
check_file "utils/bin/f2fs-extractor"
check_file "utils/bin/payload-dumper-go"
check_file "utils/bin/magiskboot"
check_file "utils/RUU_Decrypt_Tool"
check_file "utils/nb0-extract"
check_file "utils/unsin"
check_file "utils/aml-upgrade-package-extract"

if [[ ${missing} -ne 0 ]]; then
	echo
	printf "${YELLOW}Some optional tools are missing. Core dumps still work; the affected format paths will report a clear error.${NORMAL}\n"
fi

echo
echo "Building standalone dumprx (offline)..."
if rustup target list --installed 2>/dev/null | grep -qx "x86_64-unknown-linux-musl"; then
	( cd "${PROJECT_DIR}" && ./build-standalone.sh )
else
	echo "  musl target not installed; building the glibc release instead."
	( cd "${PROJECT_DIR}" && cargo build --offline --release )
fi

echo
printf "${GREEN}Setup complete.${NORMAL}\n"
echo "Run: ./dumper.sh <firmware file, archive or extracted dump directory>"
echo "Standalone binary (if built): ${PROJECT_DIR}/dist/dumprx"
