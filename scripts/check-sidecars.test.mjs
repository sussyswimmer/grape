// Tests for the sidecar bundling gate (D25). Run: node --test scripts/check-sidecars.test.mjs
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { join } from 'node:path';

import {
  bannerProtocol,
  check,
  defaultOverlay,
  firstLine,
  grapeProtocol,
  hasSection,
  PROTOCOL_SOURCE,
  sidecarFile,
  sidecarName,
} from './check-sidecars.mjs';

const REPO = '/repo';
const APP = join(REPO, 'apps', 'desktop', 'src-tauri');
const WINDOWS_OVERLAY = join(APP, 'tauri.sidecars.windows.conf.json');
const MACOS_OVERLAY = join(APP, 'tauri.sidecars.macos.conf.json');
const OVERLAY = JSON.stringify({
  build: { beforeBuildCommand: 'node ../../scripts/check-sidecars.mjs && pnpm -C ui build' },
  bundle: { externalBin: ['binaries/ffmpeg', 'binaries/ffprobe'] },
});
const GRAPE_FFMPEG = 'ffmpeg version 9.0.2-grape Copyright (c) 2000-2026 the FFmpeg developers\nbuilt with gcc\n';
const GRAPE_FFPROBE = 'ffprobe version 9.0.2-grape Copyright (c) 2007-2026 the FFmpeg developers\r\n';
const NOTICES = '# Third-party notices\n\n## PDFium\n\ntext\n\n## FFmpeg\n\nGrape runs its own build of FFmpeg.\n';

/** A fake file system and runner: `files` maps paths to contents, `outputs` maps programs to stdout or an Error. */
function fakeIo({ files = {}, dirs = {}, outputs = {} } = {}) {
  const ran = [];
  return {
    ran,
    exists: (path) => path in files || path in dirs,
    isFile: (path) => path in files,
    list: (dir) => dirs[dir] ?? [],
    read: (path) => {
      if (!(path in files)) throw new Error(`ENOENT: ${path}`);
      return files[path];
    },
    run: (file, args) => {
      ran.push([file, args]);
      const out = outputs[file];
      if (out instanceof Error) throw out;
      if (out === undefined) throw new Error(`spawn ${file} ENOENT`);
      return out;
    },
  };
}

/** Everything a passing Windows build has. */
function windowsOk(overrides = {}) {
  const ffmpeg = join(APP, 'binaries', 'ffmpeg-x86_64-pc-windows-msvc.exe');
  const ffprobe = join(APP, 'binaries', 'ffprobe-x86_64-pc-windows-msvc.exe');
  return {
    files: {
      [WINDOWS_OVERLAY]: OVERLAY,
      [join(REPO, 'about.hbs')]: NOTICES,
      [join(REPO, 'THIRD_PARTY_NOTICES.md')]: NOTICES,
      [ffmpeg]: 'MZ',
      [ffprobe]: 'MZ',
      ...overrides.files,
    },
    dirs: { [join(REPO, 'licenses', 'ffmpeg')]: ['LICENSE.txt', 'SOURCE.md'], ...overrides.dirs },
    outputs: { [ffmpeg]: GRAPE_FFMPEG, [ffprobe]: GRAPE_FFPROBE, ...overrides.outputs },
  };
}

test('names follow Tauri externalBin: the triple, and .exe on Windows only', () => {
  assert.equal(sidecarFile('binaries/ffmpeg', 'x86_64-pc-windows-msvc'), 'binaries/ffmpeg-x86_64-pc-windows-msvc.exe');
  assert.equal(sidecarFile('binaries/ffmpeg', 'universal-apple-darwin'), 'binaries/ffmpeg-universal-apple-darwin');
  assert.equal(sidecarFile('binaries/ffprobe', 'aarch64-apple-darwin'), 'binaries/ffprobe-aarch64-apple-darwin');
  assert.equal(sidecarName('binaries/ffprobe'), 'ffprobe');
});

test('first lines and sections', () => {
  assert.equal(firstLine('a b\r\nc'), 'a b');
  assert.equal(firstLine(''), '');
  assert.ok(hasSection(NOTICES, 'FFmpeg'));
  assert.ok(!hasSection('### FFmpeg\n', 'FFmpeg'));
  assert.ok(!hasSection('## FFmpegs\n', 'FFmpeg'));
  assert.ok(!hasSection('Grape uses FFmpeg.\n', 'FFmpeg'));
});

test("Grape's own build with its licenses passes on Windows", () => {
  const io = fakeIo(windowsOk());
  assert.deepEqual(check({ repo: REPO, triple: 'x86_64-pc-windows-msvc', io }), []);
  assert.deepEqual(io.ran.map(([, args]) => args), [['-hide_banner', '-version'], ['-hide_banner', '-version']]);
});

test('a universal Mac build checks the universal files', () => {
  const ffmpeg = join(APP, 'binaries', 'ffmpeg-universal-apple-darwin');
  const ffprobe = join(APP, 'binaries', 'ffprobe-universal-apple-darwin');
  const base = windowsOk();
  const io = fakeIo({
    files: {
      [MACOS_OVERLAY]: OVERLAY,
      [join(REPO, 'about.hbs')]: NOTICES,
      [join(REPO, 'THIRD_PARTY_NOTICES.md')]: NOTICES,
      [ffmpeg]: '',
      [ffprobe]: '',
    },
    dirs: base.dirs,
    outputs: { [ffmpeg]: GRAPE_FFMPEG, [ffprobe]: GRAPE_FFPROBE },
  });
  assert.deepEqual(check({ repo: REPO, triple: 'universal-apple-darwin', io }), []);
});

test("the prebuilt dev pins are refused: no -grape marker", () => {
  const ffmpeg = join(APP, 'binaries', 'ffmpeg-x86_64-pc-windows-msvc.exe');
  const io = fakeIo(
    windowsOk({ outputs: { [ffmpeg]: 'ffmpeg version n9.0.2-2-g1234abcd-20261001 Copyright (c) 2000-2026 the FFmpeg developers\n' } }),
  );
  const failures = check({ repo: REPO, triple: 'x86_64-pc-windows-msvc', io });
  assert.equal(failures.length, 1);
  assert.match(failures[0], /^ffmpeg: this isn't Grape's own build/);
  assert.match(failures[0], /n9\.0\.2-2-g1234abcd-20261001/);
});

