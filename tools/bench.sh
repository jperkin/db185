#!/bin/bash
# Single canonical entry point for db185 vs libnbcompat benchmarks.
#
# Builds:
#   - /tmp/c-rebuild      libnbcompat-only pkg_admin rebuild equivalent
#   - /tmp/c-replay       libnbcompat-only trace replay
#   - pkgsrc-rs release examples (rebuild + replay)
#
# Inputs:
#   PKGDB_DIR (default /opt/pkg/.pkgdb) - pkgdb directory to rebuild from
#   TRACE     (default /tmp/db185-bench.trace) - captured PUT/DEL trace
#
# Workloads (matches the README table):
#   1. dump      - read + iterate pkgdb.byfile.db
#   2. replay    - writer only: replay captured trace
#   3. rebuild   - end-to-end: walk pkgdb + write pkgdb.byfile.db
#
# Usage: tools/bench.sh [dump|replay|rebuild|all]   (default: all)

set -euo pipefail

DB185_DIR="$(cd "$(dirname "$0")/.." && pwd)"
PKGSRC_RS_DIR="${PKGSRC_RS_DIR:-/work/git/pkgsrc-rs}"
PKGDB_DIR="${PKGDB_DIR:-/opt/pkg/.pkgdb}"
TRACE="${TRACE:-/tmp/db185-bench.trace}"
PKGDB_FILE="$PKGDB_DIR/pkgdb.byfile.db"

C_REBUILD=/tmp/c-rebuild
C_REPLAY=/tmp/c-replay
RUST_REBUILD="$PKGSRC_RS_DIR/target/release/examples/pkgdb-byfile-rebuild"
RUST_REPLAY="$PKGSRC_RS_DIR/target/release/examples/pkgdb-byfile-replay"
RUST_TRACE="$PKGSRC_RS_DIR/target/release/examples/pkgdb-byfile-trace"
RUST_DUMP="$DB185_DIR/target/release/examples/dump"

NBCOMPAT_INC=/opt/pkg/include
NBCOMPAT_LIB=/opt/pkg/lib/libnbcompat.a

build() {
    echo "==> building C tools"
    clang -O2 -I"$NBCOMPAT_INC" -o "$C_REBUILD" "$DB185_DIR/tools/c-rebuild.c" "$NBCOMPAT_LIB"
    clang -O2 -I"$NBCOMPAT_INC" -o "$C_REPLAY"  "$DB185_DIR/tools/c-replay.c"  "$NBCOMPAT_LIB"

    echo "==> building Rust binaries"
    (cd "$DB185_DIR"     && cargo build --release --example dump)
    (cd "$PKGSRC_RS_DIR" && cargo build --release \
        --example pkgdb-byfile-rebuild \
        --example pkgdb-byfile-replay \
        --example pkgdb-byfile-trace)
}

ensure_trace() {
    if [ ! -f "$TRACE" ]; then
        echo "==> capturing trace from $PKGDB_DIR -> $TRACE"
        "$RUST_TRACE" "$PKGDB_DIR" > "$TRACE"
    fi
}

bench_dump() {
    echo
    echo "### dump (read + iterate) ###"
    hyperfine --warmup 3 --shell=none \
        --command-name "C   pkg_admin dump"  "pkg_admin dump" \
        --command-name "Rust db185 dump"     "$RUST_DUMP $PKGDB_FILE"
}

bench_replay() {
    echo
    echo "### replay (writer only) ###"
    ensure_trace
    hyperfine --warmup 3 --shell=none \
        --prepare "rm -f /tmp/bench-c.db /tmp/bench-rust.db" \
        --command-name "C   c-replay"   "$C_REPLAY  $TRACE /tmp/bench-c.db" \
        --command-name "Rust db185"     "$RUST_REPLAY $TRACE /tmp/bench-rust.db"
}

bench_rebuild() {
    echo
    echo "### end-to-end rebuild ###"
    hyperfine --warmup 3 --shell=none \
        --prepare "rm -f /tmp/bench-c.db /tmp/bench-rust.db" \
        --command-name "C   c-rebuild"  "$C_REBUILD $PKGDB_DIR /tmp/bench-c.db" \
        --command-name "Rust db185"     "$RUST_REBUILD $PKGDB_DIR -o /tmp/bench-rust.db"
}

cmd="${1:-all}"
build
case "$cmd" in
    dump)    bench_dump ;;
    replay)  bench_replay ;;
    rebuild) bench_rebuild ;;
    all)     bench_dump; bench_replay; bench_rebuild ;;
    *)       echo "usage: $0 [dump|replay|rebuild|all]" >&2; exit 1 ;;
esac
