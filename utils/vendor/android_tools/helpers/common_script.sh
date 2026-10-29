#!/bin/bash

# SPDX-License-Identifier: GPL-3.0-or-later
#
# Minimal, fully offline version of android_tools' common_script.sh.
# The original file cloned Firmware_extractor, android_boot_image_editor and
# extract-dtb on every run.  DumprX vendors everything it needs, so this
# helper only sets up the working directories.

PROJECT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )/.." >/dev/null && pwd )"

mkdir -p "$PROJECT_DIR/dumps/" "$PROJECT_DIR/working" "$PROJECT_DIR/input"

if [ "$1" == "y" ]; then
    rm -rf "$PROJECT_DIR"/working/*
fi

[[ -z "$TMPDIR" ]] && TMPDIR="/tmp"
export LC_ALL=C
