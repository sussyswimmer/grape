"""Smoke test for Grape's own bsdtar (DECISIONS D27), in Grape's exact environment.

    python3 -I scripts/bsdtar/smoke.py --bsdtar <file> \
        --tarball <libarchive-3.8.10.tar.gz> --sources scripts/bsdtar/sources.json

(`python -I` on Windows.) Standard library only.

It checks the tarball's size and SHA-256 against sources.json, then reads
libarchive's own RAR test files straight out of it (the `.uu` members under
libarchive/test/, decoded in memory; nothing is extracted). Every case runs
bsdtar exactly as Grape does:

  - argv `-c -P -f - --format pax @-`;
  - the input written into a stdin pipe from Python, never a file handle;
  - the working folder a new, empty temp folder, checked still empty after;
  - the environment only PATH (that folder), TZ=UTC0,
    TAR_READER_OPTIONS=rar:hdrcharset=UTF-8, plus SystemRoot on Windows,
    LC_ALL=en_US.UTF-8 on macOS or LC_ALL=C.UTF-8 on Linux;
  - CREATE_NO_WINDOW on Windows, and a 60 s timeout.

Good RAR files must give exit 0, an empty stderr, a tar padded to 10,240
bytes that tarfile reads, the expected entries (names and link targets
compared after NFC, as Grape does), and each regular file's CRC32 equal to
the RAR header's. Encrypted and multi-volume ones must fail with exit 1.
ZIP, mtree, gzip and empty input must fail with exit 1 and "Unrecognized
archive format" (patch 0006), with no canary bytes in the output.

It prints one PASS/FAIL line per case and never prints bsdtar's stderr,
which can hold entry names. The exit code is the number of failures.
"""

import argparse
import binascii
import hashlib
import io
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import unicodedata
import zipfile
import zlib

ARGS = ["-c", "-P", "-f", "-", "--format", "pax", "@-"]
TIMEOUT_S = 60
CREATE_NO_WINDOW = 0x08000000
PAD = 10240
CANARY = b"SECRET-ON-DISK\n"

# What bsdtar must write for each good vector: (tar type, name, size, link
# target), in archive order. From the prototype's Linux runs of the patched
# build (rar-proto logs/runs, DECISIONS D27). tarfile drops a folder's
# trailing slash.
GOOD = [
    ("test_read_format_rar.rar", [
        ("0", "test.txt", 20, ""),
        ("2", "testlink", 0, "test.txt"),
        ("0", "testdir/test.txt", 20, ""),
        ("5", "testdir", 0, ""),
        ("5", "testemptydir", 0, ""),
    ]),
    ("test_read_format_rar_filter.rar", [
        ("0", "bsdcat.exe", 204288, ""),
    ]),
    ("test_read_format_rar_unicode.rar", [
        ("0", "表だよ/新しいフォルダ/"
              "新規テキスト ドキュメント.txt", 0, ""),
        ("0", "表だよ/漢字長いファイル名"
              "long-filename-in-漢字.txt", 5, ""),
        ("5", "表だよ/新しいフォルダ", 0, ""),
        ("5", "表だよ", 0, ""),
        ("2", "表だよ/ファイル", 0,
              "漢字長いファイル名long-filename-in-漢字.txt"),
        ("0", "abcdefghijklmnopqrsテスト.txt", 16, ""),
    ]),
    ("test_read_format_rar5_compressed.rar", [
        ("0", "test.bin", 1200, ""),
    ]),
    ("test_read_format_rar5_solid.rar", [
        ("0", "test.bin", 1200, ""),
    ] + [("0", "test%d.bin" % i, 4096, "") for i in range(1, 7)]),
    ("test_read_format_rar5_unicode.rar", [
        ("0", "\U0001f44b\U0001f30e.txt", 13, ""),
        ("1", "Ⓗⓐⓡⓓ Ⓛⓘⓝⓚ.txt", 0, "\U0001f44b\U0001f30e.txt"),
        ("2", "\U0001d4ae\U0001d4ce\U0001d4c2\U0001d4b7\U0001d45c\U0001d4c1\U0001d4be\U0001d4b8 "
              "\U0001d43f\U0001d4be\U0001d4c3\U0001d4c0.txt", 0, "\U0001f44b\U0001f30e.txt"),
    ]),
]

