#!/usr/bin/env node
// The bundling gate for sidecars (DECISIONS D25). A build that bundles
// sidecars passes `--config src-tauri/tauri.sidecars.conf.json`, and that
// overlay runs this script first as its `beforeBuildCommand`. It refuses the
// build unless every sidecar in the overlay's `externalBin`:
//   - is in `apps/desktop/src-tauri/binaries/` for the target being built,
//   - is Grape's own build: its version line carries the `-grape` marker
//     (the prebuilt FFmpeg pins in `scripts/sidecars.lock` are for dev
//     testing only and never ship, D24),
//   - has its license texts in `licenses/<folder>/` (with `SOURCE.md` for
//     GPL code) and its section in `about.hbs` and `THIRD_PARTY_NOTICES.md`.
//
// Usage: node scripts/check-sidecars.mjs [--target <triple>]
// The target defaults to TAURI_ENV_TARGET_TRIPLE, which the Tauri CLI sets
// for `beforeBuildCommand`. Exit code 0 when every check passes, 1 when one
// fails (each failure printed on its own line), 2 on bad usage.

import { execFileSync } from 'node:child_process';
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

/** What each sidecar must answer and which license files must ship with it. */
export const RULES = {
  ffmpeg: {
    args: ['-hide_banner', '-version'],
    // `--extra-version=grape` in scripts/ffmpeg/build.sh: "ffmpeg version 9.0.2-grape …".
    line: /^ffmpeg version \S*-grape(?:\.\d+)?(?:\s|$)/,
    licenses: 'ffmpeg',
    gpl: true,
    notice: 'FFmpeg',
  },
  ffprobe: {
    args: ['-hide_banner', '-version'],
    line: /^ffprobe version \S*-grape(?:\.\d+)?(?:\s|$)/,
    licenses: 'ffmpeg',
    gpl: true,
    notice: 'FFmpeg',
  },
};

/** How long one `-version` run may take, in milliseconds. */
const RUN_TIMEOUT_MS = 20_000;

/** The file name a Tauri `externalBin` entry has in `binaries/` for `triple`. */
export function sidecarFile(base, triple) {
  return `${base}-${triple}${triple.includes('windows') ? '.exe' : ''}`;
}

/** The sidecar's name: the last part of its `externalBin` entry. */
export function sidecarName(entry) {
  const parts = entry.split('/');
  return parts[parts.length - 1];
}

/** The first line of a program's output, without its line ending. */
export function firstLine(output) {
  return output.split(/\r?\n/, 1)[0] ?? '';
}

/** True when `notices` has a level-two heading that starts with `name`. */
export function hasSection(notices, name) {
  const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return new RegExp(`^## ${escaped}(?:\\s|$)`, 'm').test(notices);
}

/**
 * Runs every check and returns the failures (empty when the build may go on).
 * `io` holds what the checks touch, so tests can fake it:
 * `exists(path)`, `isFile(path)`, `list(dir)`, `read(path)` and
 * `run(file, args)`, which returns stdout or throws.
 */
