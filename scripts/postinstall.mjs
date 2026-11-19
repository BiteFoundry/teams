#!/usr/bin/env node
// Zero-Node-Process npm native binary linker.
//
// On Unix (linux/darwin): replaces installed `bin/teams.js` with
// hardlinks/symlinks directly to `@bitefoundry/teams-<platform>-<arch>/bin/teams` so npm's
// bin symlink execve's the native Rust binary directly (0ms Node startup, 0MB Node RSS).
//
// On Windows (win32): creates an NTFS hardlink (fallback copy) `bin/teams.exe`
// and rewrites npm's `.bin/teams[.cmd|.ps1]` shims when present to invoke
// `teams.exe` directly without spawning `node.exe`.

import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const BIN_NAMES = ['teams'];
const PKG_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

function linkOrCopyFile(source, destination) {
  const tmp = `${destination}.tmp-${process.pid}`;
  try {
    fs.rmSync(tmp, { force: true });
  } catch {
    // Ignore cleanup errors on nonexistent temp paths.
  }
  try {
    fs.linkSync(source, tmp);
  } catch {
    fs.copyFileSync(source, tmp);
  }
  try {
    fs.renameSync(tmp, destination);
  } catch {
    try {
      fs.rmSync(destination, { force: true });
    } catch {
      // Ignore; renameSync below will surface or fail safely.
    }
    fs.renameSync(tmp, destination);
  }
}

function replaceJsWithNativeLinkUnix(binaryPath, destinationJs) {
  const tmp = `${destinationJs}.tmp-${process.pid}`;
  try {
    fs.rmSync(tmp, { force: true });
  } catch {
    // Ignore cleanup errors.
  }
  try {
    fs.linkSync(binaryPath, tmp);
  } catch {
    try {
      fs.rmSync(tmp, { force: true });
    } catch {
      // Ignore cleanup errors.
    }
    fs.symlinkSync(binaryPath, tmp);
  }
  try {
    fs.renameSync(tmp, destinationJs);
  } catch (err) {
    try {
      fs.rmSync(tmp, { force: true });
    } catch {
      // Ignore cleanup errors.
    }
    throw err;
  }
}

function collectWindowsShimDirs(pkgRoot, env = process.env) {
  const dirs = [];
  const parentScope = path.dirname(pkgRoot);
  const nodeModulesDir = path.dirname(parentScope);
  if (
    path.basename(parentScope) === '@bitefoundry' &&
    path.basename(nodeModulesDir) === 'node_modules'
  ) {
    dirs.push(path.join(nodeModulesDir, '.bin'));
    // On global Windows npm installs (`%APPDATA%\npm\node_modules\@bitefoundry\teams`),
    // shims sit directly in the parent of `node_modules` even when `npm_config_prefix` is unset in npm v9+.
    dirs.push(path.dirname(nodeModulesDir));
  }

  if (env.npm_config_global === 'true' && env.npm_config_prefix) {
    const prefix = path.resolve(env.npm_config_prefix);
    dirs.push(prefix);
    dirs.push(path.join(prefix, 'bin'));
  }

  return [...new Set(dirs)];
}

function shimReferencesLauncher(content, name) {
  const normalized = content.replace(/\\/g, '/').toLowerCase();
  return (
    normalized.includes('@bitefoundry/teams') &&
    normalized.includes(`bin/${name}.js`)
  );
}

