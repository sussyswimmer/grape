#!/usr/bin/env bash
# Builds Grape's own FFmpeg: ffmpeg and ffprobe with only the parts Grape
# uses, statically linked, with networking compiled out (DECISIONS D24).
#
#   scripts/ffmpeg/build.sh linux-x86_64     on Linux; for local testing only
#   scripts/ffmpeg/build.sh windows-x86_64   on Linux, with mingw-w64 (win32 threads)
#   scripts/ffmpeg/build.sh macos-arm64      on an Apple silicon Mac
#   scripts/ffmpeg/build.sh macos-x86_64     on an Intel Mac
#
# Every source is pinned in scripts/ffmpeg/sources.json. Each tarball is
# downloaded into a new, empty folder, and its size and SHA-256 are checked
# before anything reads it. Then its file list is checked (one top folder,
# no absolute paths, no "..", no link that leaves the folder) before it's
# unpacked. The libraries are built as static libraries into a private
# prefix, and FFmpeg is configured against that prefix only (pkg-config
# looks nowhere else).
#
# Writes:
#   dist/<target>/ffmpeg[.exe], ffprobe[.exe]
#   dist/<target>/BUILDINFO.txt   versions, configure line, hashes, toolchain
#   dist/grape-ffmpeg-sources.tar every tarball exactly as downloaded, plus
#                                 scripts/ffmpeg/ and licenses/ffmpeg/: the
#                                 corresponding source the GPL asks for
#
# Then it checks what it built: only the file and pipe protocols, no
# network code, GPL-2.0-or-later (never version3 or nonfree), every
# component Grape uses, on Windows that the exe needs no DLL beyond
# Windows' own, and that licenses/ffmpeg/ffmpeg-notices.txt has every
# license notice in the files it compiled (scripts/ffmpeg/notices.py).
#
# Environment (all optional):
#   GRAPE_FFMPEG_WORK      build trees, default <repo>/build/ffmpeg. The
#                          target's folder in it is deleted and made again
#                          on every run.
#   GRAPE_FFMPEG_DIST      outputs, default <repo>/dist
#   GRAPE_FFMPEG_TARBALLS  a folder that already holds the tarballs (an
#                          unpacked source bundle's tarballs/, or a cache).
#                          A file found there is copied instead of
#                          downloaded, and checked the same way.
#   GRAPE_FFMPEG_JOBS      parallel jobs, default the number of CPUs
#
# scripts/ffmpeg/README.md lists what each OS needs installed. Works with
# macOS's bash 3.2.

set -euo pipefail
export LC_ALL=C

me="build.sh"

fail() {
  echo "$me: $*" >&2
  exit 1
}

note() {
  echo "==> $*"
}

usage() {
  cat <<'EOF'
Usage: scripts/ffmpeg/build.sh <target>

Targets:
  linux-x86_64     on Linux (local testing only)
  windows-x86_64   on Linux, cross-compiled with mingw-w64 (win32 threads)
  macos-arm64      on an Apple silicon Mac
  macos-x86_64     on an Intel Mac

Builds ffmpeg and ffprobe from the sources pinned in scripts/ffmpeg/sources.json
into dist/<target>/, with BUILDINFO.txt and dist/grape-ffmpeg-sources.tar.
See scripts/ffmpeg/README.md.
EOF
}

target=""
for arg in "$@"; do
  case "$arg" in
    --help | -h)
      usage
      exit 0
      ;;
    -*) fail "unknown option '$arg'. Try --help." ;;
    *)
      [ -z "$target" ] || fail "give one target. Try --help."
      target=$arg
      ;;
  esac
done
[ -n "$target" ] || {
  usage >&2
  exit 2
}

host_os=$(uname -s)
host_arch=$(uname -m)
case "$target:$host_os:$host_arch" in
  linux-x86_64:Linux:x86_64 | windows-x86_64:Linux:* | macos-arm64:Darwin:arm64 | macos-x86_64:Darwin:x86_64) ;;
  linux-x86_64:*) fail "linux-x86_64 builds on x86_64 Linux only." ;;
  windows-x86_64:*) fail "windows-x86_64 is cross-compiled on Linux with mingw-w64." ;;
  macos-arm64:*) fail "macos-arm64 builds natively on an Apple silicon Mac." ;;
  macos-x86_64:*) fail "macos-x86_64 builds natively on an Intel Mac (Rosetta isn't enough: uname -m must say x86_64)." ;;
  *) fail "unknown target '$target'. Try --help." ;;
esac

script_dir=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$script_dir/../.." && pwd)
sources_json="$script_dir/sources.json"
licenses_dir="$repo/licenses/ffmpeg"
[ -f "$sources_json" ] || fail "can't find scripts/ffmpeg/sources.json."
[ -d "$licenses_dir" ] || fail "licenses/ffmpeg/ is missing; the source bundle needs it."

work_root=${GRAPE_FFMPEG_WORK:-$repo/build/ffmpeg}
dist_root=${GRAPE_FFMPEG_DIST:-$repo/dist}
tarballs=${GRAPE_FFMPEG_TARBALLS:-}
jobs=${GRAPE_FFMPEG_JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)}
case "$jobs" in
  '' | *[!0-9]*) fail "GRAPE_FFMPEG_JOBS must be a number." ;;
esac

# --- Tools -----------------------------------------------------------------

