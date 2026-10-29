#!/bin/bash

# DumprX is now a Rust application.  This compatibility wrapper keeps the
# historical `./dumper.sh <firmware>` invocation working while guaranteeing
# that nothing is downloaded at runtime.

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null && pwd)"
BIN="${PROJECT_DIR}/dist/dumprx"

if ! [[ -x "${BIN}" ]]; then
	echo "[*] Building standalone dumprx (offline)..." >&2
	if ! ( cd "${PROJECT_DIR}" && ./build-standalone.sh ); then
		BIN="${PROJECT_DIR}/target/release/dumprx"
		if ! [[ -x "${BIN}" ]]; then
			echo "[ERROR] Standalone build failed and no release binary exists." >&2
			exit 1
		fi
	fi
fi

if [[ $# -eq 0 ]]; then
	exec "${BIN}" --help
fi

case "${1}" in
	-h|--help|-V|--version)
		exec "${BIN}" "$1"
		;;
	*)
		exec "${BIN}" dump "$@"
		;;
esac
