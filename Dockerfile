FROM rust:1.88-slim-bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl ffmpeg gnupg pkg-config \
    && curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*
RUN npm install -g @anthropic-ai/claude-code

WORKDIR /app

# Pre-build dependencies (cached in image layer)
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && cargo build --release \
    && cargo build --release --tests \
    && rm -rf src

COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

EXPOSE 8484
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -f http://localhost:8484/health || exit 1
ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["serve"]