need() {
  command -v "$1" >/dev/null 2>&1 || fail "$1 is missing. scripts/ffmpeg/README.md lists what to install."
}

for tool in curl python3 tar make pkg-config meson ninja; do
  need "$tool"
done
# GNU tar runs these to unpack; macOS's tar has them built in.
if [ "$host_os" = "Linux" ]; then
  for tool in xz bzip2 gzip; do
    need "$tool"
  done
fi
python3 -I -c 'import json, tarfile, lzma, bz2' >/dev/null 2>&1 ||
  fail "python3 doesn't run, or lacks its lzma/bz2 modules."

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum <"$1" | awk '{ print $1 }'; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 <"$1" | awk '{ print $1 }'; }
else
  fail "sha256sum (or shasum) is missing."
fi

file_size() {
  wc -c <"$1" | tr -d ' '
}

# --- Target settings -------------------------------------------------------

exe=""
cross=""           # the mingw-w64 tool prefix, Windows only
autotools_host=()  # --build/--host for autotools libraries
macos_min="11.0"
case "$target" in
  linux-x86_64)
    cc=gcc
    ;;
  windows-x86_64)
    cross=x86_64-w64-mingw32
    cc=$cross-gcc
    exe=".exe"
    autotools_host=(--host="$cross")
    ;;
  macos-arm64)
    cc=clang
    autotools_host=(--build=aarch64-apple-darwin --host=aarch64-apple-darwin)
    ;;
  macos-x86_64)
    cc=clang
    autotools_host=(--build=x86_64-apple-darwin --host=x86_64-apple-darwin)
    ;;
esac
need "$cc"
case "$target" in
  *x86_64) need nasm ;;
esac
if [ -n "$cross" ]; then
  for tool in ar ranlib strip nm windres objdump; do
    need "$cross-$tool"
  done
  # Only the win32 thread model keeps winpthreads out of the exe.
  # (sed, not grep -q, which would cut gcc's output short under pipefail)
  model=$("$cc" -v 2>&1 | sed -n 's/^Thread model: //p')
  [ "$model" = "win32" ] ||
    fail "$cc doesn't use the win32 thread model. On Debian or Ubuntu: sudo update-alternatives --set $cc /usr/bin/$cc-win32"
fi
if [ "$host_os" = "Darwin" ]; then
  need xcrun
  # Apple's clang and ld take the deployment target from here when no
  # -mmacosx-version-min is given, as in the autotools libraries.
  export MACOSX_DEPLOYMENT_TARGET="$macos_min"
fi

# --- Work folders ----------------------------------------------------------

mkdir -p "$work_root" "$dist_root"
work_root=$(cd "$work_root" && pwd)
dist_root=$(cd "$dist_root" && pwd)
# Autotools and FFmpeg's configure break on spaces and shell characters.
case "$work_root" in
  *[!A-Za-z0-9._/+-]*) fail "the work folder '$work_root' has characters the libraries' build systems can't handle. Set GRAPE_FFMPEG_WORK to a plain path." ;;
esac
if [ -n "$tarballs" ]; then
  [ -d "$tarballs" ] || fail "GRAPE_FFMPEG_TARBALLS ('$tarballs') isn't a folder."
  tarballs=$(cd "$tarballs" && pwd)
fi

tdir="$work_root/$target"
marker="$tdir/.grape-ffmpeg-build"
if [ -e "$tdir" ]; then
  # Only ever delete a folder this script made.
  [ -f "$marker" ] || [ -z "$(ls -A "$tdir")" ] ||
    fail "$tdir exists and wasn't made by this script. Move it or set GRAPE_FFMPEG_WORK."
  rm -rf "$tdir"
fi
mkdir -p "$tdir"
: >"$marker"
downloads="$tdir/downloads"
src="$tdir/src"
prefix="$tdir/prefix"
logs="$tdir/logs"
mkdir "$downloads" "$src" "$prefix" "$logs"

# pkg-config sees the private prefix and nothing else.
export PKG_CONFIG_LIBDIR="$prefix/lib/pkgconfig"
unset PKG_CONFIG_PATH PKG_CONFIG_SYSROOT_DIR
# Keep a user's flags out of the libraries' build systems.
unset CFLAGS CXXFLAGS CPPFLAGS LDFLAGS
# Tools that stamp a date take this one instead of the clock (strip writes
# it into the Windows exe's header), so two builds can match byte for byte.
export SOURCE_DATE_EPOCH=0
export CC="$cc"
if [ -n "$cross" ]; then
  export AR="$cross-ar" RANLIB="$cross-ranlib" STRIP="$cross-strip" NM="$cross-nm"
fi

started=$SECONDS
step_started=$SECONDS

# Runs a command with its output in logs/<name>.log; on failure shows the
# end of the log.
run() {
  local name=$1 log
  shift
  log="$logs/$name.log"
  if ! "$@" >>"$log" 2>&1; then
    echo "---- last lines of $log ----" >&2
    tail -n 60 "$log" >&2
    fail "$name failed: $*"
  fi
}

step_done() {
  echo "    done in $((SECONDS - step_started)) s"
  step_started=$SECONDS
}

# --- Sources ---------------------------------------------------------------

