#!/usr/bin/env python3
"""License notices in the source files compiled into Grape's FFmpeg (DECISIONS D24).

FFmpeg and the libraries built into it carry code from other projects under
BSD, MIT, ISC, zlib, IJG and similar licenses, whose notices binaries must
reproduce. This finds every source file a build compiled, with the headers it
included, reads all their comments, and checks that
licenses/ffmpeg/ffmpeg-notices.txt has every notice in them.

  notices.py check <target> <work folder> <sources.json> <licenses/ffmpeg>
      What scripts/ffmpeg/build.sh runs after building FFmpeg. Writes, in
      the target's logs/ folder: compiled-files.txt (every file, by
      source), notices-found.json and notices-found.txt (the notices, in
      the format of ffmpeg-notices.txt). Fails when a notice, or a file
      that has one, is missing from ffmpeg-notices.txt.

  notices.py render <notices-found.json>...
      Prints ffmpeg-notices.txt for the union of the given builds' notices.

  notices.py scan <sources.json> <label> <source name> <folder> <file>...
      Scans files no build here compiles (another CPU's assembly, say) and
      prints their notices as notices-found.json, for render. <folder> is
      where the source is unpacked; the files are paths inside it.

What counts, in each file's comments:
  - a notice: a permissive license's grant, conditions or disclaimer
    (GRANTS below), with the copyright lines just before it;
  - a statement: anything else that talks about licenses (LICENSEISH), like
    "based on public domain code" or "BSD licensed", with the copyright
    lines just before it. They're listed too, so that each one has been read.
Neither is needed for the library's own license given by reference (the GNU
boilerplate, libvpx's "see the LICENSE file", ...: OWN_LICENSE), nor for a
notice that's word for word in the library's license file in this folder.

Standard library only; run with python3 -I.
"""

import json
import os
import re
import subprocess
import sys

ME = "build.sh: notices"

# --- Reading comments -------------------------------------------------------

NASM = (".asm", ".nas", ".mac")
MARKS = ("/*", "*/", "//", ";", "*", "#")


def strip_decoration(text):
    """A comment line without its comment marks (/* */ * // ; #) and box edges."""
    text = re.sub(r"(?:\s*\*+/|\s+\*+)\s*$", "", text)
    for _ in range(3):
        text = text.lstrip()
        for m in MARKS:
            if text.startswith(m):
                text = text[len(m):].lstrip(m[-1])
                break
        else:
            break
    return re.sub(r"(?:\s*\*+/|\s+\*+)\s*$", "", text).strip()


def norm(text):
    return " ".join(text.split())


def decode(data):
    try:
        return data.decode("utf-8"), "utf-8"
    except UnicodeDecodeError:
        return data.decode("latin-1"), "latin-1"


def comment_lines(text, nasm):
    """Each line's comment text (marks included), whether it has code, and
    whether it's inside a comment.

    C, C++, gas and resource files: /* */ and //, skipping string and
    character literals. NASM: ; to the end of the line.
    """
    out = []
    in_block = False
    for line in text.split("\n"):
        if nasm:
            j = line.find(";")
            if j < 0:
                out.append(("", bool(line.strip()), False))
            else:
                out.append((line[j:], bool(line[:j].strip()), True))
            continue
        inside = in_block
        comment = []
        code = False
        i, n = 0, len(line)
        while i < n:
            if in_block or line.startswith("/*", i):
                j = line.find("*/", i if in_block else i + 2)
                end = n if j < 0 else j + 2
                comment.append(line[i:end])
                in_block = j < 0
                inside = True
                i = end
            elif line.startswith("//", i):
                comment.append(line[i:])
                inside = True
                break
            elif line[i] in "\"'":
                q = line[i]
                j = i + 1
                while j < n and line[j] != q:
                    j += 2 if line[j] == "\\" else 1
                code = True
                i = j + 1
            else:
                code = code or not line[i].isspace()
                i += 1
        out.append(("".join(comment), code, inside))
    return out


