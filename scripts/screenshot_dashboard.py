#!/usr/bin/env python3
"""Screenshot the demo dashboard with a headless Chromium-family browser and crop it to 1600x900.

    docker compose up --build -d     # let it run ~12 minutes so the outage and CRC burst are in range
    python scripts/screenshot_dashboard.py --browser "<path to a Chromium-based browser executable>"

Writes docs/figure-full.png (whole dashboard) and docs/figure.png (top two rows, 1600x900). Needs Pillow.
"""
import argparse
import subprocess
import tempfile
import time
from pathlib import Path

from PIL import Image

ap = argparse.ArgumentParser()
ap.add_argument("--browser", required=True, help="path to chrome / msedge / chromium")
ap.add_argument("--url", default="http://127.0.0.1:3000/d/lgh-gateway-health/lgh-gateway-health?kiosk&from=now-12m&to=now&refresh=")
ap.add_argument("--full-height", type=int, default=2000)
ap.add_argument("--wait-ms", type=int, default=20000)
args = ap.parse_args()

docs = Path(__file__).resolve().parent.parent / "docs"
docs.mkdir(exist_ok=True)
full = docs / "figure-full.png"
full.unlink(missing_ok=True)
with tempfile.TemporaryDirectory() as profile:
    proc = subprocess.Popen(
        [args.browser, "--headless=new", "--disable-gpu", "--hide-scrollbars", f"--user-data-dir={profile}",
         f"--window-size=1600,{args.full_height}", f"--virtual-time-budget={args.wait_ms}",
         f"--screenshot={full}", args.url],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    # Some headless builds write the PNG and then never exit; wait for the file, then stop the browser.
    deadline = time.time() + 180
    while time.time() < deadline and proc.poll() is None:
        if full.exists() and full.stat().st_size > 10_000:
            time.sleep(2)
            break
        time.sleep(1)
    proc.kill()
    proc.wait()
if not full.exists():
    raise SystemExit("browser produced no screenshot")
im = Image.open(full)
print("full:", im.size)
# Keep whole panels: crop above the third row (y=848 at 1600 px width), pad to 900 with the page background.
top = im.crop((0, 0, 1600, 848))
canvas = Image.new("RGB", (1600, 900), im.convert("RGB").getpixel((1590, 1990)))
canvas.paste(top.convert("RGB"), (0, 0))
canvas.save(docs / "figure.png", optimize=True)
im.save(full, optimize=True)
print("wrote", docs / "figure.png")