# Checks sources.json and prints one tab-separated line per source:
#   <name> <version or commit> <url> <file> <size> <sha256> <dir>
read_sources() {
  python3 -I - "$sources_json" <<'PY'
import json
import re
import sys

NAMES = ("zlib", "libogg", "libvorbis", "opus", "lame", "x264", "libvpx", "dav1d", "ffmpeg")


def die(msg):
    sys.stderr.write("build.sh: scripts/ffmpeg/sources.json: " + msg + "\n")
    sys.exit(1)


def no_duplicates(pairs):
    seen = {}
    for key, value in pairs:
        if key in seen:
            die("the key '%s' appears twice in one object." % key)
        seen[key] = value
    return seen


try:
    with open(sys.argv[1], encoding="utf-8") as f:
        data = json.load(f, object_pairs_hook=no_duplicates)
except (OSError, ValueError) as e:
    die("can't read it as JSON (%s)." % e)

if not isinstance(data, dict) or data.get("format") != 1 or type(data.get("format")) is not int:
    die("'format' must be 1.")
sources = data.get("sources")
if not isinstance(sources, dict):
    die("'sources' must be an object.")
names = [k for k in sources if not k.startswith("_")]
if sorted(names) != sorted(NAMES):
    die("it must pin exactly these sources: %s." % ", ".join(NAMES))

URL = re.compile(r"^https://[^\s\"'\\]+$")
SHA = re.compile(r"^[0-9a-f]{64}$")
PLAIN = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._+-]*$")
REQUIRED = ("url", "file", "size", "sha256", "dir", "license", "license_files", "released")
OPTIONAL = ("version", "commit")

for name in NAMES:
    s = sources[name]
    where = "sources.%s" % name
    if not isinstance(s, dict):
        die("%s must be an object." % where)
    for key in s:
        if not key.startswith("_") and key not in REQUIRED + OPTIONAL:
            die("%s has an unknown key '%s'." % (where, key))
    for key in REQUIRED:
        if key not in s:
            die("%s is missing '%s'." % (where, key))
    if ("version" in s) == ("commit" in s):
        die("%s needs a version or a commit (one of them)." % where)
    pin = s.get("version", s.get("commit"))
    if not isinstance(pin, str) or not PLAIN.match(pin):
        die("%s: the version or commit must be plain letters, digits and dots." % where)
    if not isinstance(s["url"], str) or not URL.match(s["url"]):
        die("%s.url must be an https:// URL." % where)
    for key in ("file", "dir"):
        if not isinstance(s[key], str) or not PLAIN.match(s[key]):
            die("%s.%s must be a plain name, no slashes." % (where, key))
    if not isinstance(s["sha256"], str) or not SHA.match(s["sha256"]):
        die("%s.sha256 must be 64 lower-case hex digits." % where)
    if type(s["size"]) is not int or s["size"] <= 0:
        die("%s.size must be the size in bytes." % where)
    files = s["license_files"]
    if not isinstance(files, list) or not files or not all(isinstance(p, str) and p for p in files):
        die("%s.license_files must list paths inside the tarball's folder." % where)
    for p in files:
        if p.startswith("/") or "\\" in p or any(part in ("", ".", "..") for part in p.split("/")):
            die("%s.license_files has a bad path '%s'." % (where, p))
    print("\t".join([name, pin, s["url"], s["file"], str(s["size"]), s["sha256"], s["dir"]]))
PY
}
plan=$(read_sources) || fail "sources.json has a problem (above)."

# The value of column <n> for source <name>.
source_field() {
  printf '%s\n' "$plan" | awk -F '\t' -v n="$1" -v c="$2" '$1 == n { print $c }'
}

# Checks a tarball's member list before anything is unpacked: one top
# folder named <dir>, no absolute paths or "..", only files, folders and
# links, and no link that points outside <dir>.
check_tarball() {
  python3 -I - "$1" "$2" <<'PY'
import posixpath
import sys
import tarfile

path, top = sys.argv[1], sys.argv[2]


def die(msg):
    sys.stderr.write("build.sh: %s: %s\n" % (path, msg))
    sys.exit(1)


def inside(name):
    norm = posixpath.normpath(name)
    return norm == top or norm.startswith(top + "/")


with tarfile.open(path) as tar:
    count = 0
    for m in tar:
        count += 1
        name = m.name
        if name.startswith("/") or "\\" in name or ".." in name.split("/"):
            die("has an unsafe member name '%s'." % name)
        if not inside(name):
            die("has '%s' outside its folder '%s'." % (name, top))
        if not (m.isfile() or m.isdir() or m.issym() or m.islnk()):
            die("has '%s', which isn't a file, folder or link." % name)
        if m.issym():
            target = posixpath.normpath(posixpath.join(posixpath.dirname(name), m.linkname))
            if m.linkname.startswith("/") or not inside(target):
                die("has a link '%s' that points outside its folder." % name)
        if m.islnk() and not inside(m.linkname):
            die("has a hard link '%s' to outside its folder." % name)
    if count == 0:
        die("is empty.")
PY
}

