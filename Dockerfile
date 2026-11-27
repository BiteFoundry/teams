# Container image for @bitefoundry/teams (stdio MCP server, CLI, and TUI).
#
# Installs the verified Linux release binary (GLIBC <= 2.28) directly from
# GitHub Releases instead of compiling Rust from source, so builds complete in
# seconds and match the published release artifact.
#
# Examples:
#   docker run -i --rm \
#     -e TEAMS_CLIENT_ID=... \
#     -e TEAMS_TENANT_ID=... \
#     -e TEAMS_TOKEN_FILE=/data/token \
#     -v ~/.config/teams:/data/config \
#     ghcr.io/bitefoundry/teams:0.9.9
FROM debian:bookworm-slim

ARG TEAMS_VERSION=0.9.9
ARG TARGETARCH

LABEL org.opencontainers.image.title="teams" \
      org.opencontainers.image.description="Single Rust binary pairing a stdio MCP server, CLI, local HTTP API, and Ratatui TUI for Microsoft Teams over an embedded SQLite WAL + FTS5 cache." \
      org.opencontainers.image.source="https://github.com/BiteFoundry/teams" \
      org.opencontainers.image.documentation="https://github.com/BiteFoundry/teams#readme" \
      org.opencontainers.image.version="${TEAMS_VERSION}" \
      org.opencontainers.image.licenses="MIT" \
      io.modelcontextprotocol.server.name="io.github.BiteFoundry/teams"

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

RUN set -eu; \
    arch="${TARGETARCH:-$(dpkg --print-architecture)}"; \
    case "$arch" in \
      amd64) target="x86_64-unknown-linux-gnu" ;; \
      arm64) target="aarch64-unknown-linux-gnu" ;; \
      *) echo "Unsupported architecture: $arch" >&2; exit 1 ;; \
    esac; \
    base="https://github.com/BiteFoundry/teams/releases/download/v${TEAMS_VERSION}"; \
    archive="teams-${TEAMS_VERSION}-${target}.tar.gz"; \
    cd /tmp; \
    curl -fsSLO "${base}/${archive}"; \
    curl -fsSLO "${base}/SHA256SUMS.txt"; \
    grep -F "  ${archive}" SHA256SUMS.txt > expected.sha256; \
    test "$(wc -l < expected.sha256)" -eq 1; \
    sha256sum -c expected.sha256; \
    tar -xzf "${archive}" teams; \
    install -m 0755 teams /usr/local/bin/teams; \
    rm -f "/tmp/${archive}" /tmp/SHA256SUMS.txt /tmp/expected.sha256 /tmp/teams; \
    teams --version

ENV TEAMS_CONFIG_DIR=/data/config \
    TEAMS_DATA_DIR=/data/state

RUN mkdir -p /data/config /data/state \
 && chmod 0777 /data /data/config /data/state

WORKDIR /data

ENTRYPOINT ["teams"]
CMD ["mcp"]
