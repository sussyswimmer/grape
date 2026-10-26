#!/usr/bin/env bash
# Fetches the sidecars pinned in scripts/sidecars.lock for macOS builds and
# puts them in apps/desktop/src-tauri/binaries/ with Tauri's target-triple
# names (DECISIONS D25; the pin itself is D22).
#
#   ./scripts/fetch-sidecars.sh            fetch, unless binaries/ is current
#   ./scripts/fetch-sidecars.sh --force    fetch again anyway
#
# For each sidecar it downloads one zip per Apple triple into a new temp
# folder, checks the zip's size and SHA-256 before extracting anything, and
# extracts only the file the lock names. Then it checks each file's
# architecture, merges the two with lipo into <name>-universal-apple-darwin
# (lipo breaks the build's own signature, so the result is signed ad hoc),
# and runs it with -hide_banner -version: the first line must name the lock's
# version, and the configuration line must not have --enable-nonfree.
# binaries/ gets the per-arch files too, because tauri-build looks for those
# while it builds each arch of a universal app. The stamp file
# binaries/sidecars.stamp is written last: the lock's hash, the target, and
# each file's SHA-256. A run that finds them all matching changes nothing.
#
# It needs curl, shasum, unzip, lipo, codesign, arch and python3, which reads
# the lock. lipo and python3 come with the Xcode Command Line Tools, which Rust
# needs too. It never changes PATH and installs nothing.
#
# GRAPE_SIDECARS_NO_RUN=1 is for testing the download on another OS only. It
# skips lipo, codesign and the -version runs, places just the per-arch
# files, and writes no stamp, so the next normal run checks everything again.
#
# Works with macOS's bash 3.2.

set -euo pipefail

me="fetch-sidecars"
universal="universal-apple-darwin"

fail() {
  echo "$me: $*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: ./scripts/fetch-sidecars.sh [--force]

Fetches the FFmpeg builds pinned in scripts/sidecars.lock into
apps/desktop/src-tauri/binaries/ and checks them. Does nothing when the files
there already match the lock. --force fetches them again anyway.
EOF
}

force=0
for arg in "$@"; do
  case "$arg" in
    --force | -f) force=1 ;;
    --help | -h)
      usage
      exit 0
      ;;
    *) fail "unknown option '$arg'. Try --help." ;;
  esac
done

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
# tab-separated lines:
#   A <archive> <url> <sha256> <size>
#   S <sidecar> <version>
#   F <sidecar> <triple> <archive> <path in archive>
#   L <sidecar> <triple> <archive> <path in archive>   (a license text)
# The same rules as in fetch-sidecars.ps1. (A function, because bash 3.2
# misreads a here-document inside $(...).)
read_lock() {
  python3 -I - "$lock" <<'PY'
import json
import re
import sys

APPLE = ("aarch64-apple-darwin", "x86_64-apple-darwin")


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
SHA256 = re.compile(r"^[0-9a-f]{64}$")
URL = re.compile(r"^https://[^\s]+$")


def check_path(path, where):
    # A plain relative path: no wildcards (unzip reads them as patterns),
    # no backslashes, nothing absolute and no . or .. segments.
    if not isinstance(path, str) or not path:
        die("%s must be a non-empty string." % where)
    if path.startswith("/") or "\\" in path or any(c in path for c in "*?[]"):
        die("%s must be a plain relative path with / separators: '%s'." % (where, path))
    if any(ord(c) < 32 for c in path):
        die("%s has a control character." % where)
    if any(part in ("", ".", "..") for part in path.split("/")):
        die("%s has an empty, '.' or '..' part: '%s'." % (where, path))


try:
    with open(sys.argv[1], encoding="utf-8") as f:
        lock = json.load(f, object_pairs_hook=no_duplicates)
except (OSError, ValueError) as e:
    die("can't read it as JSON (%s)." % e)

check_keys(lock, "the file", ("format", "archives", "sidecars"))
if type(lock["format"]) is not int or lock["format"] != 1:
    die("'format' must be 1. This script reads format 1 only.")

archives = {}
for aid, a in entries(lock["archives"], "archives"):
    where = "archives.%s" % aid
    if not NAME.match(aid):
        die("archive ids are lower-case letters, digits and dashes: '%s'." % aid)
    check_keys(a, where, ("url", "sha256", "size"))
    if not isinstance(a["url"], str) or not URL.match(a["url"]):
        die("%s.url must be an https:// URL." % where)
    if not isinstance(a["sha256"], str) or not SHA256.match(a["sha256"]):
        die("%s.sha256 must be 64 lower-case hex digits." % where)
    if type(a["size"]) is not int or a["size"] <= 0:
        die("%s.size must be the archive's size in bytes." % where)
    archives[aid] = a

out_a, out_s, out_f = [], [], []
used = []
for name, s in entries(lock["sidecars"], "sidecars"):
    where = "sidecars.%s" % name
    if not NAME.match(name):
        die("sidecar names are lower-case letters, digits and dashes: '%s'." % name)
    check_keys(s, where, ("version", "targets"))
    if not isinstance(s["version"], str) or not VERSION.match(s["version"]):
        die("%s.version must look like 9.0.2." % where)
    targets = dict(entries(s["targets"], where + ".targets"))
    for triple, t in targets.items():
        twhere = "%s.targets.%s" % (where, triple)
        if not TRIPLE.match(triple) or triple.startswith("universal-"):
            die("%s: '%s' isn't a target triple Grape fetches." % (where, triple))
        check_keys(t, twhere, ("archive", "path"), ("license",))
        if t["archive"] not in archives:
            die("%s.archive names '%s', which isn't under archives." % (twhere, t["archive"]))
        check_path(t["path"], twhere + ".path")
        if "license" in t:
            check_path(t["license"], twhere + ".license")
    apple = [triple for triple in APPLE if triple in targets]
    if not apple:
        continue
    if len(apple) != len(APPLE):
        die("%s needs both %s for the universal build." % (where, " and ".join(APPLE)))
    out_s.append("S\t%s\t%s" % (name, s["version"]))
    for triple in APPLE:
        t = targets[triple]
        out_f.append("F\t%s\t%s\t%s\t%s" % (name, triple, t["archive"], t["path"]))
        if "license" in t:
            out_f.append("L\t%s\t%s\t%s\t%s" % (name, triple, t["archive"], t["license"]))
        if t["archive"] not in used:
            used.append(t["archive"])

if not out_s:
    die("no sidecar has macOS targets.")
for aid in used:
    a = archives[aid]
    out_a.append("A\t%s\t%s\t%s\t%d" % (aid, a["url"], a["sha256"], a["size"]))
print("\n".join(out_a + out_s + out_f))
PY
}
plan=$(read_lock) || fail "the lock has a problem (above)."

