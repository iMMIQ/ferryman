#!/usr/bin/env python3
"""Start a packaged Web/Agent image and probe it without loading a GPU model."""
import argparse
import json
import re
import subprocess
import time
import urllib.request

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("kind", choices=["web", "agent"])
parser.add_argument("image")
args = parser.parse_args()
port, endpoint = (8080, "/api/config") if args.kind == "web" else (8090, "/healthz")
container = subprocess.check_output([
    "docker", "run", "--rm", "-d", "--runtime=runc", "-p", f"127.0.0.1::{port}",
    "-e", "FERRYMAN_ALLOW_LOCAL_USER=true", "-e", "FERRYMAN_DATA_DIR=/tmp/ferryman",
    "-e", "FERRYMAN_AGENT_TOKEN=ferryman-build-smoke-token", "-e", "LD_PRELOAD=",
    args.image,
], text=True).strip()
try:
    mapped = subprocess.check_output(["docker", "port", container, str(port)], text=True).strip()
    origin = "http://" + mapped
    for attempt in range(60):
        try:
            with urllib.request.urlopen(origin + endpoint, timeout=2) as response:
                result = json.load(response)
            if args.kind == "agent":
                assert result["status"] == "ok", result
            else:
                with urllib.request.urlopen(origin + "/", timeout=2) as response:
                    html = response.read().decode()
                assets = re.findall(r'(?:src|href)="(/assets/[^\"]+)"', html)
                assert assets, "frontend assets not referenced"
                for asset in assets:
                    with urllib.request.urlopen(origin + asset, timeout=2) as response:
                        assert response.read(), f"empty frontend asset: {asset}"
                        assert "text/html" not in response.headers.get("Content-Type", ""), asset
            print(f"Packaged {args.kind} smoke passed: {args.image}")
            break
        except (OSError, ValueError):
            if attempt == 59:
                raise
            time.sleep(1)
finally:
    subprocess.run(["docker", "logs", container], check=False)
    subprocess.run(["docker", "stop", container], check=False, stdout=subprocess.DEVNULL)
