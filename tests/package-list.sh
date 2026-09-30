#!/bin/sh
# Dry-run archive gate. Reads the package file list and the bytes of
# every listed file. It never uploads and never writes outside this tree.

set -u

fail()
{
    printf 'package-list: %s\n' "$1" >&2
    exit 1
}

cd "$(dirname "$0")/.." || fail 'cannot enter the package root'

command -v cargo >/dev/null 2>&1 || fail 'cargo not found'

list=$(cargo package --list --allow-dirty) || fail 'cargo package --list failed'

# Required names.
printf '%s\n' "$list" | grep -qx 'README.md' || fail 'README.md absent from the list'
printf '%s\n' "$list" | grep -qx 'src/main.rs' || fail 'src/main.rs absent from the list'

# Forbidden path prefixes.
bad=$(printf '%s\n' "$list" | grep -E '^(\.grok/|\.specify/|specs/)')
[ -n "$bad" ] && fail "forbidden path in the list: $bad"

# Forbidden file names.
bad=$(printf '%s\n' "$list" | grep -E '(^|/)(credentials(\.toml)?|\.env)$')
[ -n "$bad" ] && fail "forbidden file in the list: $bad"

# No registry token bytes in any listed file except this script and
# cargo-generated archive stubs, which have no on-disk source here.
self=tests/package-list.sh
for f in $list; do
    [ "$f" = "$self" ] && continue
    case "$f" in
        .cargo_vcs_info.json|Cargo.toml.orig) continue ;;
    esac
    [ -f "$f" ] || fail "listed file missing on disk: $f"
    if grep -qaE 'CARGO_REGISTRY_TOKEN|cio[A-Za-z0-9_-]{32,}' -- "$f"; then
        fail "registry token bytes in $f"
    fi
done

# No line that both removes a file and names memo.
if grep -E '(^|[^[:alpha:]])(rm|unlink)([^[:alpha:]]|$)|remove|delet' README.md | grep -q memo; then
    fail 'README.md has a line that removes a file and names memo'
fi

exit 0