note "Fetching and checking the sources for $target"
while IFS=$'\t' read -r name pin url file want_size want_sha dir; do
  dest="$downloads/$file"
  if [ -n "$tarballs" ] && [ -f "$tarballs/$file" ]; then
    echo "  $name $pin: from $tarballs/$file"
    cp "$tarballs/$file" "$dest"
  else
    echo "  $name $pin: downloading $url"
    if ! curl --fail --location --proto '=https' --proto-redir '=https' --tlsv1.2 \
      --silent --show-error --retry 3 --connect-timeout 30 \
      --output "$dest" "$url"; then
      rm -f "$dest"
      fail "couldn't download $name from $url."
    fi
  fi
  size=$(file_size "$dest")
  if [ "$size" != "$want_size" ]; then
    rm -f "$dest"
    fail "$file is $size bytes, but sources.json says $want_size. Deleted it; nothing was unpacked."
  fi
  got_sha=$(sha256 "$dest")
  if [ "$got_sha" != "$want_sha" ]; then
    rm -f "$dest"
    fail "$file has SHA-256 $got_sha, but sources.json says $want_sha. Deleted it; nothing was unpacked."
  fi
  check_tarball "$dest" "$dir" || fail "$file failed the file-list check (above); nothing was unpacked."
  tar -xf "$dest" -C "$src" --no-same-owner || fail "couldn't unpack $file."
  [ -d "$src/$dir" ] || fail "$file didn't unpack to $dir/."
done <<<"$plan"
step_done

srcdir() {
  printf '%s\n' "$src/$(source_field "$1" 7)"
}

# --- Libraries -------------------------------------------------------------

build_zlib() {
  local d
  d=$(srcdir zlib)
  note "zlib"
  (
    cd "$d"
    if [ -n "$cross" ]; then
      export CHOST="$cross"
    fi
    run zlib-configure ./configure --static --prefix="$prefix"
    run zlib-build make -j"$jobs" libz.a
    run zlib-install make install
  )
  step_done
}

# An autotools library: name, then its configure options.
build_autotools() {
  local name=$1 d
  shift
  d=$(srcdir "$name")
  note "$name"
  (
    cd "$d"
    run "$name-configure" ./configure "${autotools_host[@]+"${autotools_host[@]}"}" \
      --prefix="$prefix" --disable-shared --enable-static "$@"
    run "$name-build" make -j"$jobs"
    run "$name-install" make install
  )
  step_done
}

build_x264() {
  local d args
  d=$(srcdir x264)
  note "x264"
  args=(--prefix="$prefix" --enable-static --disable-cli --disable-opencl
    --disable-avs --disable-swscale --disable-lavf --disable-ffms --disable-gpac
    --disable-lsmash --disable-bashcompletion --bit-depth=8)
  case "$target" in
    linux-x86_64) args+=(--enable-pic) ;;
    windows-x86_64) args+=(--host="$cross" --cross-prefix="$cross-") ;;
    macos-*)
      args+=(--extra-cflags="-mmacosx-version-min=$macos_min"
        --extra-ldflags="-mmacosx-version-min=$macos_min")
      ;;
  esac
  (
    cd "$d"
    run x264-configure ./configure "${args[@]}"
    run x264-build make -j"$jobs" lib-static
    run x264-install make install-lib-static
  )
  step_done
}

build_libvpx() {
  local d b args vpx_target
  d=$(srcdir libvpx)
  b="$tdir/build-libvpx"
  note "libvpx"
  # libvpx compiles its configure line into the library (vpx_codec_build_config),
  # so it gets a prefix that names no folder of this machine: /prefix, which
  # make install puts under DESTDIR=$tdir, that is into $prefix. vpx.pc is
  # pointed back at $prefix afterwards.
  args=(--prefix=/prefix --disable-shared --enable-static --disable-examples
    --disable-tools --disable-docs --disable-unit-tests --disable-webm-io
    --disable-libyuv --disable-vp8 --disable-vp9-decoder --enable-vp9-encoder)
  case "$target" in
    linux-x86_64)
      vpx_target=x86_64-linux-gcc
      args+=(--enable-pic --as=nasm)
      ;;
    windows-x86_64)
      vpx_target=x86_64-win64-gcc
      # libvpx uses pthreads whenever it finds pthread.h, which mingw-w64
      # ships (winpthreads). A pthread.h that doesn't compile makes it
      # use Win32 threads, as with MSVC, so no winpthreads in the exe.
      mkdir -p "$tdir/no-pthread"
      printf '#error "Grape builds libvpx with Win32 threads"\n' >"$tdir/no-pthread/pthread.h"
      # (Relative to the build folder, again to keep paths out of the library.)
      args+=(--as=nasm --extra-cflags=-I../no-pthread)
      ;;
    macos-arm64) vpx_target=arm64-darwin20-gcc ;;
    macos-x86_64)
      vpx_target=x86_64-darwin20-gcc
      args+=(--as=nasm)
      ;;
  esac
  mkdir -p "$b"
  (
    cd "$b"
    if [ -n "$cross" ]; then
      export CROSS="$cross-"
    fi
    run libvpx-configure "$d/configure" --target="$vpx_target" "${args[@]}"
    run libvpx-build make -j"$jobs"
    run libvpx-install make install DESTDIR="$tdir"
  )
  [ "$tdir/prefix" = "$prefix" ] || fail "libvpx's install folder isn't the prefix."
  sed "s#^prefix=/prefix\$#prefix=$prefix#" "$prefix/lib/pkgconfig/vpx.pc" >"$tdir/vpx.pc"
  grep -qx "prefix=$prefix" "$tdir/vpx.pc" || fail "couldn't point vpx.pc at the prefix."
  mv -f "$tdir/vpx.pc" "$prefix/lib/pkgconfig/vpx.pc"
  if [ -n "$cross" ]; then
    grep -q '^#define HAVE_PTHREAD_H 0$' "$b/vpx_config.h" ||
      fail "libvpx would use winpthreads (HAVE_PTHREAD_H isn't 0)."
  fi
  step_done
}

