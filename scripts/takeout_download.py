#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pycookiecheat", "requests", "tqdm"]
# ///
"""Download all Google Takeout zips using Chrome cookies."""

import re
import sys
import time
from pathlib import Path

import requests
from pycookiecheat import chrome_cookies
from tqdm import tqdm

DEST = Path.home() / "Downloads"
BASE_URL = "https://takeout.google.com/takeout/download"
JOB_ID = "65a591a2-7f11-483f-9e04-d952f087a07f"
USER_ID = "118329727694314214742"
TOTAL = 99  # i=0 to i=98

def build_url(i: int) -> str:
    return f"{BASE_URL}?j={JOB_ID}&i={i}&user={USER_ID}"

def download_one(session: requests.Session, i: int) -> Path | None:
    url = build_url(i)

    # HEAD request to get filename without downloading
    resp = session.head(url, allow_redirects=True, timeout=30)
    cd = resp.headers.get("content-disposition", "")
    match = re.search(r'filename="?([^";\n]+)"?', cd)
    if match:
        filename = match.group(1).strip()
    else:
        filename = f"takeout-part-{i:03d}.zip"

    dest = DEST / filename

    # Skip if already fully downloaded
    if dest.exists():
        expected = int(resp.headers.get("content-length", 0))
        if expected and dest.stat().st_size == expected:
            print(f"  [{i:02d}] {filename} — already complete, skipping")
            return dest

    # Resume support
    headers = {}
    resume_pos = 0
    if dest.exists():
        resume_pos = dest.stat().st_size
        headers["Range"] = f"bytes={resume_pos}-"
        print(f"  [{i:02d}] Resuming {filename} from {resume_pos // 1024 // 1024}MB")
    else:
        print(f"  [{i:02d}] Downloading {filename}")

    resp = session.get(url, headers=headers, stream=True, timeout=60)
    resp.raise_for_status()

    total = int(resp.headers.get("content-length", 0)) + resume_pos
    mode = "ab" if resume_pos else "wb"

    with open(dest, mode) as f, tqdm(
        total=total,
        initial=resume_pos,
        unit="B",
        unit_scale=True,
        unit_divisor=1024,
        desc=filename[:40],
        leave=False,
    ) as bar:
        for chunk in resp.iter_content(chunk_size=1024 * 1024):
            f.write(chunk)
            bar.update(len(chunk))

    print(f"  [{i:02d}] Done → {dest.name} ({dest.stat().st_size // 1024 // 1024}MB)")
    return dest


def main():
    start_i = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    end_i = int(sys.argv[2]) if len(sys.argv) > 2 else TOTAL - 1

    print(f"Extracting Chrome cookies for takeout.google.com...")
    cookies = chrome_cookies("https://takeout.google.com")
    print(f"Got {len(cookies)} cookies")

    session = requests.Session()
    session.cookies.update(cookies)
    session.headers.update({
        "User-Agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36"
    })

    print(f"\nDownloading parts {start_i} to {end_i} → {DEST}\n")

    for i in range(start_i, end_i + 1):
        try:
            download_one(session, i)
        except Exception as e:
            print(f"  [{i:02d}] ERROR: {e} — retrying in 10s")
            time.sleep(10)
            try:
                download_one(session, i)
            except Exception as e2:
                print(f"  [{i:02d}] FAILED: {e2} — skipping")

    print("\nAll done.")


if __name__ == "__main__":
    main()
