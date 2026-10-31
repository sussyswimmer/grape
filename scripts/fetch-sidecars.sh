#!/usr/bin/env bash
# Fetches the sidecars and libraries pinned in scripts/sidecars.lock for macOS
# builds and puts them in apps/desktop/src-tauri/binaries/: FFmpeg and ffprobe
# with Tauri's target-triple names (DECISIONS D25; the pin itself is D22), and
# PDFium (D29).
#
#   ./scripts/fetch-sidecars.sh               fetch, unless binaries/ is current
#   ./scripts/fetch-sidecars.sh --force       fetch again anyway
#   ./scripts/fetch-sidecars.sh --only pdfium just that one sidecar or library
#
# It downloads each archive into a new temp folder, checks its size and
# SHA-256 before extracting anything, and extracts only the files the lock
# names (a zip with unzip, a .tgz with /usr/bin/tar).
#
# Sidecars come as one zip per Apple triple. It checks each file's
# architecture, merges the two with lipo into <name>-universal-apple-darwin
# (lipo breaks the build's own signature, so the result is signed ad hoc), and
# runs it with -hide_banner -version: the first line must name the lock's
# version, and the configuration line must not have --enable-nonfree.
# binaries/ gets the per-arch files too, because tauri-build looks for those
# while it builds each arch of a universal app.
#
# A library comes already universal. It can't be run, so the file taken from
# the archive must have the SHA-256 the lock gives it. It must hold arm64 and
# x86_64, is signed ad hoc again (the build signs only its arm64 half), and
# lands as <stem>-universal-apple-darwin<extension>, plus a copy under its own
# name in binaries/macos/ for bundle.macOS.frameworks, which keeps a file's
# name: libpdfium-universal-apple-darwin.dylib and macos/libpdfium.dylib.
#
# The stamp file binaries/sidecars.stamp is written last: the lock's hash, the
# target, and each file's SHA-256. A run that finds them all matching changes
# nothing. With --only, the other files and their lines in the stamp stay.
#
# It needs curl, shasum, unzip, /usr/bin/tar, lipo, codesign, arch and
# python3, which reads the lock. lipo and python3 come with the Xcode Command
# Line Tools, which Rust needs too. It never changes PATH and installs nothing.
#
# GRAPE_SIDECARS_NO_RUN=1 is for testing the download on another OS only. It
# skips lipo, codesign and the -version runs, places just the per-arch files
# and the unsigned libraries, and writes no stamp, so the next normal run
# checks everything again.
#
# Works with macOS's bash 3.2.

set -euo pipefail

me="fetch-sidecars"
universal="universal-apple-darwin"
tar_bin="/usr/bin/tar"

fail() {
  echo "$me: $*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: ./scripts/fetch-sidecars.sh [--force] [--only <name>]

Fetches the builds pinned in scripts/sidecars.lock (FFmpeg, ffprobe, PDFium)
into apps/desktop/src-tauri/binaries/ and checks them. Does nothing when the
files there already match the lock. --force fetches them again anyway.
--only <name> fetches just that sidecar or library, such as pdfium.
EOF
}

force=0
only=""
only_given=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --force | -f) force=1 ;;
    --only)
      [ "$#" -ge 2 ] || fail "--only needs the name of a sidecar or library, such as pdfium."
      only=$2
      only_given=1
      shift
      ;;
    --only=*)
      only=${1#--only=}
      only_given=1
      ;;
    --help | -h)
      usage
      exit 0
      ;;
    *) fail "unknown option '$1'. Try --help." ;;
  esac
  shift
done
if [ "$only_given" -eq 1 ] && [ -z "$only" ]; then
  fail "--only needs the name of a sidecar or library, such as pdfium."
fi

no_run=0
if [ "${GRAPE_SIDECARS_NO_RUN:-}" = "1" ]; then
  no_run=1
fi

if [ "$(uname -s)" != "Darwin" ] && [ "$no_run" -ne 1 ]; then
  fail "this script is for macOS. On Windows, run .\\scripts\\fetch-sidecars.ps1."
fi

for tool in curl shasum unzip python3; do
  command -v "$tool" >/dev/null 2>&1 || fail "$tool is missing. Install the Xcode Command Line Tools: xcode-select --install"
done
[ -x "$tar_bin" ] || fail "$tar_bin is missing."
if [ "$no_run" -ne 1 ]; then
  for tool in lipo codesign arch; do
    command -v "$tool" >/dev/null 2>&1 || fail "$tool is missing. Install the Xcode Command Line Tools: xcode-select --install"
  done
