#!/bin/sh
set -e

case "${1:-serve}" in
    serve)
        cargo build --release
        exec /app/target/release/stubert --runtime-dir /data
        ;;
    test)
        shift
        exec cargo test "$@"
        ;;
    *)
        exec "$@"
        ;;
esac
