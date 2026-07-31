#!/usr/bin/env python3
import json
import os
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/health":
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b"ok")
        else:
            self.send_error(404)

    def do_POST(self):
        if self.path != "/v1/chat/completions":
            self.send_error(404)
            return
        length = int(self.headers.get("content-length", "0"))
        request = json.loads(self.rfile.read(length) or b"{}")
        prompt = request.get("messages", [{}])[0].get("content", "")
        if "<delay>" in prompt:
            time.sleep(10)
        cue_ids = sorted(set(re.findall(r"<c(\d+)>", prompt)), key=int)
        translated = "\n".join(
            f"<c{cue_id}>translated {cue_id}</c{cue_id}>" for cue_id in cue_ids
        ) or "translated"
        body = json.dumps(
            {"choices": [{"message": {"content": translated}}]}
        ).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        pass


startup_delay = float(os.environ.get("FAKE_VLLM_START_DELAY", "0"))
if startup_delay:
    startup_lines = [
        "Loading safetensors checkpoint shards: 0% Completed",
        "Loading safetensors checkpoint shards: 60% Completed",
        "Using the Marlin kernel; compiling model",
        "Capturing CUDA graphs (PIECEWISE): 50%",
        "Application startup complete.",
    ]
    for startup_line in startup_lines:
        print(startup_line, file=sys.stderr, flush=True)
        time.sleep(startup_delay)

HTTPServer(("127.0.0.1", 8001), Handler).serve_forever()