def comment_blocks(lines):
    """Runs of comment lines with no code between them, as paragraphs, split
    where a line is empty once the comment marks are gone. Each paragraph:
    (whether a new comment starts before it, [(line index, stripped text)])."""
    blocks, paras, para = [], [], []
    boundary = opens = True
    for k, (comment, code, inside) in enumerate(lines):
        if code:
            if para:
                paras.append((opens, para))
            if paras:
                blocks.append(paras)
            paras, para, boundary = [], [], True
            continue
        lead = comment.lstrip()
        starts = not inside or lead.startswith("/*") or (
            lead.startswith("//") and (k == 0 or not lines[k - 1][0].lstrip().startswith("//")))
        stripped = strip_decoration(comment) if comment else ""
        if stripped:
            if not para:
                opens, boundary = boundary or starts, False
            para.append((k, stripped))
            continue
        if para:
            paras.append((opens, para))
            para = []
        boundary = boundary or starts
    if para:
        paras.append((opens, para))
    if paras:
        blocks.append(paras)
    return blocks


# --- What a comment says ----------------------------------------------------

# A permissive license's grant, conditions or disclaimer: these make a notice.
GRANTS = (
    "redistribution and use in source and binary forms",
    "permission is hereby granted",
    "permission to use, copy, modify",
    "permission is granted to",
    "without fee is hereby granted",
    "provided 'as-is'",
    "provided \"as-is\"",
    "is permitted provided that",
    "the above copyright notice",
    "this permission notice",
    "notices are preserved",
    "notice is not removed",
    "spdx-license-identifier",
)
# Words that carry a notice on into the next paragraph.
CONTINUES = (
    "warrant", "liabl", "liabilit", "as is", "as-is", "disclaim", "endorse",
    "damages", "redistribut", "conditions", "notice", "neither the name",
    "advertising", "acknowledg", "permission", "permit", "restriction",
    "misrepresent", "provided that", "independent jpeg group", "courtesy",
)
# The library's own license, given by reference: no notice of its own.
OWN_LICENSE = (
    re.compile(r"is free software; you can redistribute it and/or modify it under the terms of the gnu (?:lesser |library )?general public license as published by the free software foundation; either version (?:2|2\.1) of the license, or \(at your option\) any later version"),
    re.compile(r"use of this source code is governed by a bsd-style license (?:and patent grant )?that can be found in the license file in the root of the source tree"),
    re.compile(r"governed by a bsd-style source license included with this source in 'copying'"),
    re.compile(r"for conditions of distribution and use, see copyright notice in zlib\.h"),
)
# The rest of the GNU boilerplate, and x264's note on its other license.
KNOWN = (
    re.compile(r"is distributed in the hope that it will be useful, but without any warranty; without even the implied warranty of merchantability or fitness for a particular purpose\. see the gnu (?:lesser |library )?general public license for more details"),
    re.compile(r"you should have received a copy of the gnu (?:lesser |library )?general public license along with"),
    re.compile(r"^this program is also available under a commercial proprietary license\. for more information, contact us at licensing@x264\.com\.?$"),
)
LICENSEISH = re.compile(r"""
    \blicen[cs]ed\b
  | \blicen[cs]es?\s+(?:v(?:ersion)?\s*)?[0-9]
  | \b(?:bsd|mit|isc|apache|mozilla|mpl|l?gpl|gnu|zlib|boost|wtfpl|cc0|creative\ commons|unlicense|eclipse|cddl|artistic)\b[^.;]{0,30}\blicen[cs]e
  | \blicen[cs]e\b[^.;]{0,30}\b(?:bsd|mit|isc|apache|mozilla|mpl|l?gpl|gnu|zlib|boost|wtfpl|cc0|unlicense)\b
  | public\ domain | rights\ reserved | \bwarrant | copyleft | spdx
  | (?:released|distributed|available|redistributed|relicensed)\ under
  | under\ the\ (?:terms|conditions)\ of | permitted\ provided | freely\ (?:re)?distribut
  | copyright\ notice | this\ notice | permission\ notice | terms\ of\ use
""", re.X | re.I)
RESERVED_ONLY = re.compile(r"^rights reserved$", re.I)
COPYRIGHT = re.compile(r"copyright|\(c\)|©|all rights reserved", re.I)
AUTHORS = re.compile(r"^(?:authors?|written by|contributors?|original author|maintainers?|modified by|based on)\b", re.I)
GNU = re.compile(r"\bgnu\b|general public license", re.I)


def has_any(text, words):
    return any(w in text for w in words)


