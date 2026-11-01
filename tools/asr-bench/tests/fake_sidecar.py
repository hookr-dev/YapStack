#!/usr/bin/env python3
"""Stands in for the sidecar: answers the JSON-line IPC with canned replies."""
import json
import os
import sys

accel = os.environ.get("YAPSTACK_PARAKEET_ACCEL", "auto")
if os.environ.get("YAPSTACK_ORT_PLACEMENT") == "1":
    print('INFO parakeet: ort node placement marker="ort_placement" session="encoder" ep="webgpu" '
          'heavy_on_ep=72/366 cpu_nodes=1014 nodes_by_ep={"CPUExecutionProvider": 1014}', file=sys.stderr)
seen = set()
for line in sys.stdin:
    req = json.loads(line)
    if req["type"] == "shutdown":
        break
    if req["type"] == "query_engine_info":
        reply = {"type": "engine_info", "id": req["id"], "accel": accel, "model_dir": sys.argv[-1]}
    elif "fail" in req["audio_path"] or ("flaky" in req["audio_path"] and req["audio_path"] not in seen):
        seen.add(req["audio_path"])
        reply = {"type": "error", "id": req["id"], "message": "boom"}
    else:
        print(json.dumps({"type": "progress", "id": req["id"], "percent": 50.0}), flush=True)
        reply = {"type": "transcription", "id": req["id"], "text": "Hello, world.", "segments": [], "duration_ms": 250}
    print(json.dumps(reply), flush=True)
