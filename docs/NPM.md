# npm distribution

`@bitefoundry/teams` is distributed on npm as a small JavaScript launcher plus one
optional dependency per platform. The launcher resolves the prebuilt Rust binary
and execs it directly (with `scripts/postinstall.mjs` linking the native binary
into `bin/` and Windows shims for zero-Node-startup execution), so there is no
compiler, Node native addon, or network download during install.

The Rust crate is named `teams-cli` and produces the pure **Microsoft Teams**
binary (`teams` / `teams.exe`). The npm package is scoped as `@bitefoundry/teams`
and installs the **`teams`** command.

## Install and run

```sh
# one-off
npx @bitefoundry/teams --version

# global install (installs `teams`)
npm install -g @bitefoundry/teams
teams --version

# project-local
npm install @bitefoundry/teams
npx teams chats
```

All workflows share the same Entra ID login, SQLite WAL + FTS5 cache (`cache.db`),
background sync daemon (`teams daemon start | status | stop | restart`), HTTP API
(`teams serve`), MCP server (`teams mcp`), and multi-protocol terminal image
preview (`Kitty` / `iTerm2` / `Sixel` / `Halfblock` via `teams preview` and the TUI).
Read commands also support `--cached` / `--fast` for instant local cache reads,
or run with zero Graph round-trips while the daemon is warm:

- `teams login`, `teams whoami`, `teams doctor --probe`
- `teams chats --limit 200`, `teams teams`, `teams channels <team>`
- `teams read <target> --limit 100 --before 2026-11-01`, `teams thread <chan> <root_id>`
- `teams send <target> <message>`, `teams reply <target> <msg_id> <message>`
- `teams send-file`, `teams download`, `teams react`, `teams edit`, `teams delete`
- `teams sync --chats 100 --depth 100`, `teams search <query>`, `teams export <target>`
- `teams daemon <start|status|stop|restart>`, `teams tui`, `teams serve`, `teams mcp`, `teams update`

## Supported platforms

| npm package                   | OS      | CPU   | Rust target                         |
| ----------------------------- | ------- | ----- | ----------------------------------- |
| `@bitefoundry/teams-win32-x64`    | Windows | x64   | `x86_64-pc-windows-msvc`            |
| `@bitefoundry/teams-win32-arm64`  | Windows | arm64 | `aarch64-pc-windows-msvc`           |
| `@bitefoundry/teams-darwin-x64`   | macOS   | x64   | `x86_64-apple-darwin`               |
| `@bitefoundry/teams-darwin-arm64` | macOS   | arm64 | `aarch64-apple-darwin`              |
| `@bitefoundry/teams-linux-x64`    | Linux   | x64   | `x86_64-unknown-linux-gnu` (2.28)   |
| `@bitefoundry/teams-linux-arm64`  | Linux   | arm64 | `aarch64-unknown-linux-gnu` (2.28)  |

Linux packages are cross-compiled in CI with `cargo-zigbuild` against **GLIBC 2.28** (`*.2.28`) and verified with `scripts/verify-glibc.mjs` plus an `almalinux:8` container smoke test, so prebuilt binaries run out of the box on **AlmaLinux 8 / RHEL 8+ / Rocky Linux 8+ / Ubuntu 20.04+ / Debian 10+**. Older glibc distributions (e.g., CentOS 7 with `GLIBC < 2.28`) and Alpine/musl Linux should build from source with Cargo (`cargo build --release`).

## How resolution works

`bin/teams.js` looks up `@bitefoundry/teams-<platform>-<arch>` through
`require.resolve`, then executes
`node_modules/@bitefoundry/teams-<platform>-<arch>/bin/teams` (or `teams.exe` on
Windows) with `stdio: 'inherit'`. On `npm install`, `scripts/postinstall.mjs`
additionally replaces `bin/teams.js` (Unix) or rewrites `.bin/teams[.cmd|.ps1]`
(Windows) to invoke the native Rust executable directly without spawning Node.
Signals (`SIGINT`, `SIGTERM`, `SIGHUP`) are forwarded and the launcher exits
with the child's exit code. If the platform package is missing (for example
because optional dependencies were skipped), the launcher prints the supported
platforms and the `--include=optional` hint instead of a stack trace.