def scan_text(text, nasm):
    """Returns the notices and statements in a file's comments: lists of
    (first line index, last line index)."""
    lines = comment_lines(text, nasm)
    notices, statements = [], []
    for block in comment_blocks(lines):
        opens = [o for o, _ in block]
        paras = [p for _, p in block]
        low = [norm(" ".join(s for _, s in p)).lower() for p in paras]
        own = [any(r.search(t) for r in OWN_LICENSE) for t in low]
        used = [o or any(r.search(t) for r in KNOWN) for o, t in zip(own, low)]
        for k, t in enumerate(low):
            # The copyright lines right before or after the library's own
            # license (libvpx's before, Xiph's after) go with it.
            if not own[k] and COPYRIGHT.search(t) and not has_any(t, GRANTS) \
                    and all(RESERVED_ONLY.match(h.group(0)) for h in LICENSEISH.finditer(t)) \
                    and ((k > 0 and own[k - 1]) or (k + 1 < len(low) and own[k + 1])):
                used[k] = True

        def back(i):
            """Takes in the copyright lines (and authors) before paragraph i,
            in this comment or the ones just before it, looking past up to
            two paragraphs of something else (Opus puts a @file comment
            between its copyright lines and its license)."""
            def near(k):
                return not used[k] and not GNU.search(low[k]) and (
                    COPYRIGHT.search(low[k]) or AUTHORS.match(low[k])
                    or has_any(low[k], GRANTS) or has_any(low[k], CONTINUES))

            def other(k):
                return not used[k] and not GNU.search(low[k]) and not LICENSEISH.search(low[k])

            start = i
            while True:
                while start > 0 and near(start - 1):
                    start -= 1
                for j in (start - 2, start - 3):
                    if j >= 0 and near(j) and COPYRIGHT.search(low[j]) \
                            and all(other(k) for k in range(j + 1, start)):
                        start = j
                        break
                else:
                    break
            while start < i and AUTHORS.match(low[start]) and not COPYRIGHT.search(low[start]):
                start += 1
            return start

        def take(start, end, into):
            into.append([start, end])
            for k in range(start, end + 1):
                used[k] = True

        mine = []
        for i in range(len(paras)):
            if used[i] or not has_any(low[i], GRANTS):
                continue
            start = back(i)
            end = i
            # On to the end of the conditions and the disclaimer, in this comment.
            while end + 1 < len(paras) and not used[end + 1] and not opens[end + 1] \
                    and not GNU.search(low[end + 1]) and not COPYRIGHT.match(low[end + 1]) \
                    and (has_any(low[end + 1], GRANTS) or has_any(low[end + 1], CONTINUES)):
                end += 1
            if mine and mine[-1][1] + 1 == start:
                mine[-1][1] = end  # one notice right after another: one text
                for k in range(start, end + 1):
                    used[k] = True
            else:
                take(start, end, mine)
        mine_statements = []
        for i in range(len(paras)):
            if used[i]:
                continue
            hits = [m.group(0) for m in LICENSEISH.finditer(low[i])]
            if not hits:
                continue
            if all(RESERVED_ONLY.match(h) for h in hits) and i + 1 < len(paras) \
                    and not used[i + 1] and LICENSEISH.search(low[i + 1]):
                continue  # taken in by the statement that follows it
            take(back(i), i, mine_statements)
        for into, spans in ((notices, mine), (statements, mine_statements)):
            for start, end in spans:
                into.append((paras[start][0][0], paras[end][-1][0]))
    return lines, notices, statements


class Scanned:
    """One file's notices and statements: (normalized text, verbatim excerpt)."""

    def __init__(self, path):
        with open(path, "rb") as f:
            text, self.encoding = decode(f.read())
        text = text.replace("\r\n", "\n").replace("\r", "\n")
        lines, notices, statements = scan_text(text, path.endswith(NASM))

        def excerpt(first, last):
            got = [lines[k][0] for k in range(first, last + 1)]
            return (norm(" ".join(strip_decoration(c) for c in got)),
                    "\n".join(c.rstrip() for c in got))

        self.notices = [excerpt(a, b) for a, b in notices]
        self.statements = [excerpt(a, b) for a, b in statements]


# --- Labels -----------------------------------------------------------------

