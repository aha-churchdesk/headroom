#!/bin/bash
# Install a complete, already-built bundle without rebuilding it.
set -euo pipefail
source "$(dirname "$0")/install-common.sh"
[ "$#" -eq 1 ] || { echo "usage: $0 PATH/Headroom.app" >&2; exit 2; }
hr_install_bundle "$1"
