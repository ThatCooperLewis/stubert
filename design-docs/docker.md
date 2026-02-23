# Docker

Stubert runs in a Docker container. There is no local development environment — all building, testing, and running happens through Docker.

## Image Build

The Dockerfile is a single-stage build:

1. **Base image:** Rust (version TBD — likely `rust:1.xx-slim-bookworm` for build, `debian:bookworm-slim` for runtime if using multi-stage)
2. **System dependencies:**
   - `curl` — health check
   - `ffmpeg` — audio format conversion (for whisper)
   - Node.js 20.x — Claude Code CLI runtime
3. **Claude Code CLI:** Installed globally via `npm install -g @anthropic-ai/claude-code`
4. **Rust build:** Compile the project in release mode
5. **Copy entrypoint script**

For the Rust rewrite, consider a multi-stage build:

```dockerfile
# Stage 1: Build
FROM rust:1.xx-slim-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release

# Stage 2: Runtime
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl ffmpeg gnupg \
    && curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*
RUN npm install -g @anthropic-ai/claude-code

COPY --from=builder /app/target/release/stubert /usr/local/bin/stubert
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

EXPOSE 8484
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -f http://localhost:8484/health || exit 1
ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["serve"]
```

## Entrypoint

The entrypoint script dispatches based on the first argument:

```sh
#!/bin/sh
set -e

case "${1:-serve}" in
    serve)
        exec stubert --runtime-dir /data
        ;;
    test)
        shift
        exec cargo test "$@"
        ;;
    *)
        exec "$@"
        ;;
esac
```

**Modes:**

| Command | What It Does |
|---------|-------------|
| `docker run stubert:local` | Start the service (default: `serve`) |
| `docker run stubert:local serve` | Start the service (explicit) |
| `docker run stubert:local test` | Run all tests |
| `docker run stubert:local test --test test_session` | Run specific test binary |
| `docker run stubert:local bash` | Interactive shell |

## Volumes

Three mount points are used at runtime:

| Host Path | Container Path | Purpose |
|-----------|---------------|---------|
| `./config` | `/data` | Runtime directory (config, memory files, history, logs, sessions) |
| `$HOME/.claude` | `/root/.claude` | Claude Code authentication token |
| `$HOME/.claude.json` | `/root/.claude.json` | Claude Code authentication metadata |

### Runtime Directory (`/data`)

This is the working directory for the service. All relative paths in `config.yaml` resolve against `/data`:

```
/data/
├── config.yaml
├── .env
├── CLAUDE.md
├── SOUL.md
├── USER.md
├── MEMORY.md
├── HEARTBEAT.md
├── PUBLIC.md           # Optional
├── schedules.yaml
├── sessions.json
├── .claude/
│   └── skills/
├── submitted-files/
├── history/
└── logs/
```

### Claude Code Auth

The Claude Code CLI authenticates via files in `$HOME/.claude/` and `$HOME/.claude.json`. These must be mounted into the container at `/root/` because:

- The container runs as root (UID 0)
- With rootless Docker (NixOS default), container root maps to the host user — no privilege escalation
- The Claude CLI looks for auth at `$HOME/.claude` inside the container, and `HOME=/root` for the root user

## Health Check

Docker's built-in health check monitors the service:

```dockerfile
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -f http://localhost:8484/health || exit 1
```

The `/health` endpoint returns JSON with status, uptime, active sessions, and recent execution times (see [gateway.md](gateway.md)).

## Running

### Service Mode

```bash
docker build -t stubert:local .

docker run --rm \
  -v ./config:/data \
  -v "$HOME/.claude":/root/.claude \
  -v "$HOME/.claude.json":/root/.claude.json \
  stubert:local
```

### Test Mode

```bash
# All tests
docker run --rm stubert:local test

# Specific test
docker run --rm stubert:local test --test test_session

# With specific test name filter
docker run --rm stubert:local test -- --test-threads=1
```

For live integration tests (real Claude CLI calls):

```bash
docker run --rm \
  -v "$HOME/.claude":/root/.claude \
  -v "$HOME/.claude.json":/root/.claude.json \
  stubert:local test --test live
```

## Networking

The health endpoint listens on port 8484. In production (NixOS), the container runs with `--network=host`, so no port mapping is needed — the health endpoint is accessible at `localhost:8484` on the host.

For development without `--network=host`:

```bash
docker run --rm -p 8484:8484 \
  -v ./config:/data \
  -v "$HOME/.claude":/root/.claude \
  -v "$HOME/.claude.json":/root/.claude.json \
  stubert:local
```

## NixOS Deployment

The production deployment is defined in a NixOS container module:

- **Service name:** `docker-stubert.service`
- **Network:** `--network=host` (no port mapping needed)
- **Restart policy:** Managed by systemd (on-failure restart)
- **Auth mounts:** Same as development (host `$HOME/.claude` → container `/root/.claude`)
- **Rootless Docker:** Host UID 1000 maps to container UID 0 (root inside container = unprivileged host user)

## Build Caching

The Dockerfile is structured for layer caching:

1. **Dependencies first:** `Cargo.toml` and `Cargo.lock` are copied and dependencies built before source code. This means dependency changes (rare) bust the cache, but source changes (frequent) only rebuild the project itself.
2. **Dev dependencies included:** Test dependencies are included in the image so the same image can run both the service and tests.

## Rootless Docker Note

On NixOS with rootless Docker, container root (UID 0) maps to the host user (UID 1000). This means:

- Files created by the container in mounted volumes are owned by the host user
- `chmod 0o000` inside the container doesn't actually restrict access (root permissions aren't real)
- No privilege escalation risk from running as root inside the container

This is why one test (`test_returns_none_on_permission_error` in the Python version) is a known failure when running as container root — permission restrictions don't apply.
