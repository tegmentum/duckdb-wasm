#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WIT_ROOT="$REPO_ROOT/wit"
CORE_DIR="$REPO_ROOT/core/wit"

FORCE=0
for arg in "$@"; do
  case "$arg" in
    --force|-f)
      FORCE=1
      ;;
    --help|-h)
      cat <<EOF
Usage: sync-core-wit.sh [--force]

Copies wit/ into core/wit/, destructively replacing the target.

The sync refuses to run if the source's declared duckdb:extension
version is lower than the target's — this is the guard that prevents a
regressed wit/ tree from silently rolling core/wit/ back to an older
shape (see 7e4b0a8 for the incident). Pass --force to bypass.
EOF
      exit 0
      ;;
    *)
      echo "sync-core-wit.sh: unknown argument: $arg" >&2
      echo "Usage: sync-core-wit.sh [--force]" >&2
      exit 2
      ;;
  esac
done

# Extract every `duckdb:extension/<iface>@X.Y.Z` version referenced by a WIT
# file and print the LOWEST one (semver-sorted). Using the min catches a
# partial downgrade: if a single `use` line reverts to @4 while the rest stay
# at @5, the file's min drops to @4 and the guard fires. Prints nothing if
# the file has no such reference.
extension_version_of() {
  local file="$1"
  if [ ! -f "$file" ]; then
    return 0
  fi
  grep -oE 'duckdb:extension/[a-zA-Z0-9_-]+@[0-9]+\.[0-9]+\.[0-9]+' "$file" \
    | sed 's/.*@//' \
    | sort -Vu \
    | head -1
}

SRC_WIT="$WIT_ROOT/core/duckdb-core.wit"
DST_WIT="$CORE_DIR/duckdb-core.wit"

if [ ! -f "$SRC_WIT" ]; then
  echo "sync-core-wit.sh: source not found: $SRC_WIT" >&2
  exit 1
fi

SRC_VER="$(extension_version_of "$SRC_WIT" || true)"
DST_VER="$(extension_version_of "$DST_WIT" || true)"

if [ "$FORCE" -eq 0 ]; then
  if [ -z "$SRC_VER" ]; then
    echo "sync-core-wit.sh: source has no duckdb:extension/<iface>@X.Y.Z references" >&2
    echo "  source: $SRC_WIT" >&2
    echo "Refusing to sync; pass --force to override." >&2
    exit 1
  fi

  if [ -n "$DST_VER" ]; then
    # If source < target (strictly), refuse. Equal or higher is fine.
    LOWEST="$(printf '%s\n%s\n' "$SRC_VER" "$DST_VER" | sort -V | head -1)"
    if [ "$LOWEST" = "$SRC_VER" ] && [ "$SRC_VER" != "$DST_VER" ]; then
      echo "sync-core-wit.sh: refusing to downgrade core/wit duckdb:extension version" >&2
      echo "  source: $SRC_WIT (@${SRC_VER})" >&2
      echo "  target: $DST_WIT (@${DST_VER})" >&2
      echo "Pass --force to override (legitimate downgrade)." >&2
      exit 1
    fi
  fi
fi

if [ "$FORCE" -eq 1 ]; then
  echo "sync-core-wit.sh: source @${SRC_VER:-?} -> target @${DST_VER:-<empty>} (forced)"
else
  echo "sync-core-wit.sh: source @${SRC_VER:-?} -> target @${DST_VER:-<empty>}"
fi

rm -rf "$CORE_DIR"
mkdir -p "$CORE_DIR/deps"

cp "$WIT_ROOT/core/duckdb-core.wit" "$CORE_DIR/duckdb-core.wit"
cp "$WIT_ROOT/core/deps.toml" "$CORE_DIR/deps.toml"
rm -rf "$CORE_DIR/deps/duckdb-extension"
cp -R "$WIT_ROOT/duckdb-extension" "$CORE_DIR/deps/duckdb-extension"

for dep in "$WIT_ROOT"/deps/*; do
  name="$(basename "$dep")"
  rm -rf "$CORE_DIR/deps/$name"
  cp -R "$dep" "$CORE_DIR/deps/$name"
done
