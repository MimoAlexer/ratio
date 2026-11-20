#!/usr/bin/env bash
# Build inside the same Debian/Ubuntu environment that linked the release.
set -euo pipefail
export LC_ALL=C
if [[ $# -ne 2 ]]; then
    echo 'Usage: build.sh /path/to/ratio output-directory' >&2
    exit 2
fi
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
desktop=$(cd -- "$here/../.." && pwd)
binary=$(realpath -- "$1")
mkdir -p -- "$2"
output=$(realpath -- "$2")
version=$(cd "$desktop" && cargo metadata --no-deps --format-version 1 | python3 -c '
import json, sys
print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "ratio"))
')
dpkg --validate-version "$version"
if ! readelf -h "$binary" | grep -q 'Machine:.*Advanced Micro Devices X86-64'; then
    echo 'Expected an x86-64 Linux release binary.' >&2
    exit 1
fi
if [[ "$("$binary" --version)" != "Ratio $version" ]]; then
    echo "Expected the Ratio $version release executable." >&2
    exit 1
fi
stage=$(mktemp -d "$output/.ratio-package.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT
root="$stage/root"
install -Dm755 -- "$binary" "$root/usr/bin/ratio"
install -Dm644 -- "$here/ratio.desktop" "$root/usr/share/applications/ratio.desktop"
install -Dm644 -- "$desktop/assets/icon-256.png" "$root/usr/share/icons/hicolor/256x256/apps/ratio.png"
desktop-file-validate "$root/usr/share/applications/ratio.desktop"

# Read the dependency floors from this binary's imported symbols and the build
# distro's dpkg database. This catches new direct runtime dependencies as well.
mkdir -p "$stage/debian" "$root/DEBIAN"
cat > "$stage/debian/control" <<'CONTROL'
Source: ratio
Section: devel
Priority: optional
Maintainer: Ratio contributors <noreply@localhost>

Package: ratio
Architecture: amd64
Description: Desktop project and issue tracker for solo developers
CONTROL
deps=$(cd "$stage" && dpkg-shlibdeps -O -eroot/usr/bin/ratio)
deps=${deps#shlibs:Depends=}
if [[ "$deps" != *libwebkit2gtk-4.1-0* || "$deps" != *libgtk-3-0* ]]; then
    echo "Unexpected WebKitGTK or GTK dependencies: $deps" >&2
    exit 1
fi
size=$(du -sk "$root/usr" | cut -f1)
cat > "$root/DEBIAN/control" <<CONTROL
Package: ratio
Version: $version
Architecture: amd64
Maintainer: Ratio contributors <noreply@localhost>
Section: devel
Priority: optional
Installed-Size: $size
Depends: $deps, bubblewrap, ca-certificates, xdg-utils
Provides: working-set (= $version)
Conflicts: working-set (<< 0.1.1)
Replaces: working-set (<< 0.1.1)
Description: Desktop project and issue tracker for solo developers
 Open Ratio in a native desktop window to manage projects, issues,
 notes, and releases.
CONTROL
chmod 0755 "$root" "$root/DEBIAN"
# Normalized timestamps, archive compression and ownership make repeat packaging
# of the same executable with the same dpkg version deterministic.
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-946684800}
find "$root" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +
archive="ratio_${version}_amd64.deb"
dpkg-deb -Zxz --root-owner-group --build "$root" "$stage/$archive"
mv -- "$stage/$archive" "$output/$archive"
(cd "$output" && sha256sum "$archive")