# The files a full run leaves in binaries/, one per line.
expected_files() {
  while IFS=$'\t' read -r kind name rest; do
    case "$kind" in
      S) printf '%s\n' "$name-aarch64-apple-darwin" "$name-x86_64-apple-darwin" "$name-$universal" ;;
      L) printf '%s\n' "$name-LICENSE.txt" ;;
    esac
  done <<<"$plan" | sort -u
}

lock_hash=$(sha256 "$lock")

# True when the stamp was written from this lock for this target, and every
# file it should list is there with the SHA-256 it recorded.
stamp_is_current() {
  [ -f "$stamp" ] || return 1
  grep -qxF "lock $lock_hash" "$stamp" || return 1
  grep -qxF "target $universal" "$stamp" || return 1
  local file want
  for file in $(expected_files); do
    [ -f "$bin_dir/$file" ] || return 1
    want=$(awk -v f="$file" 'length($1) == 64 && $2 == f { print $1 }' "$stamp")
    [ -n "$want" ] || return 1
    [ "$(sha256 "$bin_dir/$file")" = "$want" ] || return 1
  done
  return 0
}

if [ "$force" -eq 0 ] && stamp_is_current; then
  echo "Sidecars are already current in apps/desktop/src-tauri/binaries (they match scripts/sidecars.lock). Pass --force to fetch them again."
  exit 0
fi

tmp_root="${TMPDIR:-/tmp}"
tmp=$(mktemp -d "${tmp_root%/}/grape-sidecars.XXXXXX") || fail "couldn't make a temp folder."
trap 'rm -rf "$tmp"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir "$tmp/zip" "$tmp/x" "$tmp/out"

# Download each archive and check it before anything reads it.
while IFS=$'\t' read -r kind id url want_sha want_size; do
  [ "$kind" = "A" ] || continue
  zip="$tmp/zip/$id.zip"
  echo "Downloading $id ($want_size bytes) from $url"
  if ! curl --fail --location --proto '=https' --proto-redir '=https' --tlsv1.2 \
    --silent --show-error --retry 3 --connect-timeout 30 \
    --output "$zip" "$url"; then
    rm -f "$zip"
    fail "couldn't download $id from $url."
  fi
  size=$(wc -c <"$zip" | tr -d ' ')
  if [ "$size" != "$want_size" ]; then
    rm -f "$zip"
    fail "$id is $size bytes, but the lock says $want_size. Deleted it; nothing was extracted or changed."
  fi
  got_sha=$(sha256 "$zip")
  if [ "$got_sha" != "$want_sha" ]; then
    rm -f "$zip"
    fail "$id has SHA-256 $got_sha, but the lock says $want_sha. Deleted it; nothing was extracted or changed."
  fi
  echo "  SHA-256 matches the lock."
done <<<"$plan"

# Extract only the paths the lock names.
extract() { # archive, path in archive; prints where it landed
  local id=$1 path=$2 dir="$tmp/x/$1"
  mkdir -p "$dir"
  unzip -q -o "$tmp/zip/$id.zip" "$path" -d "$dir" >&2 || fail "$id has no '$path' in it."
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
    archs=" $(lipo -archs "$fat") "
    case "$archs" in *" arm64 "*) ;; *) fail "$name-$universal has no arm64 half." ;; esac
    case "$archs" in *" x86_64 "*) ;; *) fail "$name-$universal has no x86_64 half." ;; esac
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
fi

# Everything checked: replace the files in binaries/, then write the stamp.
mkdir -p "$bin_dir"
rm -f "$stamp"
for file in "$tmp/out"/*; do
  mv -f "$file" "$bin_dir/"
done

if [ "$no_run" -eq 1 ]; then
  echo "Placed the per-arch files in apps/desktop/src-tauri/binaries and wrote no stamp, so the next run without GRAPE_SIDECARS_NO_RUN fetches and checks everything again."
  exit 0
fi

{
  echo "# Written by scripts/fetch-sidecars.sh. Delete this file or pass --force to fetch again."
  echo "lock $lock_hash"
  echo "target $universal"
  for file in $(expected_files); do
    [ -f "$bin_dir/$file" ] || fail "$file is missing from binaries/ after the move."
    echo "$(sha256 "$bin_dir/$file")  $file"
  done
} >"$tmp/stamp" || fail "couldn't write the stamp."
mv -f "$tmp/stamp" "$stamp"

echo "Sidecars are ready in apps/desktop/src-tauri/binaries:"
for file in $(expected_files); do
  printf '  %-40s %12s bytes\n' "$file" "$(wc -c <"$bin_dir/$file" | tr -d ' ')"
done
