#!/usr/bin/env bash

SCRIPT_DIR="$(readlink -f "$(dirname "${BASH_SOURCE[0]}")")"

ver="$1"
sed -i "s/^pkgver=.*/pkgver=${ver}/" ${SCRIPT_DIR}/PKGBUILD