fi
# Without the Command Line Tools, /usr/bin/python3 is a stub that fails.
python3 -I -c 'import json' >/dev/null 2>&1 || fail "python3 doesn't run. Install the Xcode Command Line Tools: xcode-select --install"

script_dir=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$script_dir/.." && pwd)
lock="$script_dir/sidecars.lock"
bin_dir="$repo/apps/desktop/src-tauri/binaries"
stamp="$bin_dir/sidecars.stamp"

[ -f "$lock" ] || fail "can't find scripts/sidecars.lock."

sha256() {
  # Read from stdin, so no file name can change shasum's output format.
  shasum -a 256 <"$1" | awk '{ print $1 }'
}

# Reads and checks the lock, and prints what macOS needs from it as
# tab-separated lines (with --only, just that sidecar's or library's):
#   A <archive> <url> <sha256> <size> <zip|tgz>
#   S <sidecar> <version>
#   F <sidecar> <triple> <archive> <path in archive>
#   L <sidecar> <triple> <archive> <path in archive>   (a license text)
#   D <library> <version> <archive> <path in archive> <sha256 of that file>
#     <file in binaries/> <copy in binaries/>
# The same rules as in fetch-sidecars.ps1. (A function, because bash 3.2
# misreads a here-document inside $(...).)
read_lock() {
  python3 -I - "$lock" "$only" <<'PY'
import json
import re
import sys

APPLE = ("aarch64-apple-darwin", "x86_64-apple-darwin")
UNIVERSAL = "universal-apple-darwin"


def die(msg):
    sys.stderr.write("fetch-sidecars: scripts/sidecars.lock: " + msg + "\n")
    sys.exit(1)


def no_duplicates(pairs):
    seen = {}
    for key, value in pairs:
        if key in seen:
            die("the key '%s' appears twice in one object." % key)
        seen[key] = value
    return seen


def check_keys(obj, where, required, optional=()):
    if not isinstance(obj, dict):
        die("%s must be an object." % where)
    for key in obj:
        if not key.startswith("_") and key not in required and key not in optional:
            die("%s has an unknown key '%s'." % (where, key))
    for key in required:
        if key not in obj:
            die("%s is missing '%s'." % (where, key))


def entries(obj, where):
    if not isinstance(obj, dict):
        die("%s must be an object." % where)
    return [(k, v) for k, v in obj.items() if not k.startswith("_")]


NAME = re.compile(r"^[a-z0-9][a-z0-9-]*$")
TRIPLE = re.compile(r"^[a-z0-9_]+(-[a-z0-9_]+){2,3}$")
VERSION = re.compile(r"^[0-9]+(\.[0-9]+)*$")
LIB_VERSION = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._/-]*$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
URL = re.compile(r"^https://[^\s]+$")


def check_path(path, where):
    # A plain relative path: no wildcards (unzip and tar read them as
    # patterns), no backslashes, nothing absolute, no leading - (they'd read
    # it as an option) and no . or .. segments.
    if not isinstance(path, str) or not path:
        die("%s must be a non-empty string." % where)
    if (path.startswith("/") or path.startswith("-") or "\\" in path
            or any(c in path for c in "*?[]")):
        die("%s must be a plain relative path with / separators: '%s'." % (where, path))
    if any(ord(c) < 32 for c in path):
        die("%s has a control character." % where)
    if any(part in ("", ".", "..") for part in path.split("/")):
        die("%s has an empty, '.' or '..' part: '%s'." % (where, path))


def check_sha256(value, where):
    if not isinstance(value, str) or not SHA256.match(value):
        die("%s must be 64 lower-case hex digits." % where)


def check_archive(t, twhere, archives):
    if not isinstance(t["archive"], str) or t["archive"] not in archives:
        die("%s.archive names '%s', which isn't under archives." % (twhere, t["archive"]))


def library_file(path, target):
    # Its file name with the target before the extension:
    # lib/libpdfium.dylib -> libpdfium-universal-apple-darwin.dylib.
    name = path.rsplit("/", 1)[-1]
    dot = name.rfind(".")
    if dot > 0:
        return name[:dot] + "-" + target + name[dot:]
    return name + "-" + target


try:
    with open(sys.argv[1], encoding="utf-8") as f:
        lock = json.load(f, object_pairs_hook=no_duplicates)
except (OSError, ValueError) as e:
    die("can't read it as JSON (%s)." % e)
