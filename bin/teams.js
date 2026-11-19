#!/usr/bin/env node
'use strict';

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');

const SUPPORTED = [
  'win32-x64',
  'win32-arm64',
  'darwin-x64',
  'darwin-arm64',
  'linux-x64',
  'linux-arm64',
];

function fail(message) {
  process.stderr.write(`teams: ${message}\n`);
  process.exit(1);
}

function formatLinuxGlibcHint() {
  return [
    'Prebuilt Linux binaries target GLIBC >= 2.28 (AlmaLinux 8 / RHEL 8 / Rocky Linux 8 / Ubuntu 20.04+ / Debian 10+).',
    'If you are on an older glibc distribution (such as CentOS 7 with GLIBC < 2.28) or Alpine/musl,',
    'build from source with Cargo instead:',
    '  cargo install --git https://github.com/BiteFoundry/teams --locked',
    'or clone the repository and run:',
    '  cargo build --release',
  ].join('\n');
}

const platform = process.platform;
const arch = process.arch;
const platformKey = `${platform}-${arch}`;
const packageName = `@bitefoundry/teams-${platformKey}`;
const binaryName = platform === 'win32' ? 'teams.exe' : 'teams';

let packageDir = null;
try {
  packageDir = path.dirname(require.resolve(`${packageName}/package.json`));
} catch {
  packageDir = null;
}

const binaryPath = packageDir ? path.join(packageDir, 'bin', binaryName) : null;

if (!binaryPath || !fs.existsSync(binaryPath)) {
  const lines = [
    `no prebuilt binary found for ${platformKey}.`,
    '',
    `Expected the optional dependency "${packageName}" to provide:`,
    `  ${binaryPath || `node_modules/${packageName}/bin/${binaryName}`}`,
    '',
    `Supported platforms: ${SUPPORTED.join(', ')}`,
    '',
    'Try one of:',
    '  npm install -g @bitefoundry/teams --include=optional',
    '  npm install @bitefoundry/teams --include=optional',
    '  npx --yes @bitefoundry/teams',
    '',
    'If you are on an unsupported platform (for example Alpine/musl Linux),',
    'build from source with Cargo:',
    '  cargo build --release',
  ];
  fail(lines.join('\n'));
}

// npm does not guarantee the executable bit survives packaging, so make sure
// the binary is runnable before spawning it (only chmod when X_OK is missing).
if (platform !== 'win32') {
  try {
    fs.accessSync(binaryPath, fs.constants.X_OK);
  } catch {
    try {
      fs.chmodSync(binaryPath, 0o755);
    } catch {
      // Read-only install; spawn will report EACCES if the bit is missing.
    }
  }
}

const child = spawn(binaryPath, process.argv.slice(2), {
  stdio: 'inherit',
  windowsHide: false,
});

child.on('error', (err) => {
  if (
    platform === 'linux' &&
    (err.code === 'ENOENT' || /GLIBC_/i.test(String(err.message || '')))
  ) {
    fail(
      `failed to launch ${binaryPath}: ${err.message}\n\n${formatLinuxGlibcHint()}`,
    );
  }
  fail(`failed to launch ${binaryPath}: ${err.message}`);
});

// Forward termination signals to the child process.
for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
  const handler = () => {
    if (!child.killed) {
      try {
        child.kill(signal);
      } catch {
        // Child already exited.
      }
    }
  };
  try {
    process.on(signal, handler);
  } catch {
    // Signal not supported on this platform.
  }
}

child.on('exit', (code, signal) => {
  if (signal) {
    const number = os.constants.signals[signal] || 1;
    process.exit(128 + number);
  }
  if (platform === 'linux' && code === 127) {
    process.stderr.write(
      `teams: binary exited with code 127 (missing shared library, dynamic linker, or GLIBC symbol version).\n${formatLinuxGlibcHint()}\n`,
    );
  }
  process.exit(code === null ? 1 : code);
});
