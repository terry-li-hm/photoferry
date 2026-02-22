# Safe Rerun and Recovery

This runbook covers safe recovery when state files are missing/corrupt or a run is interrupted.

## State Files

- Per-zip import manifests:
  - `.photoferry-manifest-<zip-stem>.json`
- Per-job download progress:
  - `.photoferry-download-<job-prefix>-<hash>.json`

Both are now treated strictly during runtime; corrupt JSON fails fast instead of silently resetting.

## Standard Safe Rerun

1. Keep all ZIP files in place.
2. Run:

```bash
photoferry verify --dir ~/Downloads
photoferry retry-missing --dir ~/Downloads --verbose
```

3. For download flows:

```bash
photoferry download --job <JOB_ID> --user <USER_ID> --dir ~/Downloads --start <N> --end <M> --icloud-confirmed
```

## Corrupt Manifest Recovery

Symptoms:
- Error contains `Corrupt manifest JSON`.

Steps:

1. Back up the corrupt manifest:

```bash
cp ~/Downloads/.photoferry-manifest-<zip-stem>.json ~/Downloads/.photoferry-manifest-<zip-stem>.json.bak
```

2. Keep original ZIP.
3. Remove or repair the corrupt manifest JSON.
4. Re-run single ZIP safely:

```bash
photoferry run --dir ~/Downloads --once --verbose
photoferry verify --dir ~/Downloads
```

## Corrupt Download Progress Recovery

Symptoms:
- Error contains `Corrupt download progress JSON`.

Steps:

1. Back up progress file:

```bash
cp ~/Downloads/.photoferry-download-<prefix>-<hash>.json ~/Downloads/.photoferry-download-<prefix>-<hash>.json.bak
```

2. Remove corrupt progress file.
3. Re-run `photoferry download ...` with same `job` and `user`.
4. Validate with `verify`.

## Safety Rules

- Never delete ZIPs unless verify passes.
- Keep `--icloud-confirmed` explicit for deletion in download flow.
- Prefer `retry-missing` over ad-hoc re-import loops.