def kind_of(plain):
    t = plain.lower()
    if "independent jpeg group" in t:
        return "IJG"
    if "redistribution and use in source and binary forms" in t:
        if "advertising materials" in t:
            return "BSD-4-Clause"
        if re.search(r"neither the name|may (?:not )?be used to endorse|names? of (?:the )?(?:author|contributors)", t):
            return "BSD-3-Clause"
        return "BSD-2-Clause"
    if "permission is hereby granted, free of charge" in t:
        return "MIT" if "to any person obtaining a copy" in t else "BSL-1.0" if "covered by this license" in t else "MIT-style"
    if re.search(r"permission to use, copy, modify, and(?:/or)? distribute this software for any purpose with or without fee is hereby granted", t):
        return "ISC"
    if "provided 'as-is'" in t and "permission is granted to anyone" in t:
        return "Zlib"
    return "permissive"


def holders(excerpt):
    """Who holds a notice's copyright, from its lines that start with one."""
    out = []
    lines = [strip_decoration(x) for x in excerpt.split("\n")]
    for k, s in enumerate(lines):
        if not re.match(r"(?:copyright\b(?!\s+notice)|\(c\)|©)", s, re.I):
            continue
        if re.match(r"(?i)^(?:copyright|\(c\)|©|\s|[0-9,-])*$", s) and k + 1 < len(lines):
            s = lines[k + 1]  # "Copyright (c) 2013", and who on the next line
        s = re.sub(r"(?i)copyright|\(c\)|©|all rights reserved\.?", " ", s)
        s = re.sub(r"\b(?:19|20)[0-9]{2}\b(?:\s*[-,]\s*(?:(?:19|20)?[0-9]{2})\b)*", " ", s)
        s = norm(s).strip(" .,:;")
        s = re.sub(r"^by\s+", "", s)
        if s and s not in out:
            out.append(s)
    who = "; ".join(out)
    return who if len(who) <= 90 else who[:87].rstrip() + "..."


# --- Sources ----------------------------------------------------------------

def die(msg):
    sys.stderr.write("%s: %s\n" % (ME, msg))
    sys.exit(1)


SOURCE_EXT = (".c", ".h", ".S", ".s", ".asm", ".inc")


def read_sources(path):
    with open(path, encoding="utf-8") as f:
        data = json.load(f)
    out = {}
    for name, s in data["sources"].items():
        if name.startswith("_"):
            continue
        pin = s.get("version") or s["commit"][:8]
        out[name] = {"dir": s["dir"], "version": pin,
                     "license_files": [p for p in s["license_files"] if not p.endswith(SOURCE_EXT)]}
    return out


def own_license_text(folder, files):
    parts = []
    for rel in files:
        with open(os.path.join(folder, rel), "rb") as f:
            text, _ = decode(f.read())
        parts.append(norm(" ".join(strip_decoration(x) for x in text.split("\n"))))
    return " ".join(parts)


# --- What a build compiled ----------------------------------------------------

OBJ = re.compile(r"\.(?:o|obj|lo)$")
# FFmpeg's objects for programs it runs while building (ffbuild/bin2c),
# compiled with the build machine's compiler and never linked in.
HOST = re.compile(r"_host\.o$")
RULE = re.compile(r"^(.*?):(?:\s|$)(.*)$")
INCLUDE_NASM = re.compile(r"^\s*%\s*include\s+[\"<]([^\">]+)[\">]", re.M)
INCLUDE_C = re.compile(r"^\s*#\s*include\s+\"([^\"]+)\"", re.M)


def parse_make_deps(path):
    """A make-style dependency file's rules: [(targets, prerequisites)]."""
    with open(path, "rb") as f:
        text, _ = decode(f.read())
    rules = []
    for line in text.replace("\\\n", " ").split("\n"):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        m = RULE.match(line)
        if m:
            rules.append((m.group(1).split(), [p for p in m.group(2).split() if p != "|"]))
    return rules