build_dav1d() {
  local d b args
  d=$(srcdir dav1d)
  b="$tdir/build-dav1d"
  note "dav1d"
  args=(--prefix="$prefix" --libdir=lib --buildtype=release --default-library=static
    --wrap-mode=nodownload -Denable_tools=false -Denable_tests=false
    -Denable_examples=false -Denable_docs=false -Dxxhash_muxer=disabled)
  if [ -n "$cross" ]; then
    cat >"$tdir/meson-cross.txt" <<EOF
[binaries]
c = '$cross-gcc'
ar = '$cross-ar'
strip = '$cross-strip'
windres = '$cross-windres'
pkg-config = 'pkg-config'

[host_machine]
system = 'windows'
cpu_family = 'x86_64'
cpu = 'x86_64'
endian = 'little'
EOF
    args+=(--cross-file="$tdir/meson-cross.txt")
  fi
  if [ "$host_os" = "Darwin" ]; then
    args+=(-Dc_args="-mmacosx-version-min=$macos_min" -Dc_link_args="-mmacosx-version-min=$macos_min")
  fi
  # In a cross build, meson takes CC and the like for the build machine's
  # compiler, so they must not name the mingw-w64 tools.
  run dav1d-configure env -u CC -u AR -u RANLIB -u STRIP -u NM meson setup "$b" "$d" "${args[@]}"
  run dav1d-build ninja -C "$b" -j "$jobs"
  run dav1d-install ninja -C "$b" install
  step_done
}

build_zlib
build_autotools libogg
if [ "$host_os" = "Darwin" ]; then
  # libvorbis's configure adds -force_cpusubtype_ALL on macOS, an option of
  # Apple's old ld64 linker that current Xcode doesn't need and may not
  # take. This is the one change the script makes to a source (untested,
  # like all of the macOS path).
  d=$(srcdir libvorbis)
  sed 's/ -force_cpusubtype_ALL//g' "$d/configure" >"$d/configure.grape"
  chmod 755 "$d/configure.grape"
  mv -f "$d/configure.grape" "$d/configure"
fi
build_autotools libvorbis --with-ogg="$prefix" --disable-oggtest --disable-docs --disable-examples
build_autotools opus --disable-doc --disable-extra-programs
build_autotools lame --disable-frontend --disable-decoder --disable-analyzer-hooks --disable-gtktest
build_x264
build_libvpx
build_dav1d

# --- FFmpeg ----------------------------------------------------------------

# The components Grape uses (crates/grape-engines/src/media/args.rs), and
# the filters ffmpeg inserts on its own: transpose/hflip/vflip/rotate turn a
# rotated video upright, crop applies a container's crop, trim/atrim cut to
# -ss/-t, scale/aresample/format/aformat/premultiply_dynamic convert between
# filters, null/anull are the default graph. (FFmpeg's configure selects
# most of those for the ffmpeg program anyway.) lavfi with sine, testsrc
# and pan is only for the engine tests, which make their sources with this
# ffmpeg; Grape's -format_whitelist keeps it out of reach of Grape's argv.
protocols="file,pipe"
demuxers="mov,matroska,avi,asf,mp3,flac,ogg,wav,aiff,gif,image_png_pipe"
muxers="mp4,mov,matroska,webm,avi,asf,gif,mp3,ipod,wav,flac,ogg,opus,aiff,image2pipe"
encoders="libx264,aac,libvpx_vp9,libopus,mpeg4,libmp3lame,wmv2,wmav2,gif,png,pcm_s16le,pcm_s24le,pcm_s16be,pcm_s24be,flac,libvorbis"
filters="scale,fps,palettegen,paletteuse,format,aformat,aresample,null,anull,transpose,hflip,vflip,rotate,crop,trim,atrim,premultiply_dynamic,sine,testsrc,pan"
# FFmpeg's own decoders come first anyway; these copies are never picked.
library_decoders="libopus,libvorbis,libvpx_vp8,libvpx_vp9"

ff_args=(
  --pkg-config=pkg-config --pkg-config-flags=--static
  --enable-gpl --enable-static --disable-shared
  --disable-autodetect --disable-network --disable-doc --disable-ffplay --disable-debug
  --disable-everything
  --enable-protocol="$protocols"
  --enable-demuxer="$demuxers"
  --enable-muxer="$muxers"
  --enable-decoders --disable-decoder="$library_decoders"
  --enable-parsers --enable-bsfs
  --enable-encoder="$encoders"
  --enable-filter="$filters"
  --enable-indev=lavfi
  --enable-libx264 --enable-libmp3lame --enable-libopus --enable-libvorbis
  --enable-libvpx --enable-libdav1d --enable-zlib
)
case "$target" in
  windows-x86_64)
    ff_args+=(--target-os=mingw32 --arch=x86_64 --cross-prefix="$cross-" --enable-cross-compile
      --enable-w32threads)
    ;;
  *) ff_args+=(--enable-pthreads) ;;
esac
# Relative to FFmpeg's folder (src/ffmpeg-x), so no path of this machine
# ends up in the configure line that ffmpeg -version prints.
ff_args+=(--extra-cflags=-I../../prefix/include --extra-ldflags=-L../../prefix/lib)
case "$target" in
  # -static: no DLLs but Windows' own. No timestamp in the PE header, so two
  # builds of one commit can come out byte for byte the same.
  windows-x86_64) ff_args+=(--extra-ldflags=-static "--extra-ldflags=-Wl,--no-insert-timestamp") ;;
  macos-*)
    ff_args+=(--extra-cflags="-mmacosx-version-min=$macos_min"
      --extra-ldflags="-mmacosx-version-min=$macos_min")
    ;;
