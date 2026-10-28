#!/usr/bin/env bash
# Builds Grape's own bsdtar (DECISIONS D27) on macOS, and on Linux for local
# testing only: libarchive's release tarball plus the patches in
# scripts/bsdtar/patches, as pinned in scripts/bsdtar/sources.json. Windows
# has scripts/bsdtar/build.ps1.
#
#   ./scripts/bsdtar/build.sh                   build and check into target/bsdtar
#   ./scripts/bsdtar/build.sh --root <dir>      use another folder
#   ./scripts/bsdtar/build.sh --check-only      only check what <root>/out holds
#   ./scripts/bsdtar/build.sh --install         also copy the results into
#                                               apps/desktop/src-tauri/binaries/
#
# The root folder holds dl/ (the checked tarball, kept), work/ (the extract
# and the build trees, deleted at the end of every run) and out/ (the
# results). A full build:
#   1. reads and checks sources.json (with python3);
#   2. keeps dl/libarchive-<version>.tar.gz if its size and SHA-256 match,
#      else downloads it into a new temp folder with curl (HTTPS only, also
#      on redirects) and checks both before anything is extracted;
#   3. extracts it into work/src;
#   4. checks each patch's SHA-256, then applies it with git apply, in order;
#   5. configures with every optional library off and builds the bsdtar
#      target (macOS: arm64 and x86_64, minimum macOS 13.0);
#   6. puts the results in out/ under Tauri's externalBin names
#      (bsdtar-aarch64-apple-darwin, bsdtar-x86_64-apple-darwin and
#      bsdtar-universal-apple-darwin, all three signed ad hoc; on Linux
#      bsdtar-<arch>-unknown-linux-gnu), with BUILDINFO.txt and SHA256SUMS;
#   7. checks them (the whole of --check-only): the exact --version line from
#      sources.json and only the allowed tokens after "libarchive <version>";
#      on macOS only libSystem linked, both architectures, minos 13.0 and
#      valid signatures; on Linux only libc; and SHA256SUMS. A full build also
#      regenerates the copyright lines of the compiled sources and compares
#      them with licenses/libarchive/copyright-lines.txt;
#   8. with --install, copies the binaries into apps/desktop/src-tauri/binaries/
#      for `tauri dev`. It never touches sidecars.stamp.
#
# Needs python3, git, tar, curl (only to download) and shasum or sha256sum.
# macOS: the Xcode Command Line Tools (clang, lipo, codesign, vtool, otool)
# and CMake 3.23 or later. Linux: cc, cmake, make and ldd. It never changes
# PATH and installs nothing. Works with macOS's bash 3.2.

set -euo pipefail

me="bsdtar build"

fail() {
  echo "$me: $*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: ./scripts/bsdtar/build.sh [--root <dir>] [--check-only] [--install]

Builds Grape's bsdtar from the libarchive tarball and patches pinned in
scripts/bsdtar/sources.json, and checks it. The root (default target/bsdtar)
gets dl/, work/ (deleted at the end) and out/.
  --check-only   check the binaries already in <root>/out, build nothing
  --install      also copy them into apps/desktop/src-tauri/binaries/
EOF
}

root=""
check_only=0
install=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --root)
      [ "$#" -ge 2 ] || fail "--root needs a folder."
      root=$2
      shift
      ;;
    --root=*) root=${1#--root=} ;;
    --check-only) check_only=1 ;;
    --install) install=1 ;;
    --help | -h)
      usage
      exit 0
      ;;
    *) fail "unknown option '$1'. Try --help." ;;
  esac
  shift
done

case "$(uname -s)" in
  Darwin) os=macos ;;
  Linux) os=linux ;;
  *) fail "this script is for macOS (and Linux, for testing). On Windows, run .\\scripts\\bsdtar\\build.ps1." ;;
esac

need() {
  command -v "$1" >/dev/null 2>&1 || fail "$1 is missing. $2"
}

