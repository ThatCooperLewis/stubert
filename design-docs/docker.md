# Docker

Stubert runs in a Docker container. There is no local development environment — all building, testing, and running happens through Docker.

## Development Model

The image contains the Rust toolchain and pre-compiled dependencies but **not** the application source. The host `src/` directory is mounted into the container at runtime. The entrypoint compiles from the mounted source on startup, so code changes only require a container restart — not an image rebuild.

**When to rebuild the image:** Only when `Cargo.toml` or `Cargo.lock` change (new/updated dependencies).

**When to restart the container:** Any change to files in `src/`.

## Image Build

The Dockerfile is a single-stage build that includes the Rust toolchain:

1. **Base image:** `rust:1.xx-slim-bookworm` (toolchain stays in the image)
2. **System dependencies:**
   - `curl` — health check
   - `ffmpeg` — audio format conversion (for whisper)
   - Node.js 20.x — Claude Code CLI runtime
3. **Claude Code CLI:** Installed globally via `npm install -g @anthropic-ai/claude-code`
4. **Dependency pre-build:** `Cargo.toml` and `Cargo.lock` are copied in and dependencies are compiled against a dummy `main.rs`. This means the expensive dependency compilation is cached in the image layer — only the project source (mounted at runtime) needs to compile on startup.
5. **Copy entrypoint script**

Source code is **not** copied into the image. It is mounted at runtime.

```dockerfile
FROM rust:1.xx-slim-bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
    curl ffmpeg gnupg \
    && curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*
RUN npm install -g @anthropic-ai/claude-code

WORKDIR /app

# Pre-build dependencies (cached in image layer)
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && cargo build --release --tests \
    && rm -rf src

COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

EXPOSE 8484
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -f http://localhost:8484/health || exit 1
ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["serve"]
```

## Entrypoint

The entrypoint script compiles from the mounted source, then dispatches based on the first argument:

```sh
#!/bin/sh
set -e

case "${1:-serve}" in
    serve)
        cargo build --release
        exec /app/target/release/stubert --runtime-dir /app/config
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

In `serve` mode, the entrypoint builds the project in release mode from the mounted `src/`, then exec's the resulting binary. Because dependencies are pre-compiled in the image, this only compiles the project source — typically a few seconds.

In `test` mode, `cargo test` handles its own compilation.

**Modes:**

| Command | What It Does |
|---------|-------------|
| `docker run stubert:local` | Start the service (default: `serve`) |
| `docker run stubert:local serve` | Start the service (explicit) |
| `docker run stubert:local test` | Run all tests |
| `docker run stubert:local test --test test_session` | Run specific test binary |
| `docker run stubert:local bash` | Interactive shell |

## Volumes

Four mount points are used at runtime:

| Host Path | Container Path | Purpose |
|-----------|---------------|---------|
| `./src` | `/app/src` | Live source code (compiled on container startup) |
| `./config` | `/app/config` | Runtime directory (config, memory files, history, logs, sessions) |
| `$HOME/.claude` | `/root/.claude` | Claude Code authentication token |
| `$HOME/.claude.json` | `/root/.claude.json` | Claude Code authentication metadata |

### Source Mount (`/app/src`)

The host `src/` directory is mounted into the container at `/app/src`, overlaying the dummy source used during the dependency pre-build. The entrypoint compiles from this mounted source on every startup. Because the image already contains pre-compiled dependencies in `/app/target/`, only the project source needs to compile — this is fast (a few seconds for incremental builds).

The `/app/target/` directory lives inside the container's writable layer. It persists across entrypoint compilation but is lost when the container is removed. This is fine — dependency artifacts are rebuilt from the image cache, and source compilation is fast.

### Runtime Directory (`/app/config`)

This is the working directory for the service. All relative paths in `config.yaml` resolve against `/app/config`:

```
/app/config/
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
# Build image (only needed when dependencies change)
docker build -t stubert:local .

# Run (compiles src/ on startup)
docker run --rm \
  -v ./src:/app/src \
  -v ./config:/app/config \
  -v "$HOME/.claude":/root/.claude \
  -v "$HOME/.claude.json":/root/.claude.json \
  stubert:local
```

After editing files in `src/`, stop the container and re-run the same `docker run` command. The entrypoint recompiles from the mounted source — no image rebuild needed.

### Test Mode

```bash
# All tests
docker run --rm -v ./src:/app/src stubert:local test

# Specific test
docker run --rm -v ./src:/app/src stubert:local test --test test_session

# With specific test name filter
docker run --rm -v ./src:/app/src stubert:local test -- --test-threads=1
```

For live integration tests (real Claude CLI calls):

```bash
docker run --rm \
  -v ./src:/app/src \
  -v "$HOME/.claude":/root/.claude \
  -v "$HOME/.claude.json":/root/.claude.json \
  stubert:local test --test live
```

## Networking

The health endpoint listens on port 8484. In production (NixOS), the container runs with `--network=host`, so no port mapping is needed — the health endpoint is accessible at `localhost:8484` on the host.

For development without `--network=host`:

```bash
docker run --rm -p 8484:8484 \
  -v ./src:/app/src \
  -v ./config:/app/config \
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

1. **Dependencies pre-compiled:** `Cargo.toml` and `Cargo.lock` are copied in and dependencies are built against a dummy `main.rs`. This layer is cached — it only rebuilds when `Cargo.toml` or `Cargo.lock` change.
2. **Dev dependencies included:** Both release and test dependencies are pre-compiled so the same image supports `serve` and `test` modes.
3. **Source not in image:** Because `src/` is mounted at runtime, source changes never invalidate any image layer. Only dependency changes require an image rebuild.

## Rootless Docker Note

On NixOS with rootless Docker, container root (UID 0) maps to the host user (UID 1000). This means:

- Files created by the container in mounted volumes are owned by the host user
- `chmod 0o000` inside the container doesn't actually restrict access (root permissions aren't real)
- No privilege escalation risk from running as root inside the container

This is why one test (`test_returns_none_on_permission_error` in the Python version) is a known failure when running as container root — permission restrictions don't apply.
