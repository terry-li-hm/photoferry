#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/smoke_integration.sh --zip /path/to/takeout.zip [options]

Options:
  --zip PATH             Path to a real Google Takeout ZIP (required)
  --workdir PATH         Working directory for manifests/extraction
                         Default: /tmp/photoferry-smoke-<timestamp>
  --bin CMD              photoferry command
                         Default: "cargo run --quiet --"
  --enable-import        Run real import step (writes into Photos library)
  --enable-verify        Run verify step after import
  --keep-workdir         Keep working directory after completion
  -h, --help             Show this help

Notes:
  - verify requires full Photos access permission.
  - enable-import and enable-verify are intentionally explicit.
EOF
}

ZIP_PATH=""
WORKDIR=""
BIN_CMD="${PHOTOFERRY_BIN:-cargo run --quiet --}"
ENABLE_IMPORT=0
ENABLE_VERIFY=0
KEEP_WORKDIR=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --zip)
      ZIP_PATH="${2:-}"
      shift 2
      ;;
    --workdir)
      WORKDIR="${2:-}"
      shift 2
      ;;
    --bin)
      BIN_CMD="${2:-}"
      shift 2
      ;;
    --enable-import)
      ENABLE_IMPORT=1
      shift
      ;;
    --enable-verify)
      ENABLE_VERIFY=1
      shift
      ;;
    --keep-workdir)
      KEEP_WORKDIR=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage
      exit 1
      ;;
  esac
done

if [[ -z "$ZIP_PATH" ]]; then
  echo "--zip is required" >&2
  usage
  exit 1
fi

if [[ ! -f "$ZIP_PATH" ]]; then
  echo "ZIP not found: $ZIP_PATH" >&2
  exit 1
fi

if [[ -z "$WORKDIR" ]]; then
  WORKDIR="/tmp/photoferry-smoke-$(date +%Y%m%d-%H%M%S)"
fi

mkdir -p "$WORKDIR"
ZIP_NAME="$(basename "$ZIP_PATH")"
TARGET_ZIP="$WORKDIR/$ZIP_NAME"
cp -f "$ZIP_PATH" "$TARGET_ZIP"

cleanup() {
  if [[ "$KEEP_WORKDIR" -eq 0 ]]; then
    rm -rf "$WORKDIR"
  else
    echo "Kept workdir: $WORKDIR"
  fi
}
trap cleanup EXIT

echo "==> Smoke test workdir: $WORKDIR"
echo "==> Using ZIP: $TARGET_ZIP"
echo "==> Using command: $BIN_CMD"

echo
echo "==> Step 1/3: Dry-run parse/import loop"
eval "$BIN_CMD run --dir \"$WORKDIR\" --once --dry-run"

if [[ "$ENABLE_IMPORT" -eq 1 ]]; then
  echo
  echo "==> Step 2/3: Real import (single ZIP)"
  eval "$BIN_CMD run --dir \"$WORKDIR\" --once --verbose"
else
  echo
  echo "==> Step 2/3: Skipped real import (set --enable-import to run)"
fi

if [[ "$ENABLE_VERIFY" -eq 1 ]]; then
  if [[ "$ENABLE_IMPORT" -ne 1 ]]; then
    echo "--enable-verify requires --enable-import" >&2
    exit 1
  fi
  echo
  echo "==> Step 3/3: Verify imported assets"
  eval "$BIN_CMD verify --dir \"$WORKDIR\""
else
  echo
  echo "==> Step 3/3: Skipped verify (set --enable-verify to run)"
fi

echo
echo "Smoke integration run completed."
echo "Manifest files (if generated):"
ls -1 "$WORKDIR"/.photoferry-manifest-*.json 2>/dev/null || true
