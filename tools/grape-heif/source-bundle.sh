#!/usr/bin/env bash
# Makes grape-heif.exe's complete corresponding source (DECISIONS D33;
# GPL-3.0 section 6, ENGINES.md licensing rule 3): one reproducible tarball
# that CI ships next to every build of the installer.
#
# Usage: tools/grape-heif/source-bundle.sh <vcpkg-root> <out-dir> [<BUILDINFO.txt>]
#
# <vcpkg-root> is the folder build.ps1 built in: vcpkg at the pinned commit,
# with the source archives in downloads/. Writes, in <out-dir>:
#   grape-heif-<version>-source.tar.gz          and its .sha256
# holding grape-heif-<version>-source/:
#   grape-heif/        tools/grape-heif, crates/grape-heif-proto,
#                      licenses/grape-heif and LICENSE at HEAD (git archive)
#   vendor/            every crate in tools/grape-heif/Cargo.lock (cargo vendor)
#   vcpkg-downloads/   the archives vcpkg.lock.json pins, found in
#                      <vcpkg-root>/downloads by SHA-512, and SHA512SUMS
#   vcpkg-ports/       vcpkg's ports for libheif (the overlay's base),
#                      libde265, x265, vcpkg-cmake and vcpkg-cmake-config and
#                      the x64-windows-static triplet at the pinned commit,
#                      and Grape's overlay in overlay/libheif
#   BUILD.md           the commit, the commands and the build's own record
#
# Needs bash, GNU tar (--sort), gzip, sha256sum, sha512sum, git and cargo.
# CI runs it in a `shell: bash` step: on Windows that's Git Bash, whose tar is
# GNU tar (a bare `bash` from PowerShell can be WSL's).
set -euo pipefail

me='grape-heif source bundle'
fail() {
    echo "$me: $*" >&2
    exit 1
}