# Must fail with exit 1. The pattern, when given, must be in stderr.
BAD = [
    ("test_read_format_rar5_encrypted.rar", "Reading encrypted data"),
    ("test_read_format_rar_encryption_header.rar", "RAR encryption support unavailable"),
    ("test_read_format_rar_multivolume.part0001.rar", None),
]


def fail_usage(msg):
    sys.stderr.write("smoke: %s\n" % msg)
    sys.exit(2)


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while True:
            chunk = f.read(1 << 20)
            if not chunk:
                break
            h.update(chunk)
    return h.hexdigest()


def read_sources(path):
    try:
        with open(path, encoding="utf-8") as f:
            sources = json.load(f)
        la = sources["libarchive"]
        version, sha, size = la["version"], la["sha256"], la["size"]
    except (OSError, ValueError, KeyError, TypeError) as e:
        fail_usage("can't read %s (%s)." % (path, e))
    if not (isinstance(version, str) and re.match(r"^[0-9]+(\.[0-9]+)*$", version)):
        fail_usage("sources.json: libarchive.version must look like 3.8.10.")
    if not (isinstance(sha, str) and re.match(r"^[0-9a-f]{64}$", sha)):
        fail_usage("sources.json: libarchive.sha256 must be 64 lower-case hex digits.")
    if type(size) is not int or size <= 0:
        fail_usage("sources.json: libarchive.size must be a positive integer.")
    return version, sha, size


def uudecode(text, where):
    """The bytes between a uuencoded member's `begin` and `end` lines."""
    out = bytearray()
    started = False
    for line in text.splitlines():
        if not started:
            if line.startswith(b"begin "):
                started = True
            continue
        if line.strip() == b"end":
            return bytes(out)
        if not line:
            continue
        try:
            out += binascii.a2b_uu(line)
        except binascii.Error:
            # Some encoders pad lines short; decode just the declared length.
            n = (((line[0] - 32) & 63) * 4 + 5) // 3
            out += binascii.a2b_uu(line[:n])
    raise SystemExit("smoke: %s has no begin/end block." % where)


def rvint(b, p):
    v = s = 0
    while True:
        c = b[p]
        p += 1
        v |= (c & 0x7F) << s
        s += 7
        if not c & 0x80:
            return v, p


def rar_files(b):
    """[(size, CRC32 or None)] of the regular files, in header order.

    A port of the prototype's crccheck.py. RAR4: file headers (0x74) that
    are neither folders nor Unix symlinks. RAR5: file headers (type 2) that
    are not folders and have no redirection record (links, junctions, file
    copies). Enough for libarchive's own vectors, not a general parser.
    """
    out = []
    if b.startswith(b"Rar!\x1a\x07\x00"):
        p = 7
        while p + 7 <= len(b):
            _crc, typ, flags, size = struct.unpack_from("<HBHH", b, p)
            add = struct.unpack_from("<I", b, p + 7)[0] if (flags & 0x8000 or typ in (0x74, 0x7A)) else 0
            if typ == 0x74:
                unp, host, fcrc = struct.unpack_from("<IBI", b, p + 11)
                attr = struct.unpack_from("<I", b, p + 28)[0]
                isdir = (flags & 0xE0) == 0xE0
                islink = host == 3 and (attr & 0o170000) == 0o120000
                if not isdir and not islink:
                    out.append((unp, fcrc))
            if typ == 0x7B:
                break
            if size == 0:
                raise ValueError("a zero-size RAR4 header")
            p += size + add
    else:
        p = 8
        while p + 5 <= len(b):
            hsize, q = rvint(b, p + 4)
            end = q + hsize
            htype, q = rvint(b, q)
            hflags, q = rvint(b, q)
            esize = dsize = 0
            if hflags & 1:
                esize, q = rvint(b, q)
            if hflags & 2:
                dsize, q = rvint(b, q)
            if htype == 2:
                ff, q = rvint(b, q)
                unp, q = rvint(b, q)
                _, q = rvint(b, q)
                if ff & 2:
                    q += 4
                fcrc = struct.unpack_from("<I", b, q)[0] if ff & 4 else None
                x, redir = end - esize, False
                while x < end:
                    rs, x2 = rvint(b, x)
                    rt, _ = rvint(b, x2)
                    redir |= rt == 5
                    x = x2 + rs
                if not ff & 1 and not redir:
                    out.append((unp, fcrc))
            if htype == 5:
                break
            p = end + dsize
    return out