only = sys.argv[2] if len(sys.argv) > 2 and sys.argv[2] else None

check_keys(lock, "the file", ("format", "archives", "sidecars", "libraries"))
if type(lock["format"]) is not int or lock["format"] != 2:
    die("'format' must be 2. This script reads format 2 only.")

archives = {}
for aid, a in entries(lock["archives"], "archives"):
    where = "archives.%s" % aid
    if not NAME.match(aid):
        die("archive ids are lower-case letters, digits and dashes: '%s'." % aid)
    check_keys(a, where, ("url", "sha256", "size"))
    if not isinstance(a["url"], str) or not URL.match(a["url"]):
        die("%s.url must be an https:// URL." % where)
    if a["url"].endswith(".zip"):
        kind = "zip"
    elif a["url"].endswith(".tgz") or a["url"].endswith(".tar.gz"):
        kind = "tgz"
    else:
        die("%s.url must end in .zip, .tgz or .tar.gz, so the script knows how to extract it." % where)
    check_sha256(a["sha256"], where + ".sha256")
    if type(a["size"]) is not int or a["size"] <= 0:
        die("%s.size must be the archive's size in bytes." % where)
    archives[aid] = dict(a, kind=kind)

names = []
out_s, out_f, out_d = [], [], []
for name, s in entries(lock["sidecars"], "sidecars"):
    where = "sidecars.%s" % name
    if not NAME.match(name):
        die("sidecar names are lower-case letters, digits and dashes: '%s'." % name)
    names.append(name)
    check_keys(s, where, ("version", "targets"))
    if not isinstance(s["version"], str) or not VERSION.match(s["version"]):
        die("%s.version must look like 9.0.2." % where)
    targets = dict(entries(s["targets"], where + ".targets"))
    for triple, t in targets.items():
        twhere = "%s.targets.%s" % (where, triple)
        if not TRIPLE.match(triple) or triple.startswith("universal-"):
            die("%s: '%s' isn't a target triple Grape fetches." % (where, triple))
        check_keys(t, twhere, ("archive", "path"), ("license",))
        check_archive(t, twhere, archives)
        check_path(t["path"], twhere + ".path")
        if "license" in t:
            check_path(t["license"], twhere + ".license")
    apple = [triple for triple in APPLE if triple in targets]
    if not apple:
        continue
    if len(apple) != len(APPLE):
        die("%s needs both %s for the universal build." % (where, " and ".join(APPLE)))
    out_s.append((name, "S\t%s\t%s" % (name, s["version"])))
    for triple in APPLE:
        t = targets[triple]
        out_f.append((name, t["archive"], "F\t%s\t%s\t%s\t%s" % (name, triple, t["archive"], t["path"])))
        if "license" in t:
            out_f.append((name, t["archive"], "L\t%s\t%s\t%s\t%s" % (name, triple, t["archive"], t["license"])))

for name, lib in entries(lock["libraries"], "libraries"):
    where = "libraries.%s" % name
    if not NAME.match(name):
        die("library names are lower-case letters, digits and dashes: '%s'." % name)
    if name in names:
        die("'%s' is both a sidecar and a library." % name)
    names.append(name)
    check_keys(lib, where, ("version", "targets"))
    if not isinstance(lib["version"], str) or not LIB_VERSION.match(lib["version"]):
        die("%s.version must look like chromium/8086." % where)
    targets = dict(entries(lib["targets"], where + ".targets"))
    for triple, t in targets.items():
        twhere = "%s.targets.%s" % (where, triple)
        is_universal = triple == UNIVERSAL
        if not TRIPLE.match(triple) or (triple.startswith("universal-") and not is_universal):
            die("%s: '%s' isn't a target Grape fetches." % (where, triple))
        if triple.endswith("-apple-darwin") and not is_universal:
            die("%s: give the Mac library as %s, not '%s'. The scripts don't merge libraries with lipo." % (where, UNIVERSAL, triple))
        check_keys(t, twhere, ("archive", "path", "sha256"))
        check_archive(t, twhere, archives)
        check_path(t["path"], twhere + ".path")
        check_sha256(t["sha256"], twhere + ".sha256")
    if UNIVERSAL not in targets:
        continue
    t = targets[UNIVERSAL]
    copy = "macos/" + t["path"].rsplit("/", 1)[-1]
    out_d.append((name, t["archive"], "D\t%s\t%s\t%s\t%s\t%s\t%s\t%s" % (
        name, lib["version"], t["archive"], t["path"], t["sha256"],
        library_file(t["path"], UNIVERSAL), copy)))