esac
ff_args+=(--extra-version=grape)

ffdir=$(srcdir ffmpeg)
note "FFmpeg"
(
  cd "$ffdir"
  run ffmpeg-configure ./configure "${ff_args[@]}"
  run ffmpeg-build make -j"$jobs"
)
step_done

# --- Checks ----------------------------------------------------------------

note "Checking the build"

# The configure line as ffmpeg -version prints it, from config.h.
configuration=$(python3 -I - "$ffdir/config.h" <<'PY'
import re
import sys

text = open(sys.argv[1], encoding="utf-8").read()
m = re.search(r'^#define FFMPEG_CONFIGURATION "((?:[^"\\]|\\.)*)"$', text, re.M)
if not m:
    sys.exit("build.sh: config.h has no FFMPEG_CONFIGURATION")
print(re.sub(r"\\(.)", r"\1", m.group(1)))
PY
)

config_is() { # name, value
  grep -qx "#define $1 $2" "$ffdir/config.h" || fail "config.h doesn't have $1 $2."
}
config_is CONFIG_NETWORK 0
config_is CONFIG_GPL 1
config_is CONFIG_VERSION3 0
config_is CONFIG_NONFREE 0
config_is HAVE_THREADS 1
case "$configuration" in
  *--enable-nonfree* | *--enable-version3*) fail "the configure line has --enable-nonfree or --enable-version3." ;;
esac

# The components compiled in, from the lists configure generated:
# list file, C type suffix ("_muxer"), then the names that must be there.
# With "exact", nothing else may be.
components() { # file, suffix
  sed -n "s/^ *&ff_\([a-z0-9_]*\)$2,\$/\1/p" "$ffdir/$1" | sort
}
check_components() { # file, suffix, exact|includes, comma-separated names
  local file=$1 suffix=$2 mode=$3 want have missing extra
  want=$(printf '%s\n' "$4" | tr ',' '\n' | sort)
  have=$(components "$file" "$suffix")
  missing=$(comm -23 <(printf '%s\n' "$want") <(printf '%s\n' "$have") | tr '\n' ' ')
  [ -z "$missing" ] || fail "$file lacks: $missing"
  if [ "$mode" = exact ]; then
    extra=$(comm -13 <(printf '%s\n' "$want") <(printf '%s\n' "$have") | tr '\n' ' ')
    [ -z "$extra" ] || fail "$file has more than Grape asked for: $extra"
  fi
}
check_components libavformat/protocol_list.c _protocol exact "$protocols"
check_components libavformat/demuxer_list.c _demuxer exact "$demuxers"
check_components libavformat/muxer_list.c _muxer exact "$muxers"
check_components libavdevice/indev_list.c _demuxer exact "lavfi"
[ -z "$(components libavdevice/outdev_list.c _muxer)" ] || fail "an output device is compiled in."
encoder_list=$(sed -n 's/^ *&ff_\([a-z0-9_]*\)_encoder,$/\1/p' "$ffdir/libavcodec/codec_list.c" | sort)
# FFmpeg's MPEG-4 encoder is built on its H.263 encoder, which comes along.
want_encoders=$(printf '%s\n' "$encoders,h263" | tr ',' '\n' | sort)
[ "$encoder_list" = "$want_encoders" ] ||
  fail "the encoders compiled in aren't exactly Grape's: $(printf '%s' "$encoder_list" | tr '\n' ' ')"
decoder_count=$(grep -c '_decoder,$' "$ffdir/libavcodec/codec_list.c" || true)
[ "$decoder_count" -gt 300 ] || fail "only $decoder_count decoders are compiled in."
grep -q '&ff_libdav1d_decoder,$' "$ffdir/libavcodec/codec_list.c" || fail "the dav1d decoder is missing."
for d in libopus libvorbis libvpx_vp8 libvpx_vp9; do
  if grep -q "&ff_${d}_decoder,\$" "$ffdir/libavcodec/codec_list.c"; then
    fail "the $d decoder is compiled in."
  fi
done
parser_count=$(grep -c '_parser,$' "$ffdir/libavcodec/parser_list.c" || true)
bsf_count=$(grep -c '_bsf,$' "$ffdir/libavcodec/bsf_list.c" || true)
# ff_vf_scale, ff_asrc_sine, ...: the name after the kind.
filter_names=$(sed -n 's/^ *&ff_[av][a-z]*_\([a-z0-9_]*\),$/\1/p' "$ffdir/libavfilter/filter_list.c" | sort -u)
want_filters=$(printf '%s\n' "$filters" | tr ',' '\n' | sort)
missing=$(comm -23 <(printf '%s\n' "$want_filters") <(printf '%s\n' "$filter_names") | tr '\n' ' ')
[ -z "$missing" ] || fail "filters missing: $missing"
extra=$(comm -13 <(printf '%s\n' "$want_filters") <(printf '%s\n' "$filter_names") | tr '\n' ' ')
# The buffer sources and sinks (ff_vsrc_buffer, ff_asink_abuffer, ...) are
# always built in.
[ "$extra" = "abuffer buffer " ] || fail "filters beyond Grape's: $extra"
echo "  protocols: file, pipe. Demuxers, muxers, encoders and filters: exactly Grape's."
echo "  $decoder_count decoders, $parser_count parsers, $bsf_count bitstream filters, input device lavfi."