def parse_ninja(build_dir):
    """A meson build's objects and their inputs: the sources from
    build.ninja (for objects that exist), the headers from ninja's log."""
    rules = []
    with open(os.path.join(build_dir, "build.ninja"), encoding="utf-8") as f:
        text = f.read().replace("$\n", " ")
    for line in text.split("\n"):
        m = re.match(r"^build (.*?)(?<!\$): (\S+)(.*)$", line)
        if not m:
            continue
        outs = [o for o in m.group(1).split() if OBJ.search(o)]
        outs = [o for o in outs if os.path.exists(os.path.join(build_dir, o))]
        if outs:
            rules.append((outs, m.group(3).split("|")[0].split()))
    res = subprocess.run(["ninja", "-C", build_dir, "-t", "deps"], stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, check=False)
    if res.returncode != 0:
        die("ninja -t deps failed in %s: %s" % (build_dir, res.stderr.decode("utf-8", "replace")))
    target, deps = None, []
    for line in res.stdout.decode("utf-8", "replace").split("\n"):
        if not line.strip():
            continue
        if line.startswith(" "):
            deps.append(line.strip())
            continue
        if target:
            rules.append(([target], deps))
        target, deps = line.split(": #deps")[0], []
    if target:
        rules.append(([target], deps))
    return rules


class Build:
    """The files one build compiled, by source, from its dependency records."""

    def __init__(self, tdir, sources):
        self.tdir = os.path.realpath(tdir)
        self.roots = []  # (folder, source name, how paths in it are shown)
        for name, s in sources.items():
            d = os.path.join(self.tdir, "src", s["dir"])
            if os.path.isdir(d):
                self.roots.append((d, name, s["dir"]))
            b = os.path.join(self.tdir, "build-" + name)
            if os.path.isdir(b):
                self.roots.append((b, name, "build-" + name))
        self.prefix = os.path.join(self.tdir, "prefix")
        self.index = {}  # source name -> {file name: [paths]}
        for root, name, _ in self.roots:
            idx = self.index.setdefault(name, {})
            for dirpath, _, filenames in os.walk(root):
                for fn in filenames:
                    idx.setdefault(fn, []).append(os.path.join(dirpath, fn))
        self.files = {}  # path -> source name
        self.toolchain = set()
        self.problems = []

    def owner(self, path):
        for root, name, shown in self.roots:
            if path.startswith(root + os.sep):
                return root, name, shown
        return None

    def display(self, path):
        root, _, shown = self.owner(path)
        return shown + "/" + os.path.relpath(path, root).replace(os.sep, "/")

    def add(self, path):
        path = os.path.realpath(path)
        if not path.startswith(self.tdir + os.sep):
            self.toolchain.add(path)  # the compiler's and the C library's headers
            return
        if path.startswith(self.prefix + os.sep):
            # A library's header, as installed: the file it was installed from.
            with open(path, "rb") as f:
                data = f.read()
            found = []
            for idx in self.index.values():
                for cand in idx.get(os.path.basename(path), []):
                    with open(cand, "rb") as f:
                        if f.read() == data:
                            found.append(cand)
            if not found:
                self.problems.append("can't tell which source %s was installed from." % os.path.relpath(path, self.tdir))
            for p in found:
                self.add(p)
            return
        own = self.owner(path)
        if not own:
            self.problems.append("%s is in no source's folder." % os.path.relpath(path, self.tdir))
        elif os.path.isfile(path):
            self.files[path] = own[1]

    def resolve(self, rel, bases):
        for b in [""] if os.path.isabs(rel) else bases:
            p = os.path.normpath(os.path.join(b, rel))
            if os.path.exists(p):
                return p
        return None

    def take_rules(self, rules, bases, where):
        stems = set()
        for targets, prereqs in rules:
            objs = [t for t in targets if OBJ.search(t)]
            if not objs:
                continue
            if all(HOST.search(t) for t in objs):
                continue  # a program the build runs on the build machine
            for t in targets:
                if OBJ.search(t):
                    stems.add(OBJ.sub("", self.resolve(t, bases) or os.path.normpath(os.path.join(bases[0], t))))
            for p in prereqs:
                r = self.resolve(p, bases)
                if r:
                    self.add(r)
                elif not os.path.isabs(p):
                    self.problems.append("%s names %s, which isn't there." % (where, p))
        return stems

    def collect(self):
        compiled = set()  # objects the records account for, without extension
        for root, _, _ in self.roots:
            if os.path.isfile(os.path.join(root, "build.ninja")):
                compiled |= self.take_rules(parse_ninja(root), [root], self.display(os.path.join(root, "build.ninja")))
                continue
            for dirpath, _, filenames in os.walk(root):
                for fn in filenames:
                    if fn.endswith((".d", ".Po", ".Plo")):
                        dep = os.path.join(dirpath, fn)
                        # Automake runs the compiler in the Makefile's folder
                        # (above .deps) or the top one; the others in the top.
                        bases = [os.path.dirname(dirpath)] if os.path.basename(dirpath) == ".deps" else []
                        bases += [root, dirpath]
                        compiled |= self.take_rules(parse_make_deps(dep), bases, self.display(dep))
        # Objects no record accounts for (zlib's Makefile writes none): the
        # source with the object's name.
        fallback = []
        for root, _, _ in self.roots:
            for dirpath, _, filenames in os.walk(root):
                if os.path.basename(dirpath) == ".libs":
                    continue  # libtool's copies of objects it has records for
                for fn in filenames:
                    if not OBJ.search(fn) or fn.endswith(".lo") or HOST.search(fn):
                        continue
                    stem = OBJ.sub("", os.path.join(dirpath, fn))
                    if stem in compiled:
                        continue
                    for ext in (".c", ".S", ".asm", ".rc"):
                        if os.path.isfile(stem + ext):
                            self.add(stem + ext)
                            fallback.append(os.path.realpath(stem + ext))
                            break
                    else:
                        self.problems.append("nothing says what %s was compiled from." % self.display(os.path.join(dirpath, fn)))
        # NASM leaves %include out of its dependency output in some builds
        # (x264's, dav1d's), so follow them, and the fallback files' #include.
        todo = [p for p in self.files if p.endswith(NASM)] + fallback
        seen = set(todo)
        while todo:
            path = todo.pop()
            name = self.files.get(path)
            if not name:
                continue
            with open(path, "rb") as f:
                text, _ = decode(f.read())
            for inc in (INCLUDE_NASM if path.endswith(NASM) else INCLUDE_C).findall(text):
                inc = inc.strip()
                local = os.path.normpath(os.path.join(os.path.dirname(path), inc))
                if os.path.isfile(local):
                    cands = [local]
                else:
                    tail = "/" + inc.lstrip("./")
                    cands = [c for c in self.index[name].get(os.path.basename(inc), [])
                             if c.replace(os.sep, "/").endswith(tail)]
                for c in cands:
                    c = os.path.realpath(c)
                    self.add(c)
                    if c not in seen:
                        seen.add(c)
                        todo.append(c)
        return self.files


