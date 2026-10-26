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
