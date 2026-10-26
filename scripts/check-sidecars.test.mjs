// Tests for the sidecar bundling gate (D25). Run: node --test scripts/check-sidecars.test.mjs
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { join } from 'node:path';

import { check, firstLine, hasSection, sidecarFile, sidecarName } from './check-sidecars.mjs';

const REPO = '/repo';
const APP = join(REPO, 'apps', 'desktop', 'src-tauri');
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
      [join(APP, 'tauri.sidecars.conf.json')]: OVERLAY,
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
      [join(APP, 'tauri.sidecars.conf.json')]: OVERLAY,
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
  const overlay = join(APP, 'tauri.sidecars.conf.json');
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
  assert.match(failures[0], /^Couldn't read tauri\.sidecars\.conf\.json/);
});

test('the sidecars overlay runs this gate before the UI build of the base config', async () => {
  const { readFileSync } = await import('node:fs');
  const { fileURLToPath } = await import('node:url');
  const app = fileURLToPath(new URL('../apps/desktop/src-tauri/', import.meta.url));
  const overlay = JSON.parse(readFileSync(join(app, 'tauri.sidecars.conf.json'), 'utf8'));
  const base = JSON.parse(readFileSync(join(app, 'tauri.conf.json'), 'utf8'));
  // The Tauri CLI runs it in apps/desktop (the folder with package.json), with
  // cmd /S /C on Windows and sh -c elsewhere; both run the second part only
  // when the gate exits 0.
  assert.equal(overlay.build.beforeBuildCommand, `node ../../scripts/check-sidecars.mjs && ${base.build.beforeBuildCommand}`);
  assert.deepEqual(Object.keys(overlay), ['build', 'bundle']);
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
      [join(APP, 'tauri.sidecars.conf.json')]: OVERLAY_ALL,
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
  // tauri.sidecars.conf.json: ffmpeg, ffprobe and bsdtar at least.
  assert.ok(checked >= 3, `only ${checked} externalBin entries found`);
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