export function check({ repo, triple, io }) {
  const failures = [];
  const appDir = join(repo, 'apps', 'desktop', 'src-tauri');
  const overlayPath = join(appDir, 'tauri.sidecars.conf.json');
  let entries;
  try {
    entries = JSON.parse(io.read(overlayPath))?.bundle?.externalBin;
  } catch (err) {
    return [`Couldn't read tauri.sidecars.conf.json: ${err.message}`];
  }
  if (!Array.isArray(entries) || entries.length === 0) {
    return ['tauri.sidecars.conf.json lists no externalBin, so there is nothing to bundle.'];
  }
  const aboutPath = join(repo, 'about.hbs');
  const noticesPath = join(repo, 'THIRD_PARTY_NOTICES.md');
  const about = io.exists(aboutPath) ? io.read(aboutPath) : '';
  const notices = io.exists(noticesPath) ? io.read(noticesPath) : '';
  const checkedLicenses = new Set();

  for (const entry of entries) {
    const name = sidecarName(entry);
    const rule = RULES[name];
    if (!rule) {
      failures.push(`${name}: the gate has no rule for this sidecar. Add one to scripts/check-sidecars.mjs (D25).`);
      continue;
    }
    const file = join(appDir, sidecarFile(entry, triple));
    if (!io.isFile(file)) {
      failures.push(`${name}: src-tauri/${sidecarFile(entry, triple)} is missing. Build it, or take it from CI's build job (D24, D25).`);
    } else {
      let out;
      try {
        out = io.run(file, rule.args);
      } catch (err) {
        failures.push(`${name}: couldn't run it to read its version (${err.message.split('\n', 1)[0]}).`);
      }
      if (out !== undefined) {
        const line = firstLine(out);
        if (!rule.line.test(line)) {
          failures.push(`${name}: this isn't Grape's own build. Its version line is "${line}", without the -grape marker. The pins in scripts/sidecars.lock are for dev testing only and never ship (D24, D25).`);
        }
      }
    }
    if (!checkedLicenses.has(rule.licenses)) {
      checkedLicenses.add(rule.licenses);
      const dir = join(repo, 'licenses', rule.licenses);
      const files = io.exists(dir) ? io.list(dir) : [];
      if (files.length === 0) {
        failures.push(`${name}: licenses/${rule.licenses}/ is missing or empty (ENGINES.md licensing rule 3).`);
      } else if (rule.gpl && !files.includes('SOURCE.md')) {
        failures.push(`${name}: licenses/${rule.licenses}/SOURCE.md is missing. GPL code ships with where its exact source is (ENGINES.md licensing rule 3).`);
      }
      if (!hasSection(about, rule.notice)) {
        failures.push(`${name}: about.hbs has no "## ${rule.notice}" section (licenses/README.md).`);
      } else if (!hasSection(notices, rule.notice)) {
        failures.push(`${name}: THIRD_PARTY_NOTICES.md has no "## ${rule.notice}" section. Generate it again from about.hbs (D23).`);
      }
    }
  }
  return failures;
}

/** The real file system and process runner. */
export const realIo = {
  exists: (path) => existsSync(path),
  isFile: (path) => existsSync(path) && statSync(path).isFile(),
  list: (dir) => readdirSync(dir).filter((f) => statSync(join(dir, f)).isFile()),
  read: (path) => readFileSync(path, 'utf8'),
  run: (file, args) =>
    execFileSync(file, args, {
      encoding: 'utf8',
      timeout: RUN_TIMEOUT_MS,
      windowsHide: true,
      stdio: ['ignore', 'pipe', 'pipe'],
      maxBuffer: 1024 * 1024,
    }),
};

function main(argv) {
  let triple = process.env.TAURI_ENV_TARGET_TRIPLE;
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === '--target' && i + 1 < argv.length) {
      triple = argv[++i];
    } else {
      console.error(`check-sidecars: unknown argument ${argv[i]}. Usage: node scripts/check-sidecars.mjs [--target <triple>]`);
      return 2;
    }
  }
  if (!triple) {
    console.error('check-sidecars: no target. Run it through the Tauri CLI (it sets TAURI_ENV_TARGET_TRIPLE) or pass --target <triple>.');
    return 2;
  }
  const repo = resolve(dirname(fileURLToPath(import.meta.url)), '..');
  const failures = check({ repo, triple, io: realIo });
  if (failures.length > 0) {
    console.error(`check-sidecars: refusing to bundle sidecars for ${triple}:`);
    for (const failure of failures) console.error(`  - ${failure}`);
    return 1;
  }
  console.log(`check-sidecars: every sidecar for ${triple} is Grape's own build, with its licenses.`);
  return 0;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exitCode = main(process.argv.slice(2));
}
