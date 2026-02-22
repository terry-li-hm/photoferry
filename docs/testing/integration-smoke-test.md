# Integration Smoke Test

This smoke test validates the real CLI flow against a small, real Takeout ZIP:

1. `run --dry-run`
2. `run` (real import, optional)
3. `verify` (optional)

## Prerequisites

- macOS with Photos.app available.
- Full Photos permission granted to `photoferry`.
- A small real Google Takeout ZIP.

## Command

From repo root:

```bash
scripts/smoke_integration.sh --zip /path/to/takeout-small.zip --enable-import --enable-verify --keep-workdir
```

Safe parse-only mode (no import):

```bash
scripts/smoke_integration.sh --zip /path/to/takeout-small.zip
```

## Expected Output

- Dry-run step completes with inventory summary.
- Import step reports imported count and writes `.photoferry-manifest-*.json`.
- Verify step reports `All assets verified successfully` or explicit missing/mismatch reasons.

## Failure Triage

- `Photos access is limited/denied`: grant full access in macOS settings.
- `Corrupt manifest JSON`: follow `docs/operations/safe-rerun-recovery.md`.
- Download/verify state issues: keep workdir (`--keep-workdir`) and inspect manifest/progress files.