test('the marker must be in the version itself, not elsewhere on the line', () => {
  const ffprobe = join(APP, 'binaries', 'ffprobe-x86_64-pc-windows-msvc.exe');
  for (const line of [
    'ffprobe version 9.0.2 Copyright -grape',
    'ffprobe version 9.0.2-grapefruit Copyright',
    'ffmpeg version 9.0.2-grape Copyright',
    ' ffprobe version 9.0.2-grape',
  ]) {
    const io = fakeIo(windowsOk({ outputs: { [ffprobe]: `${line}\n` } }));
    const failures = check({ repo: REPO, triple: 'x86_64-pc-windows-msvc', io });
    assert.equal(failures.length, 1, line);
    assert.match(failures[0], /^ffprobe: this isn't Grape's own build/, line);
  }
  for (const line of ['ffprobe version 9.0.2-grape', 'ffprobe version 9.0.2-grape.2 Copyright']) {
    const io = fakeIo(windowsOk({ outputs: { [ffprobe]: `${line}\n` } }));
    assert.deepEqual(check({ repo: REPO, triple: 'x86_64-pc-windows-msvc', io }), [], line);
  }
});

test("a missing sidecar or one that won't run is a failure", () => {
  const ffmpeg = join(APP, 'binaries', 'ffmpeg-x86_64-pc-windows-msvc.exe');
  const ffprobe = join(APP, 'binaries', 'ffprobe-x86_64-pc-windows-msvc.exe');
  const base = windowsOk({ outputs: { [ffprobe]: new Error('spawnSync ETIMEDOUT\nmore') } });
  delete base.files[ffmpeg];
  const failures = check({ repo: REPO, triple: 'x86_64-pc-windows-msvc', io: fakeIo(base) });
  assert.equal(failures.length, 2);
  assert.match(failures[0], /^ffmpeg: src-tauri\/binaries\/ffmpeg-x86_64-pc-windows-msvc\.exe is missing\./);
  assert.equal(failures[1], "ffprobe: couldn't run it to read its version (spawnSync ETIMEDOUT).");
});

test('licenses: the folder, SOURCE.md for GPL code, and the notices section', () => {
  const licenses = join(REPO, 'licenses', 'ffmpeg');
  let failures = check({ repo: REPO, triple: 'x86_64-pc-windows-msvc', io: fakeIo(windowsOk({ dirs: { [licenses]: [] } })) });
  assert.deepEqual(failures, ['ffmpeg: licenses/ffmpeg/ is missing or empty (ENGINES.md licensing rule 3).']);

  failures = check({ repo: REPO, triple: 'x86_64-pc-windows-msvc', io: fakeIo(windowsOk({ dirs: { [licenses]: ['LICENSE.txt'] } })) });
  assert.equal(failures.length, 1);
  assert.match(failures[0], /SOURCE\.md is missing/);

  failures = check({
    repo: REPO,
    triple: 'x86_64-pc-windows-msvc',
    io: fakeIo(windowsOk({ files: { [join(REPO, 'about.hbs')]: '## PDFium\n' } })),
  });
  assert.deepEqual(failures, ['ffmpeg: about.hbs has no "## FFmpeg" section (licenses/README.md).']);

  failures = check({
    repo: REPO,
    triple: 'x86_64-pc-windows-msvc',
    io: fakeIo(windowsOk({ files: { [join(REPO, 'THIRD_PARTY_NOTICES.md')]: '## PDFium\n' } })),
  });
  assert.equal(failures.length, 1);
  assert.match(failures[0], /THIRD_PARTY_NOTICES\.md has no "## FFmpeg" section/);
});

test('the license checks run once for sidecars that share a folder', () => {
  const failures = check({
    repo: REPO,
    triple: 'x86_64-pc-windows-msvc',
    io: fakeIo(windowsOk({ dirs: { [join(REPO, 'licenses', 'ffmpeg')]: [] } })),
  });
  assert.equal(failures.length, 1);
});