if [ "$os" = macos ]; then
  hint="Install the Xcode Command Line Tools: xcode-select --install"
  for tool in python3 git lipo codesign vtool otool arch; do need "$tool" "$hint"; done
  tar_bin=/usr/bin/tar
  [ -x "$tar_bin" ] || fail "$tar_bin is missing."
else
  hint="Install it with your package manager."
  for tool in python3 git tar ldd; do need "$tool" "$hint"; done
  tar_bin=tar
fi
# Without the Command Line Tools, /usr/bin/python3 is a stub that fails.
python3 -I -c 'import json' >/dev/null 2>&1 || fail "python3 doesn't run. $hint"
if command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 <"$1" | awk '{ print $1 }'; }
elif command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum <"$1" | awk '{ print $1 }'; }
else
  fail "shasum or sha256sum is missing. $hint"
fi

script_dir=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$script_dir/../.." && pwd)
sources="$script_dir/sources.json"
patch_dir="$script_dir/patches"
notice_lines="$repo/licenses/libarchive/copyright-lines.txt"
bin_dir="$repo/apps/desktop/src-tauri/binaries"
[ -f "$sources" ] || fail "can't find scripts/bsdtar/sources.json."

# Reads and checks sources.json, and prints what this script needs from it as
# tab-separated lines:
#   L <version> <url> <sha256> <size>
#   M <marker>
#   P <patch file> <sha256>          (in order)
#   V <the exact --version line for this OS, trailing space included>
#   T <macOS deployment target>
# The same rules as in build.ps1. (A function, because bash 3.2 misreads a
# here-document inside $(...).)
read_sources() {
  python3 -I - "$sources" "$os" <<'PY'
import json
import re
import sys


def die(msg):
    sys.stderr.write("bsdtar build: scripts/bsdtar/sources.json: " + msg + "\n")
    sys.exit(1)


def no_duplicates(pairs):
    seen = {}
    for key, value in pairs:
        if key in seen:
            die("the key '%s' appears twice in one object." % key)
        seen[key] = value
    return seen


def check_keys(obj, where, required):
    if not isinstance(obj, dict):
        die("%s must be an object." % where)
    for key in obj:
        if not key.startswith("_") and key not in required:
            die("%s has an unknown key '%s'." % (where, key))
    for key in required:
        if key not in obj:
            die("%s is missing '%s'." % (where, key))


def string(value, where, pattern, hint):
    if not isinstance(value, str) or not re.match(pattern, value):
        die("%s %s." % (where, hint))
    return value


SHA256 = r"^[0-9a-f]{64}$"
try:
    with open(sys.argv[1], encoding="utf-8") as f:
        s = json.load(f, object_pairs_hook=no_duplicates)
except (OSError, ValueError) as e:
    die("can't read it as JSON (%s)." % e)
os_name = sys.argv[2]

check_keys(s, "the file", ("format", "libarchive", "marker", "patches", "version_line",
                           "macos_deployment_target", "windows_dlls"))
if type(s["format"]) is not int or s["format"] != 1:
    die("'format' must be 1. This script reads format 1 only.")

la = s["libarchive"]
check_keys(la, "libarchive", ("version", "released", "url", "sha256", "size"))
version = string(la["version"], "libarchive.version", r"^[0-9]+(\.[0-9]+)*$", "must look like 3.8.10")
string(la["released"], "libarchive.released", r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$", "must be a date like 2026-10-14")
url = string(la["url"], "libarchive.url", r"^https://[^\s]+$", "must be an https:// URL")
if not url.endswith("/libarchive-%s.tar.gz" % version):
    die("libarchive.url must end in /libarchive-%s.tar.gz." % version)
sha = string(la["sha256"], "libarchive.sha256", SHA256, "must be 64 lower-case hex digits")
if type(la["size"]) is not int or la["size"] <= 0:
    die("libarchive.size must be the tarball's size in bytes.")

marker = string(s["marker"], "marker", r"^grape\.[0-9]+$", "must look like grape.1")

patches = s["patches"]
if not isinstance(patches, list) or not patches:
    die("patches must be a non-empty list.")
names = []
out_p = []
for i, p in enumerate(patches):
    where = "patches[%d]" % i
    check_keys(p, where, ("file", "sha256", "from"))
    name = string(p["file"], where + ".file", r"^[A-Za-z0-9][A-Za-z0-9._-]*\.patch$",
                  "must be a plain .patch file name (no /, \\ or ..)")
    if ".." in name:
        die("%s.file must not contain '..'." % where)
    if name in names:
        die("%s.file names '%s' twice." % (where, name))
    names.append(name)
    psha = string(p["sha256"], where + ".sha256", SHA256, "must be 64 lower-case hex digits")
    string(p["from"], where + ".from", r"^\S", "must say where the patch comes from")
    out_p.append("P\t%s\t%s" % (name, psha))

lines = s["version_line"]
check_keys(lines, "version_line", ("windows", "macos", "linux"))
prefix = "bsdtar %s-%s - libarchive %s " % (version, marker, version)
for key in ("windows", "macos", "linux"):
    line = lines[key]
    if not isinstance(line, str) or not line.startswith(prefix) or "\t" in line or "\n" in line:
        die("version_line.%s must start with '%s' (trailing space included)." % (key, prefix))

target = string(s["macos_deployment_target"], "macos_deployment_target", r"^[0-9]+\.[0-9]+$",
                "must look like 13.0")
dlls = s["windows_dlls"]
if not isinstance(dlls, list) or not dlls:
    die("windows_dlls must be a non-empty list.")
for i, d in enumerate(dlls):
    string(d, "windows_dlls[%d]" % i, r"^[A-Za-z0-9_.-]+\.[Dd][Ll][Ll]$", "must be a DLL's file name")

print("L\t%s\t%s\t%s\t%d" % (version, url, sha, la["size"]))
print("M\t%s" % marker)
print("\n".join(out_p))
print("V\t%s" % lines[os_name])
print("T\t%s" % target)
PY
}

# The copyright lines of the sources compiled into bsdtar, as
# licenses/libarchive/copyright-lines.txt has them: a heading, a blank line,
# then every distinct line, sorted by code point. The files are those
# libarchive/CMakeLists.txt puts in include_HEADERS and libarchive_SOURCES
# (SET plus every LIST(APPEND), without archive_disk_acl_*: ACLs are off) and
# tar/CMakeLists.txt puts in bsdtar_SOURCES, .c and .h only. A line counts
# when, after any leading comment marker and spaces, it starts with
# "Copyright" or "Portions Copyright", then a space and "(c)", "(C)", "©" or
# a digit; its trailing spaces and "*/" are dropped. Every file must be
# UTF-8. build.ps1's Get-CopyrightLines does exactly the same.
copyright_lines() { # source tree; prints the file's bytes
  python3 -I - "$1" <<'PY'
import os
import re
import sys

src = sys.argv[1]
HEADING = ("Copyright lines of the libarchive and bsdtar source files compiled into Grape's "
           "bsdtar (scripts/bsdtar/build.sh and build.ps1 regenerate this list from the pinned "
           "source and fail if it differs). Most are holders of the BSD-2-Clause notice in "
           "LICENSE.txt; the parts under other terms have their full notices in the other files here.")


def die(msg):
    sys.stderr.write("bsdtar build: copyright lines: " + msg + "\n")
    sys.exit(1)


def listed(cmake, var, base):
    try:
        with open(os.path.join(src, cmake), "rb") as f:
            text = f.read().decode("utf-8")
    except (OSError, UnicodeDecodeError) as e:
        die("can't read %s (%s)." % (cmake, e))
    m = re.search(r"SET\(%s[ \t\r\n]([^)]*)\)" % var, text, re.I)
    if m is None:
        die("%s has no SET(%s ...)." % (cmake, var))
    words = []
    for line in re.split(r"\r\n|\n|\r", m.group(1)):
        words += line.split("#", 1)[0].split()
    words += re.findall(r"LIST\(APPEND[ \t]+%s[ \t]+([^ \t\r\n)]+)[ \t]*\)" % var, text, re.I)
    files = []
    for w in words:
        parts = []
        for part in (base + "/" + w).split("/"):
            if part == "..":
                if not parts:
                    die("%s names a file outside the source tree: %s" % (cmake, w))
                parts.pop()
            elif part not in ("", "."):
                parts.append(part)
        files.append("/".join(parts))
    return files


files = listed("libarchive/CMakeLists.txt", "include_HEADERS", "libarchive")
files += listed("libarchive/CMakeLists.txt", "libarchive_SOURCES", "libarchive")
files += listed("tar/CMakeLists.txt", "bsdtar_SOURCES", "tar")
files = sorted(set(f for f in files if (f.endswith(".c") or f.endswith(".h"))
                   and not f.rsplit("/", 1)[-1].startswith("archive_disk_acl_")))
if len(files) < 100:
    die("only %d source files listed; the CMake files must have changed shape." % len(files))

LEAD = re.compile(r"^[ \t]*(?:/\*+|\*+|//+|#+)?[ \t]*")
TRAIL = re.compile(r"[ \t]*(?:\*/)?[ \t]*$")
COPYRIGHT = re.compile("^(?:Portions )?Copyright (?:\\([cC]\\)|©|[0-9])")
found = set()
for f in files:
    try:
        with open(os.path.join(src, f), "rb") as fh:
            text = fh.read().decode("utf-8")
    except (OSError, UnicodeDecodeError) as e:
        die("can't read %s as UTF-8 (%s)." % (f, e))
    for line in re.split(r"\r\n|\n|\r", text):
        line = TRAIL.sub("", LEAD.sub("", line, count=1), count=1)
        if COPYRIGHT.match(line):
            found.add(line)
if not found:
    die("no copyright lines found.")
out = HEADING + "\n\n" + "\n".join(sorted(found)) + "\n"
sys.stdout.buffer.write(out.encode("utf-8"))
PY
}

plan=$(read_sources) || fail "sources.json has a problem (above)."
field() { # kind, field number; the first matching line's field, verbatim
  printf '%s\n' "$plan" | awk -F '\t' -v k="$1" -v n="$2" '$1 == k { print $n; exit }'
}
version=$(field L 2)
url=$(field L 3)
want_sha=$(field L 4)
want_size=$(field L 5)
marker=$(field M 2)
version_line=$(field V 2)
deploy=$(field T 2)

if [ -z "$root" ]; then
  root="$repo/target/bsdtar"
fi
mkdir -p "$root" || fail "couldn't make $root."
root=$(cd "$root" && pwd)
[ "$root" != "/" ] || fail "the root can't be /."
dl="$root/dl"
work="$root/work"
out="$root/out"
tarball="$dl/libarchive-$version.tar.gz"

if [ "$os" = macos ]; then
  arm=bsdtar-aarch64-apple-darwin
  intel=bsdtar-x86_64-apple-darwin
  main=bsdtar-universal-apple-darwin
  binaries="$arm $intel $main"
  allowed_tokens="CommonCrypto/system libb2/bundled"
else
  main="bsdtar-$(uname -m)-unknown-linux-gnu"
  binaries="$main"
  allowed_tokens="libb2/bundled"
fi

dl_tmp=""
cleanup() {
  rm -rf "$work"
  if [ -n "$dl_tmp" ]; then rm -rf "$dl_tmp"; fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# git apply must use the scratch repository in the extract (apply_patches),
# never one the caller's environment names.
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE

file_size() {
  wc -c <"$1" | tr -d ' '
}

fetch_tarball() {
  if [ -f "$tarball" ] && [ "$(file_size "$tarball")" = "$want_size" ] && [ "$(sha256 "$tarball")" = "$want_sha" ]; then
    echo "Using dl/$(basename "$tarball") (its size and SHA-256 match sources.json)."
    return 0
  fi
  need curl "$hint"
  mkdir -p "$dl"
  rm -f "$tarball"
  dl_tmp=$(mktemp -d "$root/dl-tmp.XXXXXX") || fail "couldn't make a temp folder."
  local file
  file="$dl_tmp/$(basename "$tarball")"
  echo "Downloading libarchive $version ($want_size bytes) from $url"
  if ! curl --fail --location --proto '=https' --proto-redir '=https' --tlsv1.2 \
    --silent --show-error --retry 3 --retry-all-errors --connect-timeout 30 \
    --speed-limit 1024 --speed-time 60 \
    --output "$file" "$url"; then
    fail "couldn't download libarchive from $url."
  fi
  local size got
  size=$(file_size "$file")
  [ "$size" = "$want_size" ] || fail "the download is $size bytes, but sources.json says $want_size. Deleted it; nothing was extracted."
  got=$(sha256 "$file")
  [ "$got" = "$want_sha" ] || fail "the download has SHA-256 $got, but sources.json says $want_sha. Deleted it; nothing was extracted."
  echo "  Its size and SHA-256 match sources.json."
  mv -f "$file" "$tarball"
  rm -rf "$dl_tmp"
  dl_tmp=""
}

hash_or_none() { # file
  if [ -f "$1" ]; then sha256 "$1"; else echo none; fi
}

apply_patches() { # source tree
  local src=$1 kind file want got patch touched path before after
  # Inside the Grape checkout (target/ is in it), git apply reads a patch's
  # paths from the checkout's top, skips every file and still exits 0. A
  # throwaway repository in the extract is the nearest one, so the paths are
  # read from there. The check after each patch catches a skip anyway.
  (cd "$src" && git -c init.defaultBranch=main init -q) || fail "git couldn't make a scratch repository in the extract."
  while IFS=$'\t' read -r kind file want; do
    [ "$kind" = P ] || continue
    patch="$patch_dir/$file"
    [ -f "$patch" ] || fail "scripts/bsdtar/patches/$file is missing."
    got=$(sha256 "$patch")
    [ "$got" = "$want" ] || fail "patches/$file has SHA-256 $got, but sources.json says $want."
    touched=$(cd "$src" && git apply --numstat "$patch" | awk -F '\t' '{ print $3 }') ||
      fail "git can't read patches/$file."
    [ -n "$touched" ] || fail "patches/$file changes no file."
    before=""
    for path in $touched; do before="$before $(hash_or_none "$src/$path")"; done
    (cd "$src" && git -c core.autocrlf=false apply --check "$patch") ||
      fail "patches/$file doesn't apply to libarchive $version."
    (cd "$src" && git -c core.autocrlf=false apply "$patch") ||
      fail "patches/$file didn't apply."
    # Every file it names must have changed: git skips files silently.
    for path in $touched; do
      after=$(hash_or_none "$src/$path")
      case "$before " in
        *" $after "*) fail "patches/$file didn't change $path (git skipped it)." ;;
      esac
    done
    echo "Applied $file."
  done <<<"$plan"
}

# Every optional library off, so nothing on the runner (Homebrew's xz, zstd,
# lz4 or OpenSSL) gets in silently.
common_cmake_args() {
  printf '%s\n' \
    -DCMAKE_BUILD_TYPE=Release \
    -DBUILD_SHARED_LIBS=OFF -DENABLE_TAR=ON -DENABLE_TAR_SHARED=OFF \
    -DENABLE_CPIO=OFF -DENABLE_CAT=OFF -DENABLE_UNZIP=OFF \
    -DENABLE_TEST=OFF -DENABLE_INSTALL=OFF -DENABLE_COVERAGE=OFF -DENABLE_WERROR=OFF \
    -DENABLE_OPENSSL=OFF -DENABLE_MBEDTLS=OFF -DENABLE_NETTLE=OFF -DENABLE_MD=OFF \
    -DENABLE_ZLIB=OFF -DENABLE_BZip2=OFF -DENABLE_LZMA=OFF -DENABLE_ZSTD=OFF \
    -DENABLE_LZ4=OFF -DENABLE_LZO=OFF -DENABLE_LIBB2=OFF \
    -DENABLE_LIBXML2=OFF -DENABLE_BSDXML=OFF -DENABLE_EXPAT=OFF \
    -DENABLE_PCREPOSIX=OFF -DENABLE_PCRE2POSIX=OFF -DENABLE_LIBGCC=OFF \
    -DENABLE_ICONV=OFF -DENABLE_ACL=OFF -DENABLE_XATTR=OFF
}

cmake_version_ok() { # major, minor: true when cmake is at least that
  local v major minor
  v=$(cmake --version | sed -n 1p | awk '{ print $3 }')
  major=${v%%.*}
  minor=${v#*.}
  minor=${minor%%.*}
  case "$major$minor" in *[!0-9]*) return 1 ;; esac
  [ "$major" -gt "$1" ] || { [ "$major" -eq "$1" ] && [ "$minor" -ge "$2" ]; }
}

build() { # source tree
  local src=$1 jobs arch
  if [ "$os" = macos ]; then
    need clang "$hint"
    need cmake "Install CMake 3.23 or later (https://cmake.org or Homebrew)."
    cmake_version_ok 3 23 || fail "CMake 3.23 or later is needed; this is $(cmake --version | sed -n 1p)."
    jobs=$(sysctl -n hw.ncpu)
    # Cross-building x86_64 on arm64 is safe: libarchive's only run-time
    # configure check is Windows-only.
    for arch in arm64 x86_64; do
      echo "Building bsdtar for $arch..."
      # shellcheck disable=SC2046 # one switch per line, none with spaces
      cmake -S "$src" -B "$work/build-$arch" \
        -DCMAKE_OSX_ARCHITECTURES="$arch" -DCMAKE_OSX_DEPLOYMENT_TARGET="$deploy" \
        -DCMAKE_FIND_FRAMEWORK=NEVER -DCMAKE_FIND_APPBUNDLE=NEVER \
        -DCMAKE_IGNORE_PREFIX_PATH="/opt/homebrew;/usr/local" \
        $(common_cmake_args) -DPOSIX_REGEX_LIB=LIBC ||
        fail "CMake couldn't configure the $arch build."
      cmake --build "$work/build-$arch" --target bsdtar -j "$jobs" || fail "the $arch build failed."
      [ -f "$work/build-$arch/bin/bsdtar" ] || fail "the $arch build has no bin/bsdtar."
    done
  else
    need cc "$hint"
    need cmake "$hint"
    need make "$hint"
    jobs=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)
    echo "Building bsdtar for Linux (dev testing only)..."
    # POSIX_REGEX_LIB=NONE as on Windows. glibc's regex.h still defines
    # HAVE_REGEX_H, so -s stays compiled in here.
    # shellcheck disable=SC2046 # one switch per line, none with spaces
    cmake -S "$src" -B "$work/build" $(common_cmake_args) -DPOSIX_REGEX_LIB=NONE ||
      fail "CMake couldn't configure the build."
    cmake --build "$work/build" --target bsdtar -j "$jobs" || fail "the build failed."
    [ -f "$work/build/bin/bsdtar" ] || fail "the build has no bin/bsdtar."
  fi
}

