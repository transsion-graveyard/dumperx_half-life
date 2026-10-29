#!/bin/bash

# Build the runtime archive embedded into standalone DumprX builds.
# This script is only needed when updating the embedded tools/Python runtime;
# normal `cargo build` uses the committed assets/runtime.tar.gz.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." >/dev/null && pwd)"
STAGE="${ROOT}/.work/runtime-stage"
OUT="${ROOT}/assets/runtime.tar.gz"

rm -rf "${STAGE}"
mkdir -p "${STAGE}"/{utils,lib,python/lib}
cp -a "${ROOT}/utils/." "${STAGE}/utils/"

# Standard tools that used to be host dependencies.
for tool in dtc lz4 xz bzip2 zstd brotli; do
	path="$(command -v "${tool}")"
	cp -L "${path}" "${STAGE}/utils/bin/${tool}"
	chmod +x "${STAGE}/utils/bin/${tool}"
done

# Portable Python for the legacy vendor-format bridge.
cp -L /usr/bin/python3.14 "${STAGE}/utils/bin/python3"
chmod +x "${STAGE}/utils/bin/python3"
cp -a /usr/lib/python3.14/. "${STAGE}/python/lib/python3.14/"
rm -rf "${STAGE}/python/lib/python3.14"/{test,config-*,idlelib,tkinter,__pycache__}
mkdir -p "${STAGE}/python/lib/python3.14/site-packages"
for pkg in Crypto Cryptodome; do
	src="${HOME}/.local/lib/python3.14/site-packages/${pkg}"
	if [[ -d "${src}" ]]; then
		cp -a "${src}" "${STAGE}/python/lib/python3.14/site-packages/${pkg}"
	fi
done

# Bundled glibc loader for the dynamically linked helper binaries.
cp -L /lib64/ld-linux-x86-64.so.2 "${STAGE}/lib/ld-linux-x86-64.so.2"

# Collect every shared library needed by the bundled dynamic tools and
# Python extension modules.
collect_deps() {
	local file="$1"
	ldd "${file}" 2>/dev/null | awk '/=> \// {print $3}' || true
}
mapfile -t candidates < <(
	find "${STAGE}/utils/bin" "${STAGE}/python/lib/python3.14/lib-dynload" \
		"${STAGE}/python/lib/python3.14/site-packages" -type f \
		\( -perm -u+x -o -name '*.so' -o -name '*.so.*' \) 2>/dev/null
)
for file in "${candidates[@]}"; do
	while IFS= read -r lib; do
		[[ -n "${lib}" && -f "${lib}" ]] || continue
		cp -Lf "${lib}" "${STAGE}/lib/$(basename "${lib}")"
	done < <(collect_deps "${file}" | sort -u)
done

mkdir -p "${ROOT}/assets"
tar -C "${STAGE}" -czf "${OUT}" .
rm -rf "${STAGE}"
ls -lh "${OUT}"