## Layout

```
package.json                  # @bitefoundry/teams launcher package (bin: teams)
bin/teams.js                  # teams launcher
scripts/postinstall.mjs       # zero-Node native binary linker on npm install
teams / teams.cmd             # repo-local Teams launchers
npm/teams-<platform>/         # one package per platform
  package.json                # os/cpu/files, same version as root
  bin/teams[.exe]             # added by scripts/build-npm.mjs, never committed
scripts/build-npm.mjs         # copy target/<triple>/release/teams[.exe]
scripts/sync-version.mjs      # Cargo.toml version -> all package.json files
scripts/verify-glibc.mjs      # verify Linux ELF GLIBC <= 2.28 symbol ceiling
.github/workflows/release.yml # tag-triggered build, publish, GitHub Release
```

Binaries are gitignored (`npm/*/bin/teams*`). A clean checkout has empty
platform `bin/` directories until a build or `npm run build:npm` runs.

## Versioning

`Cargo.toml` (`[package].version`) is the single source of truth. To bump:

```sh
# edit Cargo.toml [package].version
node scripts/sync-version.mjs
```

The script updates the root `package.json`, every `npm/teams-*/package.json`,
and the root `optionalDependencies` pins. It is idempotent; `--check` exits
non-zero if anything is stale. All packages must be published at the same
version because the root package pins its platform packages exactly.

Tag the release with a matching `v` prefix:

```sh
git tag v0.1.0
git push origin v0.1.0
```

## Publishing

Publishing is automated by `.github/workflows/release.yml`, which is triggered
by pushing a `v*` tag (or manually via `workflow_dispatch`).

1. Add a repository secret `NPM_TOKEN` (an npm automation token with publish
   rights for the `@bitefoundry/teams` packages).
2. Bump the version in `Cargo.toml`, run `node scripts/sync-version.mjs`,
   commit, and push `v<version>`.
3. The workflow builds six targets, stages binaries into the platform
   packages, uploads artifacts, publishes each platform package, publishes the
   root package, and creates a GitHub Release with the archives, `teams.exe`,
   and `SHA256SUMS.txt`.

The publish job verifies that the tag matches the package version and that all
six platform packages contain a binary before publishing anything. Platform
packages are published first so the root package's `optionalDependencies` can
resolve immediately.

Manual publish (matching order):

```sh
node scripts/sync-version.mjs --check
for dir in npm/teams-*; do npm publish "$dir" --access public; done
npm publish --access public
```

## Local validation

```sh
# stage the current host binary (Windows example)
node scripts/build-npm.mjs --target x86_64-pc-windows-msvc
node bin/teams.js --version

npm pack --dry-run
npm pack --dry-run ./npm/teams-win32-x64
```

`node scripts/build-npm.mjs` with no arguments uses the host target and falls
back to `target/release/` when a plain `cargo build --release` was used.
`--all` attempts every supported target and warns about the ones not built on
the current host.

## External token brokers & token scopes

When using an external Microsoft Graph token broker (`tc-auth`, `az account get-access-token`, Vault CLI, etc.), configure `token_file` / `token_refresh_cmd` instead of shell wrappers (avoids `cmd.exe` 1023-char `set /p` truncation and `.ps1`/`.cmd` shim issues). Setting `TEAMS_TOKEN_FILE` (`token_file`) or `TEAMS_ACCESS_TOKEN` automatically skips OS keyring reads.

### Required Microsoft Graph delegated scopes

Pure Teams requires only Microsoft Teams & Graph core scopes:

`offline_access`, `User.Read`, `User.ReadBasic.All`, `Chat.ReadWrite`, `Team.ReadBasic.All`, `Channel.ReadBasic.All`, `ChannelMessage.Read.All`, `ChannelMessage.Send`, `Files.ReadWrite.All`, `Presence.ReadWrite`

> **Important (`Chat.ReadBasic` limitation):** External tokens that only carry `Chat.ReadBasic` can list chats but **cannot** read message bodies or paginate message history (Graph returns `403 Forbidden`). Ensure your token includes `Chat.Read` or `Chat.ReadWrite` (and `ChannelMessage.Read.All` for team channels). Run `teams whoami` or `teams doctor` to inspect JWT `scp`/`roles` claims automatically.

