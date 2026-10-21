#!/usr/bin/env bash
# Hermetic: the Python driver replaces every external command with one strict stub.
set -euo pipefail
exec python3 -I "$(dirname -- "${BASH_SOURCE[0]}")/role_wake_test.py"