if only is not None:
    if only not in names:
        die("it has no sidecar or library named '%s'." % only)
    out_s = [x for x in out_s if x[0] == only]
    out_f = [x for x in out_f if x[0] == only]
    out_d = [x for x in out_d if x[0] == only]
    if not out_s and not out_d:
        die("'%s' has no macOS target." % only)
if not out_s and not out_d:
    die("nothing in it has a macOS target.")

used = []
for x in out_f + out_d:
    if x[1] not in used:
        used.append(x[1])
out_a = []
for aid in used:
    a = archives[aid]
    out_a.append("A\t%s\t%s\t%s\t%d\t%s" % (aid, a["url"], a["sha256"], a["size"], a["kind"]))
print("\n".join(out_a + [x[1] for x in out_s] + [x[2] for x in out_f] + [x[2] for x in out_d]))
PY
}
plan=$(read_lock) || fail "the lock has a problem (above)."

# The files this run leaves in binaries/, one per line.
expected_files() {
  while IFS=$'\t' read -r kind name _ _ _ _ out copy; do
    case "$kind" in
      S) printf '%s\n' "$name-aarch64-apple-darwin" "$name-x86_64-apple-darwin" "$name-$universal" ;;
      L) printf '%s\n' "$name-LICENSE.txt" ;;
      D) printf '%s\n' "$out" "$copy" ;;
    esac
  done <<<"$plan" | sort -u
}
expected=$(expected_files)

lock_hash=$(sha256 "$lock")

# The stamp's "<sha256>  <file>" lines, when it was written from this lock for
# this target. Otherwise nothing.
recorded_lines() {
  [ -f "$stamp" ] || return 0
  grep -qxF "lock $lock_hash" "$stamp" || return 0
  grep -qxF "target $universal" "$stamp" || return 0
  awk 'NF == 2 && length($1) == 64 { print }' "$stamp"
}
recorded=$(recorded_lines)

# True when every file this run should leave is there with the SHA-256 the
# stamp recorded for it.
stamp_is_current() {
  [ -n "$recorded" ] || return 1
  local file want
  for file in $expected; do
    [ -f "$bin_dir/$file" ] || return 1
    want=$(printf '%s\n' "$recorded" | awk -v f="$file" '$2 == f { w = $1 } END { print w }')
    [ -n "$want" ] || return 1
    [ "$(sha256 "$bin_dir/$file")" = "$want" ] || return 1
  done
  return 0
}

if [ "$force" -eq 0 ] && stamp_is_current; then
  if [ -n "$only" ]; then
    echo "$only is already current in apps/desktop/src-tauri/binaries (it matches scripts/sidecars.lock). Pass --force to fetch it again."
  else
    echo "Sidecars are already current in apps/desktop/src-tauri/binaries (they match scripts/sidecars.lock). Pass --force to fetch them again."
  fi
  exit 0
fi

tmp_root="${TMPDIR:-/tmp}"
tmp=$(mktemp -d "${tmp_root%/}/grape-sidecars.XXXXXX") || fail "couldn't make a temp folder."
trap 'rm -rf "$tmp"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir "$tmp/dl" "$tmp/x" "$tmp/out"

# Download each archive and check it before anything reads it.
while IFS=$'\t' read -r kind id url want_sha want_size type; do
  [ "$kind" = "A" ] || continue
  file="$tmp/dl/$id.$type"
  echo "Downloading $id ($want_size bytes) from $url"
  if ! curl --fail --location --proto '=https' --proto-redir '=https' --tlsv1.2 \
    --silent --show-error --retry 3 --retry-all-errors --connect-timeout 30 \
    --speed-limit 1024 --speed-time 60 \
    --output "$file" "$url"; then
    rm -f "$file"
    fail "couldn't download $id from $url."
  fi
  size=$(wc -c <"$file" | tr -d ' ')
  if [ "$size" != "$want_size" ]; then
    rm -f "$file"
    fail "$id is $size bytes, but the lock says $want_size. Deleted it; nothing was extracted or changed."
  fi
  got_sha=$(sha256 "$file")
  if [ "$got_sha" != "$want_sha" ]; then
    rm -f "$file"
    fail "$id has SHA-256 $got_sha, but the lock says $want_sha. Deleted it; nothing was extracted or changed."
  fi
  echo "  SHA-256 matches the lock."