def environment(wd):
    env = {"PATH": wd, "TZ": "UTC0", "TAR_READER_OPTIONS": "rar:hdrcharset=UTF-8"}
    if sys.platform == "win32":
        if os.environ.get("SystemRoot"):
            env["SystemRoot"] = os.environ["SystemRoot"]
    elif sys.platform == "darwin":
        env["LC_ALL"] = "en_US.UTF-8"
    else:
        env["LC_ALL"] = "C.UTF-8"
    return env


class Run:
    def __init__(self, code, out, err, left):
        self.code, self.out, self.err, self.left = code, out, err, left


def run_bsdtar(bsdtar, data):
    """Runs bsdtar as Grape does, with `data` piped into stdin."""
    wd = tempfile.mkdtemp(prefix="grape-bsdtar-smoke-")
    try:
        kwargs = {}
        if sys.platform == "win32":
            kwargs["creationflags"] = CREATE_NO_WINDOW
        try:
            r = subprocess.run(
                [bsdtar] + ARGS,
                input=data,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                cwd=wd,
                env=environment(wd),
                timeout=TIMEOUT_S,
                **kwargs
            )
        except subprocess.TimeoutExpired:
            return Run(None, b"", b"", os.listdir(wd))
        return Run(r.returncode, r.stdout, r.stderr, os.listdir(wd))
    finally:
        shutil.rmtree(wd, ignore_errors=True)


def describe(run):
    """What went wrong, without bsdtar's stderr text (it can name entries)."""
    if run.code is None:
        return "timed out after %d s" % TIMEOUT_S
    return "exit %d, %d bytes on stderr, %d bytes on stdout" % (run.code, len(run.err), len(run.out))


def tar_entries(out):
    with tarfile.open(fileobj=io.BytesIO(out), mode="r:") as t:
        rows = []
        crcs = []
        for m in t.getmembers():
            kind = m.type.decode("latin-1")
            rows.append((kind, m.name, m.size, m.linkname))
            if m.isreg():
                f = t.extractfile(m)
                data = f.read() if f is not None else b""
                crcs.append((len(data), zlib.crc32(data)))
        return rows, crcs


def nfc(s):
    return unicodedata.normalize("NFC", s)


def check_good(name, data, run):
    if run.code != 0 or run.err or run.left:
        why = describe(run)
        if run.left:
            why += "; the working folder wasn't empty after"
        return why
    if len(run.out) % PAD != 0:
        return "stdout is %d bytes, not a multiple of %d" % (len(run.out), PAD)
    try:
        rows, crcs = tar_entries(run.out)
    except (tarfile.TarError, OSError, ValueError) as e:
        return "tarfile can't read the output (%s)" % e
    nfd = any(nfc(n) != n or nfc(link) != link for _, n, _, link in rows)
    print("  info  %s: %s" % (name, "some names came back NFD" if nfd else "names came back NFC"))
    got = [(k, nfc(n), size, nfc(link)) for k, n, size, link in rows]
    want = dict(GOOD)[name]
    if got != want:
        # %a, not %r: Windows' piped stdout is cp1252, and a Japanese name
        # would make print() fail instead of showing the difference.
        return "entries differ: want %a, got %a" % (want, got)
    expected = rar_files(data)
    if len(expected) != len(crcs):
        return "%d regular files in the RAR headers, %d in the tar" % (len(expected), len(crcs))
    for i, ((wsize, wcrc), (gsize, gcrc)) in enumerate(zip(expected, crcs)):
        if wsize != gsize:
            return "regular file %d: %d bytes in the RAR header, %d in the tar" % (i + 1, wsize, gsize)
        if wcrc is not None and wcrc != gcrc:
            return "regular file %d: CRC32 %08x in the RAR header, %08x in the tar" % (i + 1, wcrc, gcrc)
    return None


