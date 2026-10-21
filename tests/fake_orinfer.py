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
            {"choices": [{"finish_reason": "stop", "message": {"content": translated}}]}
        ).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        pass


def batch():
    def arg(name):
        return sys.argv[sys.argv.index(name) + 1]

    input_path, output_path = arg("--input"), arg("--output")
    with open(input_path) as file:
        requests = [json.loads(line) for line in file if line.strip()]
    root = os.environ.get("FIXTURE_ROOT")
    if root:
        with open(os.path.join(root, "runs.jsonl"), "a") as file:
            file.write(json.dumps({"args": sys.argv[1:], "requests": requests}) + "\n")
    completed = set()
    if "--resume" in sys.argv and os.path.exists(output_path):
        with open(output_path) as file:
            completed = {json.loads(line)["custom_id"] for line in file if line.strip()}
    with open(output_path + ".meta.json", "w") as file:
        json.dump({"fixture": True}, file)
    with open(output_path, "a") as file:
        # Deliberately reverse completion order. Every row remains independent.
        for request in reversed(requests):
            assert request["method"] == "POST"
            assert request["url"] == "/v1/chat/completions"
            if request["custom_id"] in completed:
                continue
            body = request["body"]
            assert body["enable_thinking"] is False
            prompt = body["messages"][0]["content"]
            if "<delay>" in prompt or ("<resume-delay>" in prompt and "--resume" not in sys.argv):
                time.sleep(120)
            pairs = re.findall(r"<c(\d+)>(.*?)</c\1>", prompt, re.S)
            text = prompt.split("\n\n")[-1]
            truncated = "<truncate>" in prompt and (len(pairs) > 1 or len(text) > 128)
            content = "INCOMPLETE-DO-NOT-CACHE" if truncated else (
                "\n".join(f"<c{n}>译文:{text}</c{n}>" for n, text in pairs) if pairs else "译文:" + text
            )
            output = {"custom_id": request["custom_id"], "error": None, "response": {
                "status_code": 200, "body": {"choices": [{
                    "finish_reason": "length" if truncated else "stop", "message": {"content": content}
                }]}
            }}
            file.write(json.dumps(output, ensure_ascii=False) + "\n")
            file.flush()
            time.sleep(0.01)
    print(json.dumps({"processed": len(requests)}), flush=True)


if len(sys.argv) > 1 and sys.argv[1] == "batch":
    batch()
    sys.exit(0)


startup_delay = float(os.environ.get("FAKE_ORINFER_START_DELAY", "0"))
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

HTTPServer(("127.0.0.1", 18088), Handler).serve_forever()