# --- Notice lists ---------------------------------------------------------------

def found_notices(files, display, sources, folders, targets):
    """{(source, kind, normalized text): {excerpt, first, files: {shown: targets}}},
    and which files carry their library's own license text."""
    found = {}
    covered = {}
    own = {}
    for path, name in sorted(files.items(), key=lambda kv: display(kv[0])):
        s = Scanned(path)
        items = [("notice", p, e) for p, e in s.notices] + [("statement", p, e) for p, e in s.statements]
        for kind, plain, excerpt in items:
            if kind == "notice":
                if name not in own:
                    own[name] = own_license_text(folders[name], sources[name]["license_files"])
                if plain in own[name]:
                    covered.setdefault(name, set()).add(display(path))
                    continue
            e = found.setdefault((name, kind, plain), {"excerpt": excerpt, "first": display(path),
                                                       "encoding": s.encoding, "files": {}})
            e["files"].setdefault(display(path), set()).update(targets)
    return found, covered


def to_json(found, sources, label):
    out = []
    for (name, kind, plain), e in sorted(found.items()):
        out.append({"source": name, "kind": kind, "dir": sources[name]["dir"],
                    "version": sources[name]["version"], "text": plain, "excerpt": e["excerpt"],
                    "first": e["first"], "encoding": e["encoding"],
                    "files": {f: sorted(t) for f, t in sorted(e["files"].items())}})
    return {"format": 1, "label": label, "notices": out}


ORDER = ("ffmpeg", "x264", "lame", "opus", "libogg", "libvorbis", "libvpx", "dav1d", "zlib")
NAMES = {"ffmpeg": "FFmpeg", "x264": "x264", "lame": "LAME", "opus": "Opus", "libogg": "libogg",
         "libvorbis": "libvorbis", "libvpx": "libvpx", "dav1d": "dav1d", "zlib": "zlib"}
RULER = "=" * 78

