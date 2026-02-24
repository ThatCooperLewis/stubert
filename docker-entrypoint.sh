#!/bin/sh
set -e

case "${1:-serve}" in
    serve)
        # Touch source files so cargo detects changes from the
        # dummy sources used during the image's dependency-cache layer.
        find src -name '*.rs' -exec touch {} +
        cargo build --release
        exec /app/target/release/stubert --runtime-dir /data
        ;;
    test)
        shift
        find src -name '*.rs' -exec touch {} +
        exec cargo test "$@"
        ;;
    *)
        exec "$@"
        ;;
esac
