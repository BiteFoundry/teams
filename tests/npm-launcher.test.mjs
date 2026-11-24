import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { runPostinstall } from '../scripts/postinstall.mjs';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const POSTINSTALL_SRC = path.join(REPO_ROOT, 'scripts', 'postinstall.mjs');

function makeTempDir(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

function createMockInstallLayout(rootDir, { platform, arch, binaryContent = 'FAKE-NATIVE-BIN' }) {
  const scopeDir = path.join(rootDir, 'node_modules', '@bitefoundry');
  const pkgRoot = path.join(scopeDir, 'teams');
  const platPkgName = `teams-${platform}-${arch}`;
  const platPkgDir = path.join(scopeDir, platPkgName);
  const binaryName = platform === 'win32' ? 'teams.exe' : 'teams';

  fs.mkdirSync(path.join(pkgRoot, 'bin'), { recursive: true });
  fs.mkdirSync(path.join(pkgRoot, 'scripts'), { recursive: true });
  fs.mkdirSync(path.join(platPkgDir, 'bin'), { recursive: true });

  fs.writeFileSync(
    path.join(pkgRoot, 'package.json'),
    JSON.stringify({ name: '@bitefoundry/teams', version: '0.9.2', type: 'module' }),
    'utf8',
  );
  fs.writeFileSync(
    path.join(pkgRoot, 'bin', 'teams.js'),
    '#!/usr/bin/env node\nconsole.log("fallback launcher");\n',
    'utf8',
  );
  fs.copyFileSync(POSTINSTALL_SRC, path.join(pkgRoot, 'scripts', 'postinstall.mjs'));

  fs.writeFileSync(
    path.join(platPkgDir, 'package.json'),
    JSON.stringify({ name: `@bitefoundry/${platPkgName}`, version: '0.9.2' }),
    'utf8',
  );
  fs.writeFileSync(path.join(platPkgDir, 'bin', binaryName), binaryContent, 'utf8');

  return { pkgRoot, platPkgDir, binaryName };
}

test('runPostinstall skips git/Cargo development repository', () => {
  const result = runPostinstall({ pkgRoot: REPO_ROOT });
  assert.deepEqual(result, { skipped: true, reason: 'dev_repo' });
});

test('runPostinstall links native binary on unix layout', () => {
  const tmp = makeTempDir('teams-postinstall-unix-');
  try {
    const { pkgRoot } = createMockInstallLayout(tmp, {
      platform: 'linux',
      arch: 'x64',
      binaryContent: 'ELF-FAKE-TEAMS',
    });

    const result = runPostinstall({ pkgRoot, platform: 'linux', arch: 'x64', env: {} });
    assert.deepEqual(result, { skipped: false, platform: 'linux', linked: 1 });
    assert.equal(fs.readFileSync(path.join(pkgRoot, 'bin', 'teams.js'), 'utf8'), 'ELF-FAKE-TEAMS');
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test('runPostinstall copies/links exe and rewrites Windows shims', () => {
  const tmp = makeTempDir('teams-postinstall-win-');
  try {
    const { pkgRoot } = createMockInstallLayout(tmp, {
      platform: 'win32',
      arch: 'x64',
      binaryContent: 'MZ-FAKE-TEAMS',
    });

    const binShimDir = path.join(tmp, 'node_modules', '.bin');
    fs.mkdirSync(binShimDir, { recursive: true });
    const cmdPath = path.join(binShimDir, 'teams.cmd');
    const ps1Path = path.join(binShimDir, 'teams.ps1');
    const shPath = path.join(binShimDir, 'teams');
    fs.writeFileSync(
      cmdPath,
      '@ECHO off\r\nnode "%~dp0\\..\\@bitefoundry\\teams\\bin\\teams.js" %*\r\n',
      'utf8',
    );
    fs.writeFileSync(
      ps1Path,
      '& node "$basedir/../@bitefoundry/teams/bin/teams.js" $args\n',
      'utf8',
    );
    fs.writeFileSync(
      shPath,
      '#!/bin/sh\nexec node "$basedir/../@bitefoundry/teams/bin/teams.js" "$@"\n',
      'utf8',
    );

    const result = runPostinstall({ pkgRoot, platform: 'win32', arch: 'x64', env: {} });
    assert.equal(result.skipped, false);
    assert.equal(result.linked, 1);
    assert.equal(result.shimsRewritten, 3);
    assert.equal(fs.readFileSync(path.join(pkgRoot, 'bin', 'teams.exe'), 'utf8'), 'MZ-FAKE-TEAMS');
    assert.match(fs.readFileSync(cmdPath, 'utf8'), /teams\.exe/);
    assert.match(fs.readFileSync(ps1Path, 'utf8'), /teams\.exe/);
    assert.match(fs.readFileSync(shPath, 'utf8'), /teams\.exe/);
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test('postinstall.mjs executes main entrypoint when invoked through a symlinked prefix', (t) => {
  const tmp = makeTempDir('teams-postinstall-symlink-');
  try {
    const realPrefix = path.join(tmp, 'real-prefix');
    const linkPrefix = path.join(tmp, 'link-prefix');
    const expectedBinary = `NATIVE-${process.platform}-${process.arch}`;

    const { pkgRoot } = createMockInstallLayout(realPrefix, {
      platform: process.platform,
      arch: process.arch,
      binaryContent: expectedBinary,
    });

    try {
      fs.symlinkSync(realPrefix, linkPrefix, process.platform === 'win32' ? 'dir' : undefined);
    } catch (err) {
      if (process.platform === 'win32' && err && err.code === 'EPERM') {
        try {
          fs.symlinkSync(realPrefix, linkPrefix, 'junction');
        } catch (jErr) {
          if (jErr && jErr.code === 'EPERM') {
            t.skip('Windows unprivileged symlink/junction unavailable (EPERM)');
            return;
          }
          throw jErr;
        }
      } else {
        throw err;
      }
    }

    const symlinkScript = path.join(
      linkPrefix,
      'node_modules',
      '@bitefoundry',
      'teams',
      'scripts',
      'postinstall.mjs',
    );
    execFileSync(process.execPath, [symlinkScript], {
      cwd: path.dirname(path.dirname(symlinkScript)),
      stdio: 'pipe',
    });

    if (process.platform === 'win32') {
      assert.equal(
        fs.readFileSync(path.join(pkgRoot, 'bin', 'teams.exe'), 'utf8'),
        expectedBinary,
      );
    } else {
      assert.equal(
        fs.readFileSync(path.join(pkgRoot, 'bin', 'teams.js'), 'utf8'),
        expectedBinary,
      );
    }
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});