### 1. `config.toml` (recommended persistent setup)

```toml
token_file = "~/.cursor/tc-auth/access_token"
token_refresh_cmd = "python ~/.cursor/tc-auth/graph-auth.py refresh"
token_refresh_mins = 5
```

### 2. Environment variables

```sh
export TEAMS_TOKEN_FILE="$HOME/.cursor/tc-auth/access_token"
export TEAMS_TOKEN_REFRESH_CMD="python $HOME/.cursor/tc-auth/graph-auth.py refresh"
export TEAMS_TOKEN_REFRESH_MINS=5
```

### 3. Azure CLI (`az account get-access-token`) JSON/stdout integration

If `token_file` is omitted, `token_refresh_cmd` stdout (raw JWT or Azure CLI camelCase JSON `accessToken` / `expiresOn`) is parsed and cached in memory:

```toml
token_refresh_cmd = "az account get-access-token --resource https://graph.microsoft.com --output json"
token_refresh_mins = 5
```

### Key environment variables

| Variable | Summary |
| --- | --- |
| `TEAMS_ACCESS_TOKEN` | Static Bearer token override (automatically skips OS keyring) |
| `TEAMS_TOKEN_FILE` | External token file path (raw JWT or JSON; skips OS keyring) |
| `TEAMS_TOKEN_REFRESH_CMD` | Shell command run to refresh expiring/`401` tokens |
| `TEAMS_TOKEN_REFRESH_MINS` | Refresh lead window in minutes before expiry (default `5`) |
| `TEAMS_DISABLE_KEYRING=1` | Disable OS keyring and use `credentials.json` only |
| `TEAMS_CLIENT_ID` | Override Entra ID application client ID |
| `TEAMS_TENANT_ID` | Override Entra ID tenant ID (default `common`) |
| `TEAMS_CONFIG_DIR` | Override config/credentials directory |
| `TEAMS_DATA_DIR` | Override SQLite cache (`cache.db`) directory |
| `TEAMS_NO_BROWSER` | Disable automatic browser launch during login (`1` / `true`) |
| `TEAMS_CLI_CACHE_SECS` | CLI read cache TTL in seconds (default `0`; `--cached` / `--fast` forces cache-first reads) |
| `TEAMS_AUTO_DAEMON` | Auto-start/attach to the background sync daemon (`1` / `true`, or `config.toml`: `auto_daemon = true`, default `false`) |
| `TEAMS_IMAGE_PROTOCOL` | Terminal image protocol override (`auto`, `kitty`, `iterm2`, `sixel`, `halfblock`) |

## Troubleshooting

- **`no prebuilt binary found for <platform>-<arch>`** — the optional
  dependency was skipped. Reinstall with
  `npm install -g @bitefoundry/teams --include=optional` or
  `npm install @bitefoundry/teams --include=optional`. Check that your npm config
  does not set `omit=optional`.
- **`EBADPLATFORM` / package skipped** — npm installs only the matching
  `os`/`cpu` package. Cross-installing (for example x64 Node on ARM Windows via
  emulation) still reports the Node architecture, so install under the native
  Node build.
- **Older Linux (`GLIBC < 2.28`, e.g. CentOS 7) or Alpine/musl Linux** — prebuilt Linux binaries require **GLIBC >= 2.28** (AlmaLinux 8 / RHEL 8+ / Rocky Linux 8+ / Ubuntu 20.04+ / Debian 10+). On older glibc or musl systems, build from source with Cargo (`cargo build --release`).
- **macOS Gatekeeper** — npm-installed binaries are not notarized. If macOS
  blocks execution, allow the binary in System Settings → Privacy & Security, or
  build from source.
- **Corporate proxy/firewall** — `npx @bitefoundry/teams` performs a registry
  download each time unless cached; use a global install for offline-friendly
  use.
- **Verify the binary** —
  `node -p "require.resolve('@bitefoundry/teams-win32-x64/package.json')"`
  (adjust for your platform) shows which platform package npm selected.
- **Tag push did not publish** — check the publish job log; a tag/version
  mismatch or a missing `NPM_TOKEN` fails the job before any publish.