INTRO = """\
Notices in the source of Grape's ffmpeg and ffprobe
===================================================

The programs are under LICENSE.txt in this folder (GPL-2.0-or-later), and the
libraries built into them under their own texts here (SOURCE.md lists them).
Some of their source files also hold code from elsewhere, under BSD, MIT, ISC,
zlib, IJG and similar licenses, whose notices must go with the binaries. This
file has every such notice in the files the builds compile, and the headers
those include, that isn't already word for word in the library's own license
text in this folder.

As those licenses ask: this software is based in part on the work of the
Independent JPEG Group.

scripts/ffmpeg/build.sh finds the files from each build's own dependency
records and scripts/ffmpeg/notices.py reads every comment in them. The build
stops when it finds a notice, or a file with one, that this file doesn't
have. notices.py also wrote this file, from the builds named in it. Files
marked "macos-arm64 (read, not built yet)" are the AArch64 sources that only
the Apple silicon build compiles; they were read from the source because
that build hasn't run yet, and its first run checks them.

Part 1 has the notices. Part 2 has the other things those files' comments
say about licenses: code that came from the public domain, or that was
relicensed, or a license named without its text. They need nothing more
from Grape, and they're here so that each has been read.

Each entry: the source, the license the text reads as (a guide only: the
text is what counts) and who holds it; the files that carry it, each with
the builds that compile it; then the text, verbatim from the first of those
files, comment marks and all. Paths start with the folder the source's
tarball unpacks to (build-<source>/ is that source's own build folder).
"""


def render(docs):
    merged = {}
    for d in docs:
        for n in d["notices"]:
            e = merged.setdefault((n["source"], n["kind"], n["text"]), {"n": n, "files": {}, "excerpts": {}})
            e["excerpts"][n["first"]] = n["excerpt"]
            for f, t in n["files"].items():
                e["files"].setdefault(f, set()).update(t)

    def order(item):
        (source, kind, text), e = item
        return (kind != "notice", ORDER.index(source) if source in ORDER else 99, min(e["files"]), text)

    out = [INTRO]
    part = None
    for no, ((source, kind, text), e) in enumerate(sorted(merged.items(), key=order), 1):
        if kind != part:
            part = kind
            out.append(RULER)
            out.append("Part 1: notices" if kind == "notice" else "Part 2: other statements about licenses")
        n = e["n"]
        files = sorted(e["files"])
        excerpt = e["excerpts"].get(files[0], n["excerpt"])
        out.append(RULER)
        head = "[%d] %s %s" % (no, NAMES.get(source, source), n["version"])
        if kind == "notice":
            head += ", " + kind_of(text)
            who = holders(excerpt)
            if who:
                head += ": " + who
        out.append(head)
        out.append("Files:")
        width = max(len(f) for f in files)
        for f in files:
            out.append("  %-*s  %s" % (width, f, ", ".join(sorted(e["files"][f]))))
        out.append("Text:")
        out.append("")
        out.append(excerpt)
        out.append("")
    out.append(RULER)
    return "\n".join(out) + "\n"


def parse_notices_file(path):
    """[(number, files, normalized text)] of ffmpeg-notices.txt's entries."""
    with open(path, encoding="utf-8") as f:
        text = f.read()
    entries = []
    for chunk in text.split("\n" + RULER + "\n"):
        lines = chunk.split("\n")
        m = re.match(r"^\[([0-9]+)\] ", lines[0])
        if not m:
            continue
        if len(lines) < 3 or lines[1] != "Files:":
            die("%s: entry [%s] has no 'Files:' line after its title." % (path, m.group(1)))
        files = set()
        i = 2
        while i < len(lines) and lines[i].startswith("  "):
            files.add(lines[i].split()[0])
            i += 1
        if i >= len(lines) or lines[i] != "Text:":
            die("%s: entry [%s] has no 'Text:' line after its files." % (path, m.group(1)))
        body = norm(" ".join(strip_decoration(x) for x in lines[i + 1:]))
        entries.append((int(m.group(1)), files, body))
    return entries


# --- Commands -------------------------------------------------------------------

