#!/bin/bash

# Build the fully standalone DumprX binary.
#
# The result is a single static x86_64 Linux ELF with an embedded runtime
# (helper tools, shared libraries and portable Python).  No source tree,
# utils/ directory or host tools are needed after this completes.

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null && pwd)"
TARGET="x86_64-unknown-linux-musl"
BIN="${PROJECT_DIR}/target/${TARGET}/release/dumprx"
DIST="${PROJECT_DIR}/dist"

cd "${PROJECT_DIR}"

if ! rustup target list --installed 2>/dev/null | grep -qx "${TARGET}"; then
	echo "[ERROR] Rust target ${TARGET} is not installed." >&2
	echo "        Install it once with: rustup target add ${TARGET}" >&2
	exit 1
fi
if [[ ! -f "${PROJECT_DIR}/assets/runtime.tar.gz" ]]; then
	echo "[ERROR] assets/runtime.tar.gz is missing." >&2
	echo "        Regenerate it with tools/make-runtime.sh on a machine with the helper tools." >&2
	exit 1
fi

cargo build --offline --release --target "${TARGET}"
mkdir -p "${DIST}"
cp -f "${BIN}" "${DIST}/dumprx"
chmod +x "${DIST}/dumprx"

echo
echo "Standalone binary: ${DIST}/dumprx"
file "${DIST}/dumprx"
ls -lh "${DIST}/dumprx"