done <<<"$plan"

archive_type() { # archive id; prints zip or tgz
  printf '%s\n' "$plan" | awk -F '\t' -v id="$1" '$1 == "A" && $2 == id { print $6 }'
}

# Extract only the paths the lock names.
extract() { # archive, path in archive; prints where it landed
  local id=$1 path=$2 dir="$tmp/x/$1" type
  type=$(archive_type "$id")
  mkdir -p "$dir"
  case "$type" in
    zip)
      unzip -q -o "$tmp/dl/$id.zip" "$path" -d "$dir" >&2 || fail "$id has no '$path' in it."
      ;;
    tgz)
      "$tar_bin" -xzf "$tmp/dl/$id.tgz" -C "$dir" "$path" >&2 || fail "$id has no '$path' in it, or tar couldn't read it."
      ;;
    *) fail "the plan has no archive '$id'." ;;
  esac
  [ -f "$dir/$path" ] && [ ! -L "$dir/$path" ] || fail "'$path' in $id isn't a plain file."
  printf '%s\n' "$dir/$path"
}

while IFS=$'\t' read -r kind name triple id path; do
  case "$kind" in
    F)
      src=$(extract "$id" "$path")
      cp "$src" "$tmp/out/$name-$triple"
      chmod 755 "$tmp/out/$name-$triple"
      echo "Extracted $name for $triple."
      ;;
    L)
      src=$(extract "$id" "$path")
      dest="$tmp/out/$name-LICENSE.txt"
      if [ -f "$dest" ]; then
        cmp -s "$src" "$dest" || fail "the license texts for $name in the two Apple archives differ."
      else
        cp "$src" "$dest"
        chmod 644 "$dest"
      fi
      ;;
  esac
done <<<"$plan"

# A library's file must have the SHA-256 the lock gives it.
while IFS=$'\t' read -r kind name version id path want out copy; do
  [ "$kind" = "D" ] || continue
  src=$(extract "$id" "$path")
  got=$(sha256 "$src")
  [ "$got" = "$want" ] || fail "'$path' in $id has SHA-256 $got, but the lock says $want. Nothing was changed."
  cp "$src" "$tmp/out/$out"
  chmod 644 "$tmp/out/$out"
  echo "Extracted $name for $universal."
  echo "  $name $version: its SHA-256 matches the lock."
done <<<"$plan"

# Runs a sidecar with -hide_banner -version and checks what it says.
# Arguments: sidecar name, version, then the command to run.
check_version() {
  local name=$1 version=$2 output first config
  shift 2
  if ! output=$("$@" -hide_banner -version 2>"$tmp/stderr"); then
    cat "$tmp/stderr" >&2
    fail "$name didn't run with -version."
  fi
  first=$(printf '%s\n' "$output" | sed -n 1p)
  case "$first" in
    "$name version "*) ;;
    *) fail "$name's first line should start with '$name version', but it says: $first" ;;
  esac
  # The version must be a whole token: 9.0.2 is in
  # "n9.0.2-17-g2a571b6068", but not in "19.0.2" or "9.0.21".
  case " $(printf '%s' "$first" | tr -c '0-9.' ' ') " in
    *" $version "*) ;;
    *) fail "$name says '$first', not version $version." ;;
  esac
  config=$(printf '%s\n' "$output" | grep -E '^configuration:' || true)
  [ -n "$config" ] || fail "$name printed no configuration line."
  case "$config" in
    *--enable-nonfree*) fail "$name was built with --enable-nonfree, so it can't be shipped." ;;
  esac
  echo "  $first"
}

check_arch() { # file, the one architecture it must have
  local archs
  archs=$(lipo -archs "$1") || fail "lipo can't read $(basename "$1")."
  [ "$archs" = "$2" ] || fail "$(basename "$1") is built for '$archs', not $2."
}

check_universal() { # file; it must hold both arm64 and x86_64
  local archs
  archs=$(lipo -archs "$1") || fail "lipo can't read $(basename "$1")."
  case " $archs " in *" arm64 "*) ;; *) fail "$(basename "$1") has no arm64 half." ;; esac
  case " $archs " in *" x86_64 "*) ;; *) fail "$(basename "$1") has no x86_64 half." ;; esac
}

if [ "$no_run" -eq 1 ]; then
  echo "GRAPE_SIDECARS_NO_RUN=1: skipped lipo, codesign and the -version checks. That's for testing only."