def cmd_check(target, tdir, sources_json, licenses):
    sources = read_sources(sources_json)
    build = Build(tdir, sources)
    files = build.collect()
    logs = os.path.join(build.tdir, "logs")
    with open(os.path.join(logs, "compiled-files.txt"), "w", encoding="utf-8") as f:
        f.write("The files the %s build compiled, with the headers they include, by source.\n" % target)
        for name in ORDER:
            mine = sorted(build.display(p) for p, n in files.items() if n == name)
            f.write("\n%s (%d files)\n" % (name, len(mine)))
            f.writelines("  %s\n" % p for p in mine)
        f.write("\nThe toolchain's own headers, not scanned (%d files)\n" % len(build.toolchain))
        f.writelines("  %s\n" % p for p in sorted(build.toolchain))
    for p in build.problems:
        sys.stderr.write("%s: %s\n" % (ME, p))
    if build.problems:
        die("couldn't tell what the build compiled (above).")
    for name in sources:
        if name not in files.values():
            die("found nothing the build compiled from %s." % name)

    folders = {name: os.path.join(build.tdir, "src", s["dir"]) for name, s in sources.items()}
    found, covered = found_notices(files, build.display, sources, folders, [target])
    doc = to_json(found, sources, target)
    with open(os.path.join(logs, "notices-found.json"), "w", encoding="utf-8") as f:
        json.dump(doc, f, indent=1, sort_keys=True)
    with open(os.path.join(logs, "notices-found.txt"), "w", encoding="utf-8") as f:
        f.write(render([doc]))

    counts = {}
    for n in files.values():
        counts[n] = counts.get(n, 0) + 1
    print("  read the comments of %d files: %s" % (
        len(files), ", ".join("%s %d" % (n, counts.get(n, 0)) for n in ORDER)))
    for name in ORDER:
        if name in covered:
            print("  %s: %d with a notice that's word for word in its %s" % (
                name, len(covered[name]), " or ".join(sources[name]["license_files"])))

    entries = parse_notices_file(os.path.join(licenses, "ffmpeg-notices.txt"))
    bad = []
    for (_, kind, plain), e in sorted(found.items(), key=lambda kv: min(kv[1]["files"])):
        holders_ = [(no, fs) for no, fs, body in entries if plain in body]
        if not holders_:
            bad.append("ffmpeg-notices.txt lacks this %s, in %s:\n%s" % (kind, ", ".join(sorted(e["files"])), e["excerpt"]))
            continue
        for f in sorted(e["files"]):
            if not any(f in fs for _, fs in holders_):
                bad.append("ffmpeg-notices.txt doesn't list %s under its %s, entry [%d]." % (f, kind, holders_[0][0]))
    kinds = [k for (_, k, _) in found]
    print("  %d notices and %d other statements about licenses, in %d files; ffmpeg-notices.txt has %d entries" % (
        kinds.count("notice"), kinds.count("statement"),
        len({f for e in found.values() for f in e["files"]}), len(entries)))
    if bad:
        for b in bad:
            sys.stderr.write("%s: %s\n" % (ME, b))
        die("%d problems (above). logs/notices-found.txt has what this build found, in the same format; "
            "scripts/ffmpeg/notices.py render merges builds' notices-found.json into a new file." % len(bad))
    print("  ffmpeg-notices.txt has every one, under every file")


def cmd_render(paths):
    docs = []
    for p in paths:
        with open(p, encoding="utf-8") as f:
            docs.append(json.load(f))
    sys.stdout.write(render(docs))


def cmd_scan(sources_json, label, name, folder, paths):
    sources = read_sources(sources_json)
    if name not in sources:
        die("no source named %s." % name)
    folder = os.path.realpath(folder)
    files = {os.path.realpath(os.path.join(folder, p)): name for p in paths}

    def display(path):
        return sources[name]["dir"] + "/" + os.path.relpath(path, folder).replace(os.sep, "/")

    found, _ = found_notices(files, display, sources, {name: folder}, [label])
    json.dump(to_json(found, sources, label), sys.stdout, indent=1, sort_keys=True)
    sys.stdout.write("\n")


def main(argv):
    if len(argv) == 6 and argv[1] == "check":
        cmd_check(*argv[2:])
    elif len(argv) >= 3 and argv[1] == "render":
        cmd_render(argv[2:])
    elif len(argv) >= 7 and argv[1] == "scan":
        cmd_scan(argv[2], argv[3], argv[4], argv[5], argv[6:])
    else:
        sys.stderr.write(__doc__)
        sys.exit(2)


if __name__ == "__main__":
    main(sys.argv)