function rewriteWindowsShims(shimDir, pkgRoot) {
  if (!fs.existsSync(shimDir)) return 0;
  let rewritten = 0;

  for (const name of BIN_NAMES) {
    const exePath = path.join(pkgRoot, 'bin', `${name}.exe`);
    if (!fs.existsSync(exePath)) continue;

    const relWin = path.relative(shimDir, exePath).replace(/\//g, '\\');
    const relPosix = path.relative(shimDir, exePath).replace(/\\/g, '/');

    // 1. `<name>.cmd`
    const cmdPath = path.join(shimDir, `${name}.cmd`);
    try {
      if (fs.existsSync(cmdPath)) {
        const content = fs.readFileSync(cmdPath, 'utf8');
        if (shimReferencesLauncher(content, name)) {
          fs.writeFileSync(cmdPath, `@ECHO off\r\n"%~dp0${relWin}" %*\r\n`, 'utf8');
          rewritten += 1;
        }
      }
    } catch {
      // Fall back cleanly to bin/<name>.js if shim is read-only or locked.
    }

    // 2. `<name>.ps1`
    const ps1Path = path.join(shimDir, `${name}.ps1`);
    try {
      if (fs.existsSync(ps1Path)) {
        const content = fs.readFileSync(ps1Path, 'utf8');
        if (shimReferencesLauncher(content, name)) {
          const ps1Body = [
            '#!/usr/bin/env pwsh',
            '$basedir=Split-Path $MyInvocation.MyCommand.Definition -Parent',
            `$exePath = Join-Path $basedir '${relWin.replace(/'/g, "''")}'`,
            'if ($MyInvocation.ExpectingInput) {',
            '  $input | & $exePath @args',
            '} else {',
            '  & $exePath @args',
            '}',
            'exit $LASTEXITCODE',
            '',
          ].join('\n');
          fs.writeFileSync(ps1Path, ps1Body, 'utf8');
          rewritten += 1;
        }
      }
    } catch {
      // Fall back cleanly.
    }

    // 3. `<name>` (Git Bash / MSYS2 POSIX shell shim)
    const shPath = path.join(shimDir, name);
    try {
      if (fs.existsSync(shPath)) {
        const content = fs.readFileSync(shPath, 'utf8');
        if (shimReferencesLauncher(content, name)) {
          const shBody = [
            '#!/bin/sh',
            'basedir=$(dirname "$(echo "$0" | sed -e \'s,\\\\,/,g\')")',
            `exec "$basedir/${relPosix}" "$@"`,
            '',
          ].join('\n');
          fs.writeFileSync(shPath, shBody, 'utf8');
          rewritten += 1;
        }
      }
    } catch {
      // Fall back cleanly.
    }
  }

  return rewritten;
}

export function runPostinstall(options = {}) {
  const pkgRoot = path.resolve(options.pkgRoot || PKG_ROOT);
  const platform = options.platform || process.platform;
  const arch = options.arch || process.arch;
  const env = options.env || process.env;

  // CRITICAL SAFETY GUARD: Never mutate files inside the git development repository.
  if (
    fs.existsSync(path.join(pkgRoot, 'Cargo.toml')) ||
    fs.existsSync(path.join(pkgRoot, '.git'))
  ) {
    return { skipped: true, reason: 'dev_repo' };
  }

  const platformKey = `${platform}-${arch}`;
  const packageName = `@bitefoundry/teams-${platformKey}`;
  const binaryName = platform === 'win32' ? 'teams.exe' : 'teams';

  let packageDir = null;
  try {
    const requireFromPkg = createRequire(path.join(pkgRoot, 'package.json'));
    packageDir = path.dirname(requireFromPkg.resolve(`${packageName}/package.json`));
  } catch {
    packageDir = null;
  }

  const binaryPath = packageDir ? path.join(packageDir, 'bin', binaryName) : null;
  if (!binaryPath || !fs.existsSync(binaryPath)) {
    return { skipped: true, reason: 'missing_platform_binary' };
  }

  const binDir = path.join(pkgRoot, 'bin');
  if (!fs.existsSync(binDir)) {
    return { skipped: true, reason: 'missing_bin_dir' };
  }

  if (platform === 'win32') {
    let linked = 0;
    for (const name of BIN_NAMES) {
      const destExe = path.join(binDir, `${name}.exe`);
      try {
        linkOrCopyFile(binaryPath, destExe);
        linked += 1;
      } catch {
        // Leave fallback JS launcher untouched.
      }
    }

    let shimsRewritten = 0;
    for (const shimDir of collectWindowsShimDirs(pkgRoot, env)) {
      try {
        shimsRewritten += rewriteWindowsShims(shimDir, pkgRoot);
      } catch {
        // Never fail npm install on read-only or custom shims.
      }
    }
    return { skipped: false, platform, linked, shimsRewritten };
  }

  // Unix (linux, darwin): chmod once and replace bin/*.js with hardlink/symlink to native binary.
  try {
    fs.chmodSync(binaryPath, 0o755);
  } catch {
    // Ignore on read-only stores.
  }

  let linked = 0;
  for (const name of BIN_NAMES) {
    const destJs = path.join(binDir, `${name}.js`);
    try {
      replaceJsWithNativeLinkUnix(binaryPath, destJs);
      linked += 1;
    } catch {
      // Leave fallback JS launcher intact.
    }
  }

  return { skipped: false, platform, linked };
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    runPostinstall();
  } catch {
    // Postinstall optimization must never fail npm install.
  }
  process.exit(0);
}