# The programs are checked where FFmpeg built them, and only programs that
# pass are copied to dist/. An older run's are removed first, so a check
# that fails leaves no programs in dist/<target>/.
out="$dist_root/$target"
mkdir -p "$out"
rm -f "$out/ffmpeg$exe" "$out/ffprobe$exe" "$out/BUILDINFO.txt"

# Every file the build compiled, from its dependency records, and the
# headers those include: each BSD, MIT, ISC, zlib or IJG notice in them
# must be in ffmpeg-notices.txt, so that a new version or component can't
# bring one in unnoticed. logs/notices-found.txt has what this build found.
if ! notices=$(python3 -I "$script_dir/notices.py" check "$target" "$tdir" "$sources_json" "$licenses_dir"); then
  printf '%s\n' "$notices"
  fail "licenses/ffmpeg/ffmpeg-notices.txt lacks what's above. See scripts/ffmpeg/README.md, 'License notices'."
fi
printf '%s\n' "$notices"

case "$target" in
  windows-x86_64)
    # Every DLL the exe imports must ship with Windows.
    for prog in ffmpeg ffprobe; do
      dlls=$("$cross-objdump" -p "$ffdir/$prog.exe" | sed -n 's/^[[:space:]]*DLL Name: //p' | sort -u)
      [ -n "$dlls" ] || fail "$prog.exe imports no DLLs at all, which can't be right."
      while IFS= read -r dll; do
        lower=$(printf '%s' "$dll" | tr '[:upper:]' '[:lower:]')
        case "$lower" in
          kernel32.dll | user32.dll | advapi32.dll | bcrypt.dll | shell32.dll | ole32.dll | \
            psapi.dll | msvcrt.dll | ucrtbase.dll | api-ms-win-crt-*.dll | api-ms-win-core-*.dll) ;;
          *) fail "$prog.exe needs $dll, which isn't part of Windows." ;;
        esac
      done <<<"$dlls"
      echo "  $prog.exe imports: $(printf '%s' "$dlls" | tr '\n' ' ')"
    done
    ;;
  *)
    # Run what was built.
    version=$("$ffdir/ffmpeg" -hide_banner -version)
    first=$(printf '%s\n' "$version" | sed -n 1p)
    case "$first" in
      "ffmpeg version $(source_field ffmpeg 2)-grape "*) ;;
      *) fail "ffmpeg -version says: $first" ;;
    esac
    [ "$(printf '%s\n' "$version" | sed -n 's/^configuration: //p')" = "$configuration" ] ||
      fail "ffmpeg -version prints another configure line than config.h has."
    listed=$("$ffdir/ffmpeg" -hide_banner -protocols | sed -n '/^Input:/,$p' | grep -v ':$' | sort -u | tr -d ' ' | tr '\n' ' ')
    [ "$listed" = "file pipe " ] || fail "ffmpeg -protocols lists: $listed"
    "$ffdir/ffprobe" -hide_banner -version >/dev/null || fail "ffprobe doesn't run."
    echo "  $first"
    echo "  ffmpeg -protocols (input and output): $listed"
    ;;
esac
cp "$ffdir/ffmpeg$exe" "$ffdir/ffprobe$exe" "$out/"
chmod 755 "$out/ffmpeg$exe" "$out/ffprobe$exe"
step_done

# --- BUILDINFO.txt ---------------------------------------------------------

first_line() { # command...; its first output line, or "?"
  local line
  line=$("$@" 2>&1 | sed -n 1p) || true
  printf '%s\n' "${line:-?}"
}