[ $# -ge 2 ] && [ $# -le 3 ] || fail "usage: tools/grape-heif/source-bundle.sh <vcpkg-root> <out-dir> [<BUILDINFO.txt>]"
for tool in tar gzip sha256sum sha512sum git cargo; do
    command -v "$tool" > /dev/null || fail "$tool isn't on PATH."
done
tar --version | head -n 1 | grep -q 'GNU tar' || fail "needs GNU tar (for --sort=name)."

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
lock="$here/vcpkg.lock.json"
# POSIX paths from here on, so no tar or cp ever sees a drive letter's colon.
vcpkg_root=$(cd "$1" && pwd) || fail "$1 isn't a folder."
mkdir -p "$2"
out=$(cd "$2" && pwd)
buildinfo=${3:-}
[ -z "$buildinfo" ] || [ -f "$buildinfo" ] || fail "$buildinfo doesn't exist."

version=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$here/Cargo.toml" | head -n 1)
[ -n "$version" ] || fail "can't read the version in tools/grape-heif/Cargo.toml."
name="grape-heif-$version-source"
commit=$(git -C "$repo" rev-parse HEAD)
mtime=$(git -C "$repo" log -1 --format=%ct HEAD)

vcpkg_commit=$(sed -n 's/.*"commit": "\([0-9a-f]\{40\}\)".*/\1/p' "$lock" | head -n 1)
vcpkg_repo=$(sed -n 's/.*"repo": "\([^"]*\)".*/\1/p' "$lock" | head -n 1)
triplet=$(sed -n 's/.*"triplet": "\([^"]*\)".*/\1/p' "$lock" | head -n 1)
install=$(sed -n 's/.*"install": "\([^"]*\)".*/\1/p' "$lock" | head -n 1)
shas=$(sed -n 's/.*"sha512": "\([0-9a-f]\{128\}\)".*/\1/p' "$lock")
[ -n "$vcpkg_commit" ] && [ -n "$triplet" ] && [ -n "$shas" ] || fail "can't read tools/grape-heif/vcpkg.lock.json."
have=$(git -C "$vcpkg_root" rev-parse HEAD) || fail "$vcpkg_root isn't a vcpkg clone."
[ "$have" = "$vcpkg_commit" ] || fail "$vcpkg_root is at $have, not the pinned $vcpkg_commit."

stage=$(mktemp -d "$out/stage.XXXXXX")
trap 'rm -rf "$stage"' EXIT
root="$stage/$name"
mkdir -p "$root/grape-heif" "$root/vcpkg-downloads" "$root/vcpkg-ports/overlay"

echo "Grape's source at $commit..."
git -C "$repo" archive --format=tar HEAD tools/grape-heif crates/grape-heif-proto licenses/grape-heif LICENSE |
    tar -x -C "$root/grape-heif"

echo "The Rust crates..."
(cd "$here" && cargo vendor --locked --versioned-dirs "$root/vendor" > /dev/null)

echo "The source archives..."
(cd "$vcpkg_root/downloads" && find . -maxdepth 1 -type f -print0 | xargs -0 sha512sum) > "$stage/downloads.sha512"
for sha in $shas; do
    file=$(awk -v s="$sha" '$1 == s { sub(/^[^ ]+ +\*?(\.\/)?/, ""); print; exit }' "$stage/downloads.sha512")
    [ -n "$file" ] || fail "no file in $vcpkg_root/downloads has the SHA-512 $sha (vcpkg.lock.json)."
    cp "$vcpkg_root/downloads/$file" "$root/vcpkg-downloads/$file"
    printf '%s  %s\n' "$sha" "$file" >> "$root/vcpkg-downloads/SHA512SUMS"
    echo "  $file"
done
(cd "$root/vcpkg-downloads" && sha512sum -c --quiet SHA512SUMS) || fail "a copied archive doesn't match its SHA-512."

echo "vcpkg's ports at $vcpkg_commit, and the overlay..."
git -C "$vcpkg_root" archive --format=tar HEAD ports/libheif ports/libde265 ports/x265 ports/vcpkg-cmake ports/vcpkg-cmake-config "triplets/$triplet.cmake" |
    tar -x -C "$root/vcpkg-ports"
cp -R "$here/vcpkg/ports/libheif" "$root/vcpkg-ports/overlay/libheif"

exe_sha='(see BUILDINFO below)'
if [ -n "$buildinfo" ]; then
    exe_sha=$(sed -n 's/^SHA-256: \([0-9a-f]\{64\}\)$/\1/p' "$buildinfo" | head -n 1)
fi
{
    echo "# $name"
    echo
    echo "The complete corresponding source of grape-heif.exe $version, Grape's HEIC helper for Windows (DECISIONS D33). The program is GPL-3.0-or-later as a whole: grape-heif/licenses/grape-heif/ has the texts and SOURCE.md."
    echo
    echo "- Grape commit: \`$commit\`"
    echo "- vcpkg: $vcpkg_repo at \`$vcpkg_commit\`, triplet \`$triplet\`, \`$install\`, with the overlay in vcpkg-ports/overlay"
    echo "- grape-heif.exe SHA-256: \`$exe_sha\`"
    echo
    echo "## Build it again"
    echo
    echo "On Windows, with Visual Studio 2022's C++ tools, git and Rust (the version below):"
    echo
    echo '1. Clone vcpkg at the commit above into an empty folder (`git init`, `git fetch --depth 1 <repo> <commit>`, `git checkout --detach FETCH_HEAD`) and run `bootstrap-vcpkg.bat -disableMetrics`.'
    echo '2. Copy vcpkg-downloads/* into its downloads\ folder, so nothing is fetched (vcpkg checks each SHA-512).'
    echo "3. From that folder: \`vcpkg install \"$install:$triplet\" --overlay-ports=<this folder>\\vcpkg-ports\\overlay\`."
    echo '4. In grape-heif\tools\grape-heif, put the crates in place for an offline build: add to .cargo\config.toml'
    echo
    echo '   ```toml'
    echo '   [source.crates-io]'
    echo '   replace-with = "vendored-sources"'
    echo '   [source.vendored-sources]'
    echo '   directory = "../../../vendor"'
    echo '   ```'
    echo
    echo "5. There, with VCPKG_ROOT set to the vcpkg folder and VCPKGRS_TRIPLET=$triplet: \`cargo build --release --locked --bins\`."
    echo
    echo 'tools\grape-heif\build.ps1 does steps 1, 3 and 5 from the network, with every check.'
    if [ -n "$buildinfo" ]; then
        echo
        echo "## The build's own record (BUILDINFO.txt)"
        echo
        echo '```'
        tr -d '\r' < "$buildinfo"
        echo '```'
    fi
} > "$root/BUILD.md"

echo "Packing..."
tarball="$name.tar.gz"
(cd "$stage" && LC_ALL=C tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$mtime" \
    --mode=u+rwX,go+rX,go-w --format=gnu -cf - "$name") | gzip -9n > "$out/$tarball"
# "<hash>  <name>" exactly, whatever mode this sha256sum prints in.
sum=$(sha256sum < "$out/$tarball" | cut -d ' ' -f 1)
printf '%s  %s\n' "$sum" "$tarball" > "$out/$tarball.sha256"
echo "$out/$tarball: $(wc -c < "$out/$tarball") bytes, SHA-256 $(cut -d ' ' -f 1 "$out/$tarball.sha256")"
