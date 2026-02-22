# PhotoFerry

Google Photos to iCloud migration CLI. Processes Google Takeout exports, preserves metadata, and imports via native PhotoKit.

**macOS only** — uses Swift FFI to talk directly to Photos.app.

## What it does

- Extracts Google Takeout ZIP archives and parses sidecar JSON metadata (timestamps, GPS, favorites, descriptions)
- Recreates album structure (skips auto-generated "Photos from YYYY" folders)
- Pairs Live Photos automatically (HEIC + MOV by filename)
- Filters out trashed files
- Tracks progress via per-zip manifests for idempotent re-runs
- Verifies all imports exist in the Photos library with correct creation dates
- Can download Takeout archives directly from Google (uses Chrome cookies)

## Usage

```bash
# Check Photos.app permissions
photoferry check

# Process all Takeout zips in a directory
photoferry run ~/Downloads/takeout/

# Dry run first
photoferry run ~/Downloads/takeout/ --dry-run

# List detected albums
photoferry albums ~/Downloads/takeout/

# Verify imports match what was processed
photoferry verify ~/Downloads/takeout/

# Re-import anything that failed verification
photoferry retry-missing ~/Downloads/takeout/

# Download from Google, import, verify, clean up
photoferry download --user me@gmail.com --dir ~/Downloads/takeout/
```

## Requirements

- macOS with Full Disk Access for Photos.app (System Settings > Privacy & Security > Photos)
- Chrome running and logged into Google (for `download` command)

## Install

```bash
cargo build --release
# Binary at ./target/release/photoferry
```

## License

MIT