else
  native=$(uname -m)
  case "$native" in
    arm64) other=x86_64 ;;
    x86_64) other=arm64 ;;
    *) fail "unknown Mac architecture '$native'." ;;
  esac
  # Rosetta (on Apple silicon) lets the Intel half run too. An Intel Mac
  # can't run the arm64 half at all.
  can_run_other=0
  if arch "-$other" /usr/bin/true >/dev/null 2>&1; then
    can_run_other=1
  fi

  while IFS=$'\t' read -r kind name version; do
    [ "$kind" = "S" ] || continue
    arm="$tmp/out/$name-aarch64-apple-darwin"
    intel="$tmp/out/$name-x86_64-apple-darwin"
    fat="$tmp/out/$name-$universal"
    check_arch "$arm" arm64
    check_arch "$intel" x86_64
    lipo -create -output "$fat" "$arm" "$intel" || fail "lipo couldn't merge $name."
    chmod 755 "$fat"
    check_universal "$fat"
    codesign --force --sign - "$fat" || fail "codesign couldn't sign $name-$universal."
    codesign --verify --strict "$fat" || fail "$name-$universal's signature doesn't verify."
    echo "Made $name-$universal (arm64 + x86_64), signed ad hoc."
    check_version "$name" "$version" "$fat"
    if [ "$can_run_other" -eq 1 ]; then
      check_version "$name" "$version" arch "-$other" "$fat"
    else
      echo "  (This Mac can't run the $other half, so only the $native half ran.)"
    fi
  done <<<"$plan"

  # The build signs only a library's arm64 half (linker-signed), so sign the
  # whole file again, as lipo's output is above.
  while IFS=$'\t' read -r kind name version id path want out copy; do
    [ "$kind" = "D" ] || continue
    lib="$tmp/out/$out"
    check_universal "$lib"
    codesign --force --sign - "$lib" || fail "codesign couldn't sign $out."
    codesign --verify --strict "$lib" || fail "$out's signature doesn't verify."
    echo "Signed $out (arm64 + x86_64) ad hoc."
  done <<<"$plan"
fi

# bundle.macOS.frameworks keeps a file's name, so each library is also there
# under the name it had in its archive.
while IFS=$'\t' read -r kind name version id path want out copy; do
  [ "$kind" = "D" ] || continue
  mkdir -p "$(dirname "$tmp/out/$copy")"
  cp "$tmp/out/$out" "$tmp/out/$copy"
done <<<"$plan"

# Everything checked: replace the files in binaries/, then write the stamp.
mkdir -p "$bin_dir"
rm -f "$stamp"
for rel in $(cd "$tmp/out" && find . -type f); do
  rel=${rel#./}
  mkdir -p "$(dirname "$bin_dir/$rel")"
  mv -f "$tmp/out/$rel" "$bin_dir/$rel"
done

if [ "$no_run" -eq 1 ]; then
  echo "Placed the per-arch files and the libraries in apps/desktop/src-tauri/binaries and wrote no stamp, so the next run without GRAPE_SIDECARS_NO_RUN fetches and checks everything again."
  exit 0
fi

for file in $expected; do
  [ -f "$bin_dir/$file" ] || fail "$file is missing from binaries/ after the move."
done
{
  echo "# Written by scripts/fetch-sidecars.sh. Delete this file or pass --force to fetch again."
  echo "lock $lock_hash"
  echo "target $universal"
  {
    # What this run didn't fetch keeps its line while its file is there. The
    # next run checks that file's SHA-256 against it as usual.
    if [ -n "$recorded" ]; then
      printf '%s\n' "$recorded" | while read -r want file; do
        # Not grep -q: it can stop reading early, and pipefail would see
        # printf's SIGPIPE.
        if printf '%s\n' "$expected" | grep -xF -- "$file" >/dev/null; then
          continue
        fi
        if [ -f "$bin_dir/$file" ]; then
          printf '%s  %s\n' "$want" "$file"
        fi
      done
    fi
    for file in $expected; do
      echo "$(sha256 "$bin_dir/$file")  $file"
    done
  } | LC_ALL=C sort -k 2,2
} >"$tmp/stamp" || fail "couldn't write the stamp."
mv -f "$tmp/stamp" "$stamp"

echo "Sidecars are ready in apps/desktop/src-tauri/binaries:"
for file in $expected; do
  printf '  %-40s %12s bytes\n' "$file" "$(wc -c <"$bin_dir/$file" | tr -d ' ')"
done