sign() { # file
  codesign --force --sign - "$1" || fail "codesign couldn't sign $(basename "$1")."
}

place() {
  rm -rf "$out"
  mkdir -p "$out"
  if [ "$os" = macos ]; then
    cp "$work/build-arm64/bin/bsdtar" "$out/$arm"
    cp "$work/build-x86_64/bin/bsdtar" "$out/$intel"
    chmod 755 "$out/$arm" "$out/$intel"
    sign "$out/$arm"
    sign "$out/$intel"
    # lipo breaks the halves' signatures (D25), so the merged file is signed
    # again; tauri-build wants the per-arch files too.
    lipo -create -output "$out/$main" "$out/$arm" "$out/$intel" || fail "lipo couldn't merge the two builds."
    chmod 755 "$out/$main"
    sign "$out/$main"
  else
    cp "$work/build/bin/bsdtar" "$out/$main"
    chmod 755 "$out/$main"
  fi
  {
    echo "Grape's bsdtar (DECISIONS D27), built by scripts/bsdtar/build.sh"
    echo "sources.json SHA-256: $(sha256 "$sources")"
    echo "marker: $marker"
    echo "libarchive: $version, $(basename "$tarball") SHA-256 $want_sha"
    echo "patches:"
    printf '%s\n' "$plan" | awk -F '\t' '$1 == "P" { print "  " $2 " " $3 }'
    echo "cmake: $(cmake --version | sed -n 1p)"
    if [ "$os" = macos ]; then
      echo "compiler: $(clang --version | sed -n 1p)"
      echo "sdk: macOS $(xcrun --sdk macosx --show-sdk-version 2>/dev/null || echo unknown) ($(xcrun --sdk macosx --show-sdk-path 2>/dev/null || echo unknown))"
      echo "deployment target: $deploy"
      echo "os: $(sw_vers -productName) $(sw_vers -productVersion) ($(sw_vers -buildVersion)), $(uname -m)"
    else
      echo "compiler: $(cc --version | sed -n 1p)"
      pretty=$(sed -n 's/^PRETTY_NAME="\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' /etc/os-release 2>/dev/null || true)
      echo "os: ${pretty:-Linux}, $(uname -srm)"
    fi
    echo "built: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  } >"$out/BUILDINFO.txt"
  (
    cd "$out"
    for f in $binaries; do
      printf '%s  %s\n' "$(sha256 "$f")" "$f"
    done
  ) >"$out/SHA256SUMS"
}

# Runs `<command> --version` and checks the first line against sources.json.
check_version() { # label, then the command
  local label=$1 output first rest tok ok
  shift
  output=$("$@" --version 2>/dev/null) || fail "$label didn't run with --version."
  first=$(printf '%s\n' "$output" | sed -n 1p)
  if [ "$first" != "$version_line" ]; then
    fail "$label's version line is '$first', but sources.json wants '$version_line' (trailing space included)."
  fi
  rest=${first#"bsdtar $version-$marker - libarchive $version"}
  [ "$rest" != "$first" ] || fail "$label's version line doesn't start with 'bsdtar $version-$marker - libarchive $version'."
  for tok in $rest; do
    ok=0
    for allowed in $allowed_tokens; do
      [ "$tok" = "$allowed" ] && ok=1
    done
    [ "$ok" -eq 1 ] || fail "$label's version line has '$tok': a library got into the build."
  done
  echo "  $label: $first"
}

check_libsystem_only() { # file
  local listing bad
  listing=$(otool -L "$1") || fail "otool can't read $(basename "$1")."
  bad=$(printf '%s\n' "$listing" | awk '
    /:$/ { next }                          # "file:" or "file (architecture x):"
    { sub(/^[[:space:]]+/, ""); sub(/ \(compatibility.*$/, ""); if ($0 != "/usr/lib/libSystem.B.dylib") print }')
  [ -z "$bad" ] || fail "$(basename "$1") links more than libSystem: $bad"
}

check_minos() { # file, arch or "" for a thin file
  local shown
  if [ -n "$2" ]; then
    shown=$(vtool -arch "$2" -show-build "$1") || fail "vtool can't read the $2 half of $(basename "$1")."
  else
    shown=$(vtool -show-build "$1") || fail "vtool can't read $(basename "$1")."
  fi
  printf '%s\n' "$shown" | grep -Eq "^[[:space:]]*minos $deploy\$" ||
    fail "$(basename "$1")${2:+ ($2)} isn't built for minimum macOS $deploy."
}

check_sums() {
  [ -f "$out/SHA256SUMS" ] || fail "out/SHA256SUMS is missing."
  local f want got
  for f in $binaries; do
    want=$(awk -v f="$f" '$2 == f { print $1 }' "$out/SHA256SUMS")
    got=$(sha256 "$out/$f")
    [ -n "$want" ] && [ "$want" = "$got" ] || fail "out/$f doesn't match out/SHA256SUMS."
  done
  [ -f "$out/BUILDINFO.txt" ] || fail "out/BUILDINFO.txt is missing."
}

run_checks() {
  local f archs native other
  for f in $binaries; do
    [ -f "$out/$f" ] || fail "out/$f is missing. Build it first (without --check-only)."
  done
  check_sums
  if [ "$os" = macos ]; then
    [ "$(lipo -archs "$out/$arm")" = arm64 ] || fail "$arm isn't arm64 only."
    [ "$(lipo -archs "$out/$intel")" = x86_64 ] || fail "$intel isn't x86_64 only."
    archs=$(lipo -archs "$out/$main") || fail "lipo can't read $main."
    case "$archs" in
      "x86_64 arm64" | "arm64 x86_64") ;;
      *) fail "$main holds '$archs', not x86_64 and arm64." ;;
    esac
    for f in $binaries; do
      check_libsystem_only "$out/$f"
      codesign --verify --strict "$out/$f" || fail "$f's signature doesn't verify."
    done
    check_minos "$out/$arm" ""
    check_minos "$out/$intel" ""
    check_minos "$out/$main" arm64
    check_minos "$out/$main" x86_64
    echo "  Linked against libSystem only, arm64 + x86_64, minimum macOS $deploy, signed ad hoc."
    check_version "$main" "$out/$main"
    native=$(uname -m)
    case "$native" in
      arm64) other=x86_64 ;;
      *) other=arm64 ;;
    esac
    # Rosetta lets an Apple silicon Mac run the Intel half too.
    if arch "-$other" /usr/bin/true >/dev/null 2>&1; then
      check_version "$main ($other)" arch "-$other" "$out/$main"
    else
      echo "  Notice: this Mac can't run the $other half (no Rosetta), so only the $native half ran. The static checks above cover both."
    fi
  else
    local listing bad
    listing=$(ldd "$out/$main") || fail "ldd can't read $main."
    bad=$(printf '%s\n' "$listing" | awk '
      { name = $1 }
      name ~ /^linux-vdso\.so/ || name ~ /^libc\.so\./ || name ~ /(^|\/)ld-linux[^\/]*\.so/ { next }
      { print name }')
    [ -z "$bad" ] || fail "$main links more than libc: $bad"
    echo "  Linked against libc only."
    check_version "$main" "$out/$main"
  fi
}

check_notices() { # patched source tree
  local fresh="$root/copyright-lines.txt"
  copyright_lines "$1" >"$fresh" || fail "couldn't list the copyright lines."
  if ! cmp -s "$fresh" "$notice_lines"; then
    if [ -f "$notice_lines" ]; then diff -u "$notice_lines" "$fresh" >&2 || true; fi
    fail "licenses/libarchive/copyright-lines.txt doesn't match the copyright lines of the sources this build compiled (diff above). The pin moved and the notices didn't: check $fresh and copy it there (licenses/README.md)."
  fi
  rm -f "$fresh"
  echo "  licenses/libarchive/copyright-lines.txt matches the compiled sources."
}

if [ "$check_only" -eq 0 ]; then
  fetch_tarball
  rm -rf "$work"
  mkdir -p "$work/src"
  "$tar_bin" -xzf "$tarball" -C "$work/src" || fail "tar couldn't extract $(basename "$tarball")."
  src="$work/src/libarchive-$version"
  [ -d "$src" ] || fail "the tarball has no libarchive-$version folder."
  apply_patches "$src"
  build "$src"
  place
  echo "Checking the results..."
  run_checks
  check_notices "$src"
else
  echo "Checking what $out holds..."
  run_checks
fi

if [ "$install" -eq 1 ]; then
  mkdir -p "$bin_dir"
  for f in $binaries; do
    cp "$out/$f" "$bin_dir/$f.tmp"
    chmod 755 "$bin_dir/$f.tmp"
    mv -f "$bin_dir/$f.tmp" "$bin_dir/$f"
  done
  echo "Installed $binaries into apps/desktop/src-tauri/binaries."
fi

echo "bsdtar is ready in $out:"
for f in $binaries; do
  printf '  %-36s %10s bytes\n' "$f" "$(file_size "$out/$f")"
done