test('a sidecar without a rule, or an overlay without sidecars, is refused', () => {
  const overlay = WINDOWS_OVERLAY;
  let failures = check({
    repo: REPO,
    triple: 'x86_64-pc-windows-msvc',
    io: fakeIo(windowsOk({ files: { [overlay]: JSON.stringify({ bundle: { externalBin: ['binaries/ffmpeg', 'binaries/ffprobe', 'binaries/other'] } }) } })),
  });
  assert.deepEqual(failures, ['other: the gate has no rule for this sidecar. Add one to scripts/check-sidecars.mjs (D25).']);

  failures = check({
    repo: REPO,
    triple: 'x86_64-pc-windows-msvc',
    io: fakeIo(windowsOk({ files: { [overlay]: '{"bundle":{}}' } })),
  });
  assert.equal(failures.length, 1);
  assert.match(failures[0], /lists no externalBin/);

  failures = check({ repo: REPO, triple: 'x86_64-pc-windows-msvc', io: fakeIo(windowsOk({ files: { [overlay]: '{' } })) });
  assert.equal(failures.length, 1);
  assert.match(failures[0], /^Couldn't read tauri\.sidecars\.windows\.conf\.json/);
});

test("each OS reads its own sidecars overlay by default, and Linux has none", () => {
  assert.equal(defaultOverlay('x86_64-pc-windows-msvc'), 'tauri.sidecars.windows.conf.json');
  assert.equal(defaultOverlay('aarch64-pc-windows-msvc'), 'tauri.sidecars.windows.conf.json');
  for (const triple of ['universal-apple-darwin', 'aarch64-apple-darwin', 'x86_64-apple-darwin']) {
    assert.equal(defaultOverlay(triple), 'tauri.sidecars.macos.conf.json', triple);
  }
  assert.equal(defaultOverlay('x86_64-unknown-linux-gnu'), undefined);

  // The Mac reads the macOS overlay: a Windows one alone isn't found.
  const base = windowsOk();
  const failures = check({ repo: REPO, triple: 'universal-apple-darwin', io: fakeIo(base) });
  assert.equal(failures.length, 1);
  assert.match(failures[0], /^Couldn't read tauri\.sidecars\.macos\.conf\.json/);

  assert.deepEqual(check({ repo: REPO, triple: 'x86_64-unknown-linux-gnu', io: fakeIo(base) }), [
    'Grape has no sidecars overlay for x86_64-unknown-linux-gnu: it ships for Windows and macOS only. Pass --overlay <file>.',
  ]);
});

test('every overlay with sidecars runs this gate before the UI build of the base config', async () => {
  const { readdirSync, readFileSync } = await import('node:fs');
  const { fileURLToPath } = await import('node:url');
  const app = fileURLToPath(new URL('../apps/desktop/src-tauri/', import.meta.url));
  const base = JSON.parse(readFileSync(join(app, 'tauri.conf.json'), 'utf8'));
  const names = readdirSync(app).filter((f) => /^tauri\..+\.conf\.json$/.test(f));
  let checked = [];
  for (const name of names) {
    const overlay = JSON.parse(readFileSync(join(app, name), 'utf8'));
    if (overlay?.bundle?.externalBin === undefined) continue;
    // The Tauri CLI runs it in apps/desktop (the folder with package.json), with
    // cmd /S /C on Windows and sh -c elsewhere; both run the second part only
    // when the gate exits 0.
    assert.equal(overlay.build.beforeBuildCommand, `node ../../scripts/check-sidecars.mjs && ${base.build.beforeBuildCommand}`, name);
    assert.deepEqual(Object.keys(overlay), ['build', 'bundle'], name);
    checked.push(name);
  }
  // One sidecars overlay per OS, and nothing else sets externalBin.
  assert.deepEqual(checked.sort(), ['tauri.sidecars.macos.conf.json', 'tauri.sidecars.windows.conf.json']);
});

// bsdtar (D27): Grape's build of libarchive's bsdtar, from scripts/bsdtar.
const BSDTAR_WINDOWS = 'bsdtar 3.8.10-grape.1 - libarchive 3.8.10 libb2/bundled \r\n';
const BSDTAR_MACOS = 'bsdtar 3.8.10-grape.1 - libarchive 3.8.10 CommonCrypto/system libb2/bundled \n';
const OVERLAY_ALL = JSON.stringify({
  build: { beforeBuildCommand: 'node ../../scripts/check-sidecars.mjs && pnpm -C ui build' },
  bundle: { externalBin: ['binaries/ffmpeg', 'binaries/ffprobe', 'binaries/bsdtar'] },
});
const NOTICES_ALL = `${NOTICES}\n## libarchive\n\nGrape reads RAR archives with bsdtar.\n`;

/** Everything a passing build with FFmpeg and bsdtar has, for one triple. */
function allOk(triple, bsdtarOut, overrides = {}) {
  const file = (name) => join(APP, sidecarFile(`binaries/${name}`, triple));
  return {
    files: {
      [join(APP, defaultOverlay(triple))]: OVERLAY_ALL,
      [join(REPO, 'about.hbs')]: NOTICES_ALL,
      [join(REPO, 'THIRD_PARTY_NOTICES.md')]: NOTICES_ALL,
      [file('ffmpeg')]: '',
      [file('ffprobe')]: '',
      [file('bsdtar')]: '',
      ...overrides.files,
    },
    dirs: {
      [join(REPO, 'licenses', 'ffmpeg')]: ['LICENSE.txt', 'SOURCE.md'],
      [join(REPO, 'licenses', 'libarchive')]: ['LICENSE.txt', 'copyright-lines.txt'],
      ...overrides.dirs,
    },
    outputs: {
      [file('ffmpeg')]: GRAPE_FFMPEG,
      [file('ffprobe')]: GRAPE_FFPROBE,
      [file('bsdtar')]: bsdtarOut,
      ...overrides.outputs,
    },
  };
}

test("Grape's bsdtar passes on Windows and as a universal Mac file", () => {
  let io = fakeIo(allOk('x86_64-pc-windows-msvc', BSDTAR_WINDOWS));
  assert.deepEqual(check({ repo: REPO, triple: 'x86_64-pc-windows-msvc', io }), []);
  assert.deepEqual(io.ran[2], [join(APP, 'binaries', 'bsdtar-x86_64-pc-windows-msvc.exe'), ['--version']]);

  io = fakeIo(allOk('universal-apple-darwin', BSDTAR_MACOS));
  assert.deepEqual(check({ repo: REPO, triple: 'universal-apple-darwin', io }), []);
  assert.deepEqual(io.ran[2], [join(APP, 'binaries', 'bsdtar-universal-apple-darwin'), ['--version']]);

  // The same line on macOS without CommonCrypto, and without the trailing space.
  for (const out of ['bsdtar 3.8.10-grape.1 - libarchive 3.8.10 libb2/bundled \n', 'bsdtar 3.8.10-grape.12 - libarchive 3.8.10\n']) {
    io = fakeIo(allOk('universal-apple-darwin', out));
    assert.deepEqual(check({ repo: REPO, triple: 'universal-apple-darwin', io }), [], out);
  }
});

test('stock and system bsdtars are refused: no -grape marker on the version', () => {
  const bsdtar = join(APP, 'binaries', 'bsdtar-x86_64-pc-windows-msvc.exe');
  for (const line of [
    // Stock 3.8.10, built without Grape's patches.
    'bsdtar 3.8.10 - libarchive 3.8.10 libb2/bundled ',
    // Windows' own tar.exe.
    'bsdtar 3.7.7 - libarchive 3.7.7 zlib/1.2.13.1-motley liblzma/5.4.3 bz2lib/1.0.8 libzstd/1.5.5',
    // -grape anywhere but on bsdtar's own version doesn't count.
    'bsdtar 3.8.10 - libarchive 3.8.10-grape.1 libb2/bundled ',
    'bsdtar 3.8.10 - libarchive 3.8.10 libb2/bundled-grape ',
    'bsdtar 3.8.10 - libarchive 3.8.10 x/-grape.1',
    'bsdtar 3.8.10-grapefruit - libarchive 3.8.10 libb2/bundled ',
    'bsdtar 3.8.10-grape.1 - libarchive 3.8.10 libb2/bundled extra',
    ' bsdtar 3.8.10-grape.1 - libarchive 3.8.10',
  ]) {
    const io = fakeIo(allOk('x86_64-pc-windows-msvc', BSDTAR_WINDOWS, { outputs: { [bsdtar]: `${line}\n` } }));
    const failures = check({ repo: REPO, triple: 'x86_64-pc-windows-msvc', io });
    assert.equal(failures.length, 1, line);
    assert.match(failures[0], /^bsdtar: this isn't Grape's own build\./, line);
    assert.match(failures[0], /scripts\/bsdtar.*\(D27\)\.$/, line);
  }
});

test('a missing bsdtar points at scripts/bsdtar and the bsdtar workflow', () => {
  const base = allOk('universal-apple-darwin', BSDTAR_MACOS);
  delete base.files[join(APP, 'binaries', 'bsdtar-universal-apple-darwin')];
  const failures = check({ repo: REPO, triple: 'universal-apple-darwin', io: fakeIo(base) });
  assert.equal(failures.length, 1);
  assert.match(failures[0], /^bsdtar: src-tauri\/binaries\/bsdtar-universal-apple-darwin is missing\. Build it with scripts\/bsdtar\/build\.ps1/);
});

test("bsdtar's licenses: the folder and the notices section, but no SOURCE.md rule", () => {
  const triple = 'x86_64-pc-windows-msvc';
  // Not GPL, so LICENSE.txt alone is enough.
  let failures = check({ repo: REPO, triple, io: fakeIo(allOk(triple, BSDTAR_WINDOWS, { dirs: { [join(REPO, 'licenses', 'libarchive')]: ['LICENSE.txt'] } })) });
  assert.deepEqual(failures, []);

  failures = check({ repo: REPO, triple, io: fakeIo(allOk(triple, BSDTAR_WINDOWS, { dirs: { [join(REPO, 'licenses', 'libarchive')]: [] } })) });
  assert.deepEqual(failures, ['bsdtar: licenses/libarchive/ is missing or empty (ENGINES.md licensing rule 3).']);

  failures = check({ repo: REPO, triple, io: fakeIo(allOk(triple, BSDTAR_WINDOWS, { files: { [join(REPO, 'about.hbs')]: NOTICES } })) });
  assert.deepEqual(failures, ['bsdtar: about.hbs has no "## libarchive" section (licenses/README.md).']);

  failures = check({ repo: REPO, triple, io: fakeIo(allOk(triple, BSDTAR_WINDOWS, { files: { [join(REPO, 'THIRD_PARTY_NOTICES.md')]: NOTICES } })) });
  assert.equal(failures.length, 1);
  assert.match(failures[0], /^bsdtar: THIRD_PARTY_NOTICES\.md has no "## libarchive" section/);
});

test('every externalBin in the real overlays has a rule and ships its licenses folder', async () => {
  const { readdirSync, readFileSync } = await import('node:fs');
  const { fileURLToPath } = await import('node:url');
  const { RULES } = await import('./check-sidecars.mjs');
  const app = fileURLToPath(new URL('../apps/desktop/src-tauri/', import.meta.url));
  const overlays = readdirSync(app).filter((f) => /^tauri.*\.conf\.json$/.test(f));
  let checked = 0;
  for (const name of overlays) {
    const conf = JSON.parse(readFileSync(join(app, name), 'utf8'));
    const bins = conf?.bundle?.externalBin;
    if (bins === undefined) continue;
    assert.ok(Array.isArray(bins) && bins.length > 0, `${name}: externalBin must be a non-empty list`);
    const resources = conf.bundle.resources ?? {};
    for (const entry of bins) {
      const rule = RULES[sidecarName(entry)];
      assert.ok(rule, `${name}: ${entry} has no rule in scripts/check-sidecars.mjs`);
      const folder = `../../../licenses/${rule.licenses}/`;
      assert.equal(resources[folder], `licenses/${rule.licenses}/`, `${name}: ${entry} needs "${folder}" in bundle.resources`);
      checked++;
    }
  }
  // Windows: ffmpeg, ffprobe, bsdtar and grape-heif. macOS: the first three.
  assert.equal(checked, 7, `${checked} externalBin entries found`);
});

test("the bsdtar workflow's cache keys cover the copyright lines its full build checks", async () => {
  // The notices check runs only in a full build, and a cache hit skips the
  // build: a key without the file would pass a changed copyright-lines.txt.
  const { readFileSync } = await import('node:fs');
  const { fileURLToPath } = await import('node:url');
  const yml = readFileSync(fileURLToPath(new URL('../.github/workflows/bsdtar.yml', import.meta.url)), 'utf8');
  const keys = [...yml.matchAll(/^\s*key: (bsdtar-.*)$/gm)].map((m) => m[1]);
  assert.equal(keys.length, 2, 'one cache key per OS');
  for (const key of keys) {
    assert.match(key, /hashFiles\('scripts\/bsdtar\/\*\*', 'licenses\/libarchive\/copyright-lines\.txt'\)/);
  }
});

// grape-heif (D33, D41): Grape's image helper on Windows, built from tools/grape-heif.
const HEIF_EXE = join(APP, 'binaries', 'grape-heif-x86_64-pc-windows-msvc.exe');
const GRAPE_HEIF =
  'grape-heif version 1.23.7-grape.2 Copyright (c) 2026 the Grape contributors\r\n' +
  'configuration: libheif=1.23.7 libde265=1.1.3 x265=4.3 dav1d=1.5.4 aom=3.15.2 plugins=disabled crt=static protocol=2\r\n';
const GATE = 'node ../../scripts/check-sidecars.mjs && pnpm -C ui build';
// An overlay file and a TAURI_CONFIG that list only grape-heif (for these
// tests only), so the tests below are about its rule and the sources. A real
// Windows build's list has all four sidecars: see "the real overlays, merged".
const HEIF_ONLY_FILE = join(APP, 'tauri.test-heif.conf.json');
const HEIF_ONLY = JSON.stringify({
  build: { beforeBuildCommand: GATE },
  bundle: { externalBin: ['binaries/grape-heif'], resources: { '../../../licenses/grape-heif/': 'licenses/grape-heif/' } },
});
const HEIF_CONFIG = JSON.stringify({
  bundle: {
    resources: {
      'binaries/pdfium-x86_64-pc-windows-msvc.dll': 'pdfium.dll',
      '../../../licenses/pdfium/': 'licenses/pdfium/',
      '../../../licenses/grape-heif/': 'licenses/grape-heif/',
    },
    externalBin: ['binaries/grape-heif'],
  },
  build: { beforeBuildCommand: GATE },
});
const NOTICES_HEIF = `${NOTICES}\n## grape-heif (HEIC on Windows)\n\nGrape converts HEIC on Windows with grape-heif.exe.\n`;
// grape-heif-proto's PROTOCOL, which the banner's configuration line must name.
const PROTO = join(REPO, ...PROTOCOL_SOURCE);
const PROTO_RS = '/// The protocol version.\npub const PROTOCOL: u32 = 2;\n';
const HEIF_LICENSES = ['LICENSE.txt', 'SOURCE.md', 'libheif.txt', 'libde265.txt', 'libde265-win32cond.txt', 'x265.txt', 'rust-crates.md'];

/** Everything a passing check of grape-heif alone has. The default overlay lists FFmpeg, which isn't there. */
function heifOk(overrides = {}) {
  return {
    files: {
      [WINDOWS_OVERLAY]: OVERLAY,
      [HEIF_ONLY_FILE]: HEIF_ONLY,
      [join(REPO, 'about.hbs')]: NOTICES_HEIF,
      [join(REPO, 'THIRD_PARTY_NOTICES.md')]: NOTICES_HEIF,
      [HEIF_EXE]: 'MZ',
      [PROTO]: PROTO_RS,
      ...overrides.files,
    },
    dirs: { [join(REPO, 'licenses', 'grape-heif')]: HEIF_LICENSES, ...overrides.dirs },
    outputs: { [HEIF_EXE]: GRAPE_HEIF, ...overrides.outputs },
  };
}

const WIN = 'x86_64-pc-windows-msvc';

test("TAURI_CONFIG's merged list wins over the overlay file", () => {
  const io = fakeIo(heifOk());
  assert.deepEqual(check({ repo: REPO, triple: WIN, io, config: HEIF_CONFIG }), []);
  assert.deepEqual(io.ran, [[HEIF_EXE, ['-hide_banner', '-version']]]);
  // Without it, the default overlay's FFmpeg is checked instead (and missing).
  const failures = check({ repo: REPO, triple: WIN, io: fakeIo(heifOk()) });
  assert.match(failures[0], /^ffmpeg: src-tauri\/binaries\/ffmpeg-x86_64-pc-windows-msvc\.exe is missing\./);
  assert.ok(failures.every((f) => !f.startsWith('grape-heif')), failures.join('\n'));
});

test('a TAURI_CONFIG without sidecars, or one that isn\'t JSON, is refused and named', () => {
  let failures = check({ repo: REPO, triple: WIN, io: fakeIo(heifOk()), config: '{"bundle":{"resources":{}}}' });
  assert.deepEqual(failures, ['TAURI_CONFIG lists no externalBin, so there is nothing to bundle.']);
  failures = check({ repo: REPO, triple: WIN, io: fakeIo(heifOk()), config: '{' });
  assert.equal(failures.length, 1);
  assert.match(failures[0], /^Couldn't read TAURI_CONFIG: /);
});

test('--overlay reads that file and names it', () => {
  const overlay = HEIF_ONLY_FILE;
  assert.deepEqual(check({ repo: REPO, triple: WIN, io: fakeIo(heifOk()), overlay }), []);
  let failures = check({ repo: REPO, triple: WIN, io: fakeIo(heifOk({ files: { [overlay]: '{"bundle":{}}' } })), overlay });
  assert.deepEqual(failures, ['tauri.test-heif.conf.json lists no externalBin, so there is nothing to bundle.']);
  failures = check({ repo: REPO, triple: WIN, io: fakeIo(heifOk()), overlay: join(APP, 'tauri.nothing.conf.json') });
  assert.equal(failures.length, 1);
  assert.match(failures[0], /^Couldn't read tauri\.nothing\.conf\.json: /);
});

test("grape-heif: Grape's own build passes, anything else is refused", () => {
  for (const out of [
    GRAPE_HEIF,
    'grape-heif version 1.23.7-grape\nconfiguration: protocol=2\n',
    'grape-heif version 1.23.8-grape.12 Copyright\nconfiguration: libheif=1.23.8 protocol=2 plugins=disabled\n',
  ]) {
    assert.deepEqual(check({ repo: REPO, triple: WIN, io: fakeIo(heifOk({ outputs: { [HEIF_EXE]: out } })), config: HEIF_CONFIG }), [], out);
  }
  for (const line of [
    // libheif's own tools, or a build without Grape's version.
    'heif-enc libheif version: 1.23.7',
    'grape-heif version 1.23.7 Copyright (c) 2026 the Grape contributors',
    'grape-heif version 1.23.7-grapefruit Copyright',
    'grape-heif version 1.23.7 configuration: -grape.1',
    ' grape-heif version 1.23.7-grape.1',
    'ffmpeg version 9.0.2-grape Copyright',
  ]) {
    const failures = check({ repo: REPO, triple: WIN, io: fakeIo(heifOk({ outputs: { [HEIF_EXE]: `${line}\n` } })), config: HEIF_CONFIG });
    assert.equal(failures.length, 1, line);
    assert.match(failures[0], /^grape-heif: this isn't Grape's own build\./, line);
    assert.match(failures[0], /tools\/grape-heif ever ships \(D33\)\.$/, line);
  }
});

test('grape-heif must speak the protocol in grape-heif-proto (D41)', () => {
  const stale = (spoken) =>
    `grape-heif: it ${spoken} and this Grape speaks 2, so it would refuse every HEIC and AVIF job. ` +
    "Build it again with tools/grape-heif/build.ps1 -Install, or take it from CI's grape-heif workflow (D41).";
  for (const [out, spoken] of [
    // D33's build, from before AVIF.
    [
      'grape-heif version 1.23.7-grape.1 Copyright (c) 2026 the Grape contributors\r\n' +
        'configuration: libheif=1.23.7 libde265=1.1.3 x265=4.3 plugins=disabled crt=static protocol=1\r\n',
      'speaks protocol 1',
    ],
    // A newer helper than this checkout.
    ['grape-heif version 1.23.8-grape.1\nconfiguration: libheif=1.23.8 protocol=3\n', 'speaks protocol 3'],
    // No configuration line, or no protocol on it, or not a whole token.
    ['grape-heif version 1.23.7-grape.2 Copyright\n', "doesn't say which protocol it speaks"],
    ['grape-heif version 1.23.7-grape.2\nconfiguration: libheif=1.23.7 crt=static\n', "doesn't say which protocol it speaks"],
    ['grape-heif version 1.23.7-grape.2\nconfiguration: protocol=2x\n', "doesn't say which protocol it speaks"],
    ['grape-heif version 1.23.7-grape.2 protocol=2\nlicense: GPL-3.0-or-later\n', "doesn't say which protocol it speaks"],
  ]) {
    const failures = check({ repo: REPO, triple: WIN, io: fakeIo(heifOk({ outputs: { [HEIF_EXE]: out } })), config: HEIF_CONFIG });
    assert.deepEqual(failures, [stale(spoken)], out);
  }
  // A build that isn't Grape's is refused for that alone.
  const foreign = check({
    repo: REPO,
    triple: WIN,
    io: fakeIo(heifOk({ outputs: { [HEIF_EXE]: 'heif-enc libheif version: 1.23.7\nconfiguration: protocol=1\n' } })),
    config: HEIF_CONFIG,
  });
  assert.equal(foreign.length, 1);
  assert.match(foreign[0], /^grape-heif: this isn't Grape's own build\./);
  // Without grape-heif-proto's PROTOCOL the gate can't tell, so it refuses.
  for (const source of [undefined, 'pub const PROTOCOL: u32 = two;\n']) {
    const base = heifOk();
    if (source === undefined) delete base.files[PROTO];
    else base.files[PROTO] = source;
    assert.deepEqual(check({ repo: REPO, triple: WIN, io: fakeIo(base), config: HEIF_CONFIG }), [
      'grape-heif: couldn\'t read PROTOCOL from crates/grape-heif-proto/src/lib.rs.',
    ]);
  }
});

test("the gate reads the real PROTOCOL, and the banner fixture speaks it", async () => {
  const { readFileSync } = await import('node:fs');
  const { fileURLToPath } = await import('node:url');
  const source = readFileSync(fileURLToPath(new URL(`../${PROTOCOL_SOURCE.join('/')}`, import.meta.url)), 'utf8');
  const protocol = grapeProtocol(source);
  assert.ok(Number.isInteger(protocol) && protocol >= 2, `${protocol}`);
  assert.equal(bannerProtocol(GRAPE_HEIF), protocol);
  // tools/grape-heif's banner docs show the same line.
  const banner = readFileSync(fileURLToPath(new URL('../tools/grape-heif/src/banner.rs', import.meta.url)), 'utf8');
  const shown = banner.split('\n').find((l) => l.startsWith('//! configuration: '));
  assert.equal(bannerProtocol(shown.slice('//! '.length)), protocol, shown);
  assert.equal(grapeProtocol('pub const PROTOCOL: u32 = 12;'), 12);
  assert.equal(grapeProtocol('const PROTOCOL: u32 = 2;'), undefined);
});

test('a missing grape-heif.exe points at build.ps1 and the grape-heif workflow', () => {
  const base = heifOk();
  delete base.files[HEIF_EXE];
  const failures = check({ repo: REPO, triple: WIN, io: fakeIo(base), config: HEIF_CONFIG });
  assert.deepEqual(failures, [
    "grape-heif: src-tauri/binaries/grape-heif-x86_64-pc-windows-msvc.exe is missing. Build it with tools/grape-heif/build.ps1, or take it from CI's grape-heif workflow (D33).",
  ]);
});

test("grape-heif's licenses: SOURCE.md (GPL) and its notices section", () => {
  const dir = join(REPO, 'licenses', 'grape-heif');
  let failures = check({
    repo: REPO,
    triple: WIN,
    io: fakeIo(heifOk({ dirs: { [dir]: HEIF_LICENSES.filter((f) => f !== 'SOURCE.md') } })),
    config: HEIF_CONFIG,
  });
  assert.deepEqual(failures, [
    'grape-heif: licenses/grape-heif/SOURCE.md is missing. GPL code ships with where its exact source is (ENGINES.md licensing rule 3).',
  ]);

  failures = check({ repo: REPO, triple: WIN, io: fakeIo(heifOk({ dirs: { [dir]: [] } })), config: HEIF_CONFIG });
  assert.deepEqual(failures, ['grape-heif: licenses/grape-heif/ is missing or empty (ENGINES.md licensing rule 3).']);

  failures = check({ repo: REPO, triple: WIN, io: fakeIo(heifOk({ files: { [join(REPO, 'about.hbs')]: NOTICES } })), config: HEIF_CONFIG });
  assert.deepEqual(failures, ['grape-heif: about.hbs has no "## grape-heif" section (licenses/README.md).']);

  failures = check({
    repo: REPO,
    triple: WIN,
    io: fakeIo(heifOk({ files: { [join(REPO, 'THIRD_PARTY_NOTICES.md')]: NOTICES } })),
    config: HEIF_CONFIG,
  });
  assert.equal(failures.length, 1);
  assert.match(failures[0], /^grape-heif: THIRD_PARTY_NOTICES\.md has no "## grape-heif" section/);
});

test('the script reads TAURI_CONFIG and --overlay when it runs', async () => {
  const { spawnSync } = await import('node:child_process');
  const { fileURLToPath } = await import('node:url');
  const script = fileURLToPath(new URL('./check-sidecars.mjs', import.meta.url));
  const run = (args, env) =>
    spawnSync(process.execPath, [script, '--target', WIN, ...args], {
      encoding: 'utf8',
      env: { ...process.env, TAURI_CONFIG: undefined, TAURI_ENV_TARGET_TRIPLE: undefined, ...env },
    });
  // A name with no rule fails the same way whatever is on this machine.
  let r = run([], { TAURI_CONFIG: JSON.stringify({ bundle: { externalBin: ['binaries/not-a-sidecar'] } }) });
  assert.equal(r.status, 1, r.stderr);
  assert.match(r.stderr, /the sidecars TAURI_CONFIG lists for x86_64-pc-windows-msvc/);
  assert.match(r.stderr, /not-a-sidecar: the gate has no rule/);

  r = run(['--overlay', 'tauri.nothing.conf.json'], {});
  assert.equal(r.status, 1, r.stderr);
  assert.match(r.stderr, /Couldn't read tauri\.nothing\.conf\.json/);

  r = run(['--overlay', 'tauri.nothing.conf.json'], { TAURI_CONFIG: '{"bundle":{}}' });
  assert.equal(r.status, 1, r.stderr);
  assert.match(r.stderr, /--overlay is ignored/);
  assert.match(r.stderr, /TAURI_CONFIG lists no externalBin/);

  r = run(['--overlay'], {});
  assert.equal(r.status, 2, r.stderr);
});

/** RFC 7396, as the Tauri CLI merges `--config` overlays: objects merge, everything else replaces. */
function mergePatch(target, patch) {
  if (patch === null || typeof patch !== 'object' || Array.isArray(patch)) return patch;
  const out = target !== null && typeof target === 'object' && !Array.isArray(target) ? { ...target } : {};
  for (const [key, value] of Object.entries(patch)) {
    if (value === null) delete out[key];
    else out[key] = mergePatch(out[key], value);
  }
  return out;
}

/** The real overlays in apps/desktop/src-tauri, merged in order onto tauri.conf.json. */
async function mergedConfig(...names) {
  const { readFileSync } = await import('node:fs');
  const { fileURLToPath } = await import('node:url');
  const app = fileURLToPath(new URL('../apps/desktop/src-tauri/', import.meta.url));
  return names.reduce((conf, name) => mergePatch(conf, JSON.parse(readFileSync(join(app, name), 'utf8'))), {});
}

test('the real overlays, merged as the Tauri CLI merges them, bundle every sidecar and license folder', async () => {
  const windows = await mergedConfig('tauri.conf.json', 'tauri.pdfium.windows.conf.json', 'tauri.sidecars.windows.conf.json');
  assert.deepEqual(windows.bundle.externalBin, ['binaries/ffmpeg', 'binaries/ffprobe', 'binaries/bsdtar', 'binaries/grape-heif']);
  for (const [from, to] of [
    ['binaries/pdfium-x86_64-pc-windows-msvc.dll', 'pdfium.dll'],
    ['../../../licenses/pdfium/', 'licenses/pdfium/'],
    ['../../../licenses/ffmpeg/', 'licenses/ffmpeg/'],
    ['../../../licenses/libarchive/', 'licenses/libarchive/'],
    ['../../../licenses/grape-heif/', 'licenses/grape-heif/'],
    ['../../../THIRD_PARTY_NOTICES.md', 'THIRD_PARTY_NOTICES.md'],
  ]) {
    assert.equal(windows.bundle.resources[from], to, from);
  }
  assert.equal(windows.build.beforeBuildCommand, GATE);

  const macos = await mergedConfig('tauri.conf.json', 'tauri.pdfium.macos.conf.json', 'tauri.sidecars.macos.conf.json');
  assert.deepEqual(macos.bundle.externalBin, ['binaries/ffmpeg', 'binaries/ffprobe', 'binaries/bsdtar']);
  assert.deepEqual(macos.bundle.macOS.frameworks, ['binaries/macos/libpdfium.dylib']);
  for (const folder of ['pdfium', 'ffmpeg', 'libarchive']) {
    assert.equal(macos.bundle.resources[`../../../licenses/${folder}/`], `licenses/${folder}/`, folder);
  }
  assert.equal(macos.bundle.resources['../../../licenses/grape-heif/'], undefined);
  assert.equal(macos.build.beforeBuildCommand, GATE);
});

test("a Windows build's real merged list passes with Grape's four sidecars, and names each one missing", async () => {
  const config = JSON.stringify(await mergedConfig('tauri.pdfium.windows.conf.json', 'tauri.sidecars.windows.conf.json'));
  const all = allOk(WIN, BSDTAR_WINDOWS);
  const heif = heifOk();
  const base = {
    files: {
      ...all.files,
      [HEIF_EXE]: 'MZ',
      [PROTO]: PROTO_RS,
      [join(REPO, 'about.hbs')]: `${NOTICES_ALL}${NOTICES_HEIF}`,
      [join(REPO, 'THIRD_PARTY_NOTICES.md')]: `${NOTICES_ALL}${NOTICES_HEIF}`,
    },
    dirs: { ...all.dirs, ...heif.dirs },
    outputs: { ...all.outputs, ...heif.outputs },
  };
  const io = fakeIo(base);
  assert.deepEqual(check({ repo: REPO, triple: WIN, io, config }), []);
  assert.deepEqual(
    io.ran.map(([file]) => file),
    ['ffmpeg', 'ffprobe', 'bsdtar', 'grape-heif'].map((n) => join(APP, sidecarFile(`binaries/${n}`, WIN))),
  );

  for (const name of ['ffmpeg', 'ffprobe', 'bsdtar', 'grape-heif']) {
    const files = { ...base.files };
    delete files[join(APP, sidecarFile(`binaries/${name}`, WIN))];
    const failures = check({ repo: REPO, triple: WIN, io: fakeIo({ ...base, files }), config });
    assert.equal(failures.length, 1, name);
    assert.ok(failures[0].startsWith(`${name}: src-tauri/binaries/${name}-x86_64-pc-windows-msvc.exe is missing.`), failures[0]);
  }
});

test("a universal Mac build's real merged list passes with Grape's three sidecars", async () => {
  const config = JSON.stringify(await mergedConfig('tauri.pdfium.macos.conf.json', 'tauri.sidecars.macos.conf.json'));
  const io = fakeIo(allOk('universal-apple-darwin', BSDTAR_MACOS));
  assert.deepEqual(check({ repo: REPO, triple: 'universal-apple-darwin', io, config }), []);
  assert.equal(io.ran.length, 3);
});

/** Every `pnpm tauri build` line in the workflows, with the overlays it passes. */
async function workflowBuilds() {
  const { readdirSync, readFileSync } = await import('node:fs');
  const { fileURLToPath } = await import('node:url');
  const workflows = fileURLToPath(new URL('../.github/workflows/', import.meta.url));
  const builds = [];
  for (const file of readdirSync(workflows).filter((f) => f.endsWith('.yml'))) {
    for (const line of readFileSync(join(workflows, file), 'utf8').split('\n')) {
      if (!/tauri build/.test(line) || line.trim().startsWith('#')) continue;
      const overlays = [...line.matchAll(/--config src-tauri\/(\S+)/g)].map((m) => m[1]);
      builds.push({ file, line, overlays });
    }
  }
  return builds;
}

test('no workflow build passes two overlays that both set externalBin, and every build passes its OS\'s sidecars overlay', async () => {
  // Arrays replace when overlays merge (RFC 7396), so a second overlay with
  // externalBin would silently drop the first one's sidecars (heic-spec J29).
  const { readdirSync, readFileSync } = await import('node:fs');
  const { fileURLToPath } = await import('node:url');
  const app = fileURLToPath(new URL('../apps/desktop/src-tauri/', import.meta.url));
  const setsBins = (name) => JSON.parse(readFileSync(join(app, name), 'utf8'))?.bundle?.externalBin !== undefined;
  const builds = await workflowBuilds();
  for (const { file, line, overlays } of builds) {
    for (const name of overlays) assert.ok(readdirSync(app).includes(name), `${file}: ${name} doesn't exist`);
    assert.ok(overlays.filter(setsBins).length <= 1, `${file}: ${line.trim()}`);
  }
  const want = [
    ['ci.yml', 'tauri.pdfium.windows.conf.json', 'tauri.sidecars.windows.conf.json'],
    ['release.yml', 'tauri.pdfium.windows.conf.json', 'tauri.sidecars.windows.conf.json'],
    // The Mac build lives in macos-app.yml, which ci.yml and release.yml
    // both call (D45).
    ['macos-app.yml', 'tauri.pdfium.macos.conf.json', 'tauri.sidecars.macos.conf.json'],
  ];
  for (const [file, pdfium, sidecars] of want) {
    const found = builds.filter((b) => b.file === file && b.overlays.includes(pdfium));
    assert.ok(found.length > 0, `${file} has no build with ${pdfium}`);
    for (const b of found) assert.deepEqual(b.overlays, [pdfium, sidecars], `${file}: ${b.line.trim()}`);
  }
  // Every build that ships is one of those: none without its sidecars.
  for (const b of builds.filter((b) => ['ci.yml', 'release.yml', 'macos-app.yml'].includes(b.file))) {
    assert.equal(b.overlays.filter(setsBins).length, 1, `${b.file}: ${b.line.trim()}`);
  }
  const workflows = fileURLToPath(new URL('../.github/workflows/', import.meta.url));
  for (const file of ['ci.yml', 'release.yml']) {
    const yml = readFileSync(join(workflows, file), 'utf8');
    assert.match(yml, /^\s*uses: \.\/\.github\/workflows\/macos-app\.yml\s*$/m, `${file} doesn't build the Mac app`);
  }
});

test("the ffmpeg workflow's dist caches are keyed on everything the build and its source bundle come from", async () => {
  // A hit skips build.sh, and with it the notices check and the source
  // bundle, so a change to the script, the pins or the license texts must
  // miss the cache (D24).
  const { readFileSync } = await import('node:fs');
  const { fileURLToPath } = await import('node:url');
  const yml = readFileSync(fileURLToPath(new URL('../.github/workflows/ffmpeg.yml', import.meta.url)), 'utf8');
  const keys = [...yml.matchAll(/^\s*key: (ffmpeg-dist-.*)$/gm)].map((m) => m[1]);
  assert.equal(keys.length, 3, 'one dist cache key per target');
  for (const target of ['windows-x86_64', 'macos-arm64', 'macos-x86_64']) {
    const key = keys.find((k) => k.includes(`-${target}-`));
    assert.ok(key, `no dist cache key for ${target}`);
    assert.match(key, /hashFiles\('scripts\/ffmpeg\/\*\*', 'licenses\/ffmpeg\/\*\*'\)/, key);
  }
  const tarballs = [...yml.matchAll(/^\s*key: (ffmpeg-tarballs-.*)$/gm)].map((m) => m[1]);
  assert.equal(tarballs.length, 3, 'one tarball cache key per target');
  for (const key of tarballs) assert.match(key, /hashFiles\('scripts\/ffmpeg\/sources\.json'\)/, key);
});