def check_bad(run, pattern, canary=False):
    if run.code != 1:
        return describe(run) + "; want exit 1"
    if run.left:
        return "the working folder wasn't empty after"
    if pattern is not None and pattern.encode("ascii") not in run.err:
        return "%s; stderr doesn't say %r" % (describe(run), pattern)
    if canary and CANARY.strip() in run.out:
        return "the canary's bytes are in stdout"
    return None


def stored_zip():
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", compression=zipfile.ZIP_STORED) as z:
        z.writestr("leaked.txt", CANARY)
    return buf.getvalue()


def tar_gz():
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as t:
        info = tarfile.TarInfo("leaked.txt")
        info.size = len(CANARY)
        t.addfile(info, io.BytesIO(CANARY))
    return buf.getvalue()


def main():
    ap = argparse.ArgumentParser(description="Smoke-test Grape's bsdtar in Grape's exact environment.")
    ap.add_argument("--bsdtar", required=True, help="the bsdtar to test")
    ap.add_argument("--tarball", required=True, help="the pinned libarchive release tarball")
    ap.add_argument("--sources", required=True, help="scripts/bsdtar/sources.json")
    args = ap.parse_args()

    # The child runs in another folder, so the program needs its full path.
    bsdtar = os.path.abspath(args.bsdtar)
    if not os.path.isfile(bsdtar):
        fail_usage("%s isn't a file." % args.bsdtar)
    version, want_sha, want_size = read_sources(args.sources)
    try:
        size = os.path.getsize(args.tarball)
        sha = sha256_file(args.tarball)
    except OSError as e:
        fail_usage("can't read the tarball (%s)." % e)
    if size != want_size or sha != want_sha:
        fail_usage("the tarball is %d bytes with SHA-256 %s; sources.json says %d and %s."
                   % (size, sha, want_size, want_sha))

    prefix = "libarchive-%s/libarchive/test/" % version
    needed = [n for n, _ in GOOD] + [n for n, _ in BAD]
    vectors = {}
    with tarfile.open(args.tarball, mode="r:gz") as t:
        for name in needed:
            member = prefix + name + ".uu"
            try:
                info = t.getmember(member)
            except KeyError:
                fail_usage("the tarball has no %s." % member)
            f = t.extractfile(info) if info.isreg() else None
            if f is None:
                fail_usage("%s in the tarball isn't a plain file." % member)
            vectors[name] = uudecode(f.read(), member)

    failures = 0

    def report(case, why):
        nonlocal failures
        if why is None:
            print("PASS  %s" % case)
        else:
            failures += 1
            print("FAIL  %s: %s" % (case, why))

    for name, _ in GOOD:
        data = vectors[name]
        report(name, check_good(name, data, run_bsdtar(bsdtar, data)))

    for name, pattern in BAD:
        report(name + " (must fail)", check_bad(run_bsdtar(bsdtar, vectors[name]), pattern))

    # Patch 0006: @- reads RAR and RAR5 only.
    canary_dir = tempfile.mkdtemp(prefix="grape-bsdtar-canary-")
    try:
        canary = os.path.join(canary_dir, "canary.txt")
        with open(canary, "wb") as f:
            f.write(CANARY)
        mtree = ("#mtree\n./leaked.txt type=file contents=%s\n" % canary).encode("utf-8")
        refused = [
            ("a stored ZIP", stored_zip()),
            ("an mtree spec naming a local file", mtree),
            ("a .tar.gz", tar_gz()),
            ("empty input", b""),
        ]
        for case, data in refused:
            report("0006 refuses %s" % case,
                   check_bad(run_bsdtar(bsdtar, data), "Unrecognized archive format", canary=True))
    finally:
        shutil.rmtree(canary_dir, ignore_errors=True)

    print("%d of %d cases failed." % (failures, len(GOOD) + len(BAD) + 4) if failures
          else "All %d cases passed." % (len(GOOD) + len(BAD) + 4))
    return min(failures, 255)


if __name__ == "__main__":
    sys.exit(main())