{
  echo "Grape's FFmpeg build for $target (DECISIONS D24)"
  echo
  echo "FFmpeg $(source_field ffmpeg 2), built by scripts/ffmpeg/build.sh $target"
  if git -C "$repo" rev-parse HEAD >/dev/null 2>&1; then
    dirty=""
    [ -z "$(git -C "$repo" status --porcelain -- scripts/ffmpeg licenses/ffmpeg 2>/dev/null)" ] ||
      dirty=" (scripts/ffmpeg or licenses/ffmpeg had uncommitted changes)"
    echo "Grape commit: $(git -C "$repo" rev-parse HEAD)$dirty"
  fi
  echo "build.sh SHA-256:     $(sha256 "$script_dir/build.sh")"
  echo "sources.json SHA-256: $(sha256 "$sources_json")"
  echo
  echo "configuration: $configuration"
  echo
  echo "Sources (scripts/ffmpeg/sources.json), each checked before unpacking:"
  while IFS=$'\t' read -r name pin url _ size sha _; do
    printf '  %-10s %-42s %s\n' "$name" "$pin" "$sha"
    printf '  %-10s %s (%s bytes)\n' "" "$url" "$size"
  done <<<"$plan"
  echo
  echo "Toolchain:"
  echo "  host: $(uname -srm)"
  echo "  cc: $(first_line "$cc" --version)"
  case "$target" in
    windows-x86_64)
      echo "  ld: $(first_line "$cross-ld" -v)"
      mingw=$(printf '#include <_mingw.h>\n' | "$cc" -E -dM - 2>/dev/null |
        awk '$2 == "__MINGW64_VERSION_MAJOR" { a = $3 } $2 == "__MINGW64_VERSION_MINOR" { b = $3 }
          $2 == "__MINGW64_VERSION_BUGFIX" { c = $3 } END { if (a != "") print a "." b "." c }')
      echo "  mingw-w64 runtime: ${mingw:-?}, thread model: win32"
      # Debian and Ubuntu name their GCC "13-win32"; the package says which.
      if command -v dpkg-query >/dev/null 2>&1; then
        # shellcheck disable=SC2016 # dpkg-query's own ${...} fields
        echo "  packages: $(dpkg-query -W -f '${Package} ${Version}, ' gcc-mingw-w64-x86-64-win32 \
          mingw-w64-x86-64-dev binutils-mingw-w64-x86-64 2>/dev/null | sed 's/, $//')"
      fi
      ;;
    linux-*)
      echo "  ld: $(first_line ld -v)"
      echo "  glibc: $(first_line ldd --version)"
      ;;
    macos-*)
      echo "  ld: $(first_line ld -v)"
      echo "  macOS $(sw_vers -productVersion), SDK $(xcrun --show-sdk-version), $(first_line xcodebuild -version)"
      echo "  deployment target: $macos_min"
      ;;
  esac
  case "$target" in
    *x86_64) echo "  nasm: $(first_line nasm -v)" ;;
  esac
  echo "  meson: $(first_line meson --version), ninja: $(first_line ninja --version), pkg-config: $(first_line pkg-config --version)"
  echo "  make: $(first_line make --version)"
  echo
  echo "License notices (scripts/ffmpeg/notices.py against licenses/ffmpeg/ffmpeg-notices.txt):"
  printf '%s\n' "$notices"
  echo
  echo "Outputs:"
  for f in "ffmpeg$exe" "ffprobe$exe"; do
    printf '  %s  %s  (%s bytes)\n' "$(sha256 "$out/$f")" "$f" "$(file_size "$out/$f")"
  done
} >"$out/BUILDINFO.txt"

# --- The source bundle -----------------------------------------------------

note "Writing dist/grape-ffmpeg-sources.tar"
epoch=0
if git -C "$repo" rev-parse HEAD >/dev/null 2>&1; then
  epoch=$(git -C "$repo" log -1 --format=%ct)
fi
bundle_tmp="$dist_root/.grape-ffmpeg-sources.tar.tmp"
python3 -I - "$bundle_tmp" "$epoch" "$downloads" "$script_dir" "$licenses_dir" "$plan" <<'PY'
import io
import os
import sys
import tarfile

out, epoch, downloads, scripts, licenses, plan = sys.argv[1:7]
epoch = int(epoch)
top = "grape-ffmpeg-sources"

README = """Grape's FFmpeg: the corresponding source

This archive is the complete source of the ffmpeg and ffprobe programs that
Grape ships (DECISIONS D24 in Grape's repository):

  tarballs/          every source tarball, exactly as downloaded and checked
                     against scripts/ffmpeg/sources.json
  scripts/ffmpeg/    the script that builds them, and the pins
  licenses/ffmpeg/   the license texts, and SOURCE.md with the configure line

To build it again, on the OS and with the tools scripts/ffmpeg/README.md
names, run this in this folder (it downloads nothing):

  GRAPE_FFMPEG_TARBALLS=$PWD/tarballs scripts/ffmpeg/build.sh <target>

with <target> one of linux-x86_64, windows-x86_64, macos-arm64, macos-x86_64.
The programs land in dist/<target>/.
"""


def add(tar, name, data=None, path=None, mode=0o644):
    info = tarfile.TarInfo(top + "/" + name)
    info.mtime = epoch
    info.uid = info.gid = 0
    info.uname = info.gname = ""
    info.mode = mode
    if path is not None:
        with open(path, "rb") as f:
            data = f.read()
    info.size = len(data)
    tar.addfile(info, io.BytesIO(data))


def files_under(root):
    found = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for fn in sorted(filenames):
            full = os.path.join(dirpath, fn)
            if os.path.islink(full) or not os.path.isfile(full):
                sys.exit("build.sh: %s isn't a plain file" % full)
            found.append((os.path.relpath(full, root).replace(os.sep, "/"), full))
    return sorted(found)


with tarfile.open(out, "w", format=tarfile.PAX_FORMAT) as tar:
    add(tar, "README.txt", README.encode("utf-8"))
    for line in plan.splitlines():
        file = line.split("\t")[3]
        add(tar, "tarballs/" + file, path=os.path.join(downloads, file))
    for rel, full in files_under(scripts):
        mode = 0o755 if os.access(full, os.X_OK) else 0o644
        add(tar, "scripts/ffmpeg/" + rel, path=full, mode=mode)
    for rel, full in files_under(licenses):
        add(tar, "licenses/ffmpeg/" + rel, path=full)
PY
mv -f "$bundle_tmp" "$dist_root/grape-ffmpeg-sources.tar"

echo
echo "Built $target in $((SECONDS - started)) s:"
for f in "ffmpeg$exe" "ffprobe$exe" BUILDINFO.txt; do
  printf '  %-60s %12s bytes\n' "$out/$f" "$(file_size "$out/$f")"
done
printf '  %-60s %12s bytes\n' "$dist_root/grape-ffmpeg-sources.tar" "$(file_size "$dist_root/grape-ffmpeg-sources.tar")"
