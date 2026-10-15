#!/usr/bin/env bash
# Regenerate the virtual pad bus's declarations from its public header.
#
# Run by hand on Windows, never from a build script: the generated file is
# committed so that no build machine needs a C toolchain, the SDK or the bus's
# sources. Run from a shell that sees LLVM's clang and the SDK, with the bus's
# client library checked out, whose `include/ViGEm/km/BusShared.h` is the
# header the bus itself is built from:
#
#   cargo install bindgen-cli --version 0.72.1 --locked
#   VIGEM_CLIENT=/path/to/ViGEmClient scripts/gen-vigem-bindings.sh
#
# **The header is not vendored**, so the checkout is the source; its commit is
# written into the generated file's first lines, and a regeneration against
# another one shows in the diff. Every struct carries a compile-time layout
# assertion derived from the header, so a transcription error is a build
# failure. The control codes are the header's own macros, evaluated by clang.

set -euo pipefail

# Git Bash rewrites arguments that look like paths, a leading "//" included,
# which would mangle the generated file's comment line.
export MSYS2_ARG_CONV_EXCL='//'
py=""
for candidate in python3 python; do
    if "$candidate" -c '' 2>/dev/null; then py="$candidate"; break; fi
done
[ -n "$py" ] || { echo "no python" >&2; exit 1; }

root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/crates/inject/src/vigem.rs"
client="${VIGEM_CLIENT:?set VIGEM_CLIENT to a checkout of the bus client library}"
header="$client/include/ViGEm/km/BusShared.h"
[ -f "$header" ] || { echo "no header at $header" >&2; exit 1; }
commit="$(git -c safe.directory='*' -C "$client" rev-parse HEAD)"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cat > "$work/vigem.h" <<'EOF'
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <winioctl.h>
#include <ViGEm/km/BusShared.h>
EOF

echo "generating bus declarations against $commit"
bindgen "$work/vigem.h" \
    --no-doc-comments \
    --no-prepend-enum-name \
    --clang-macro-fallback \
    --raw-line "// The bus client library at $commit." \
    `# The requests that plug a pad in, wait for it, report it, hear what is` \
    `# asked of it and take it out again; and the check that the bus speaks` \
    `# this version of them.` \
    --allowlist-type 'VIGEM_(PLUGIN_TARGET|UNPLUG_TARGET|CHECK_VERSION|WAIT_DEVICE_READY|TARGET_TYPE)' \
    --allowlist-type 'XUSB_(REPORT|SUBMIT_REPORT|REQUEST_NOTIFICATION)' \
    --allowlist-var 'IOCTL_VIGEM_(PLUGIN_TARGET|UNPLUG_TARGET|CHECK_VERSION|WAIT_DEVICE_READY)' \
    --allowlist-var 'IOCTL_XUSB_(SUBMIT_REPORT|REQUEST_NOTIFICATION)' \
    --allowlist-var 'VIGEM_COMMON_VERSION|Xbox360Wired' \
    -o "$out" \
    -- --target=x86_64-pc-windows-msvc -I "$client/include"

# The bus's interface identifier, transcribed mechanically from the header so
# that no human copies sixteen bytes of hex. The header declares it as an
# external object that only an import library defines.
"$py" - "$header" "$out" <<'PY'
import re
import sys

header, out = sys.argv[1], sys.argv[2]
text = open(header, encoding="utf-8", errors="replace").read()
m = re.search(r"DEFINE_GUID\(GUID_DEVINTERFACE_BUSENUM_VIGEM,\s*([^)]*)\)", text)
if not m:
    raise SystemExit("no interface identifier in the header")
parts = [int(p.strip(), 16) for p in m.group(1).split(",")]
if len(parts) != 11:
    raise SystemExit("the identifier has %d parts, expected 11" % len(parts))
value = "%08x_%04x_%04x_%s_%s" % (
    parts[0], parts[1], parts[2],
    "".join("%02x" % p for p in parts[3:5]),
    "".join("%02x" % p for p in parts[5:]),
)
with open(out, "a", encoding="ascii", newline="\n") as f:
    f.write("pub const GUID_DEVINTERFACE_BUSENUM_VIGEM: windows_sys::core::GUID =\n")
    f.write("    windows_sys::core::GUID::from_u128(0x%s);\n" % value)
print("  interface identifier %s" % value)
PY

rustfmt --edition 2024 "$out"
