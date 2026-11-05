#!/usr/bin/env python3
"""Stands in for the sidecar: answers the JSON-line IPC with canned replies.

A model dir named `*mute*` returns no text; an audio path with `badts`
returns a segment ending far past any chunk."""
import json
import os
import sys

accel = os.environ.get("YAPSTACK_PARAKEET_ACCEL", "auto")
model = sys.argv[-1]
if os.environ.get("YAPSTACK_ORT_PLACEMENT") == "1":
    print('INFO parakeet: ort node placement marker="ort_placement" session="encoder" ep="webgpu" '
          'heavy_on_ep=72/366 cpu_nodes=1014 nodes_by_ep={"CPUExecutionProvider": 1014}', file=sys.stderr)
seen = set()
for line in sys.stdin:
    req = json.loads(line)
    if req["type"] == "shutdown":
        break
    if req["type"] == "query_engine_info":
        reply = {"type": "engine_info", "id": req["id"], "accel": accel, "model_dir": model}
    elif "fail" in req["audio_path"] or ("flaky" in req["audio_path"] and req["audio_path"] not in seen):
        seen.add(req["audio_path"])
        reply = {"type": "error", "id": req["id"], "message": "boom"}
    else:
        print(json.dumps({"type": "progress", "id": req["id"], "percent": 50.0}), flush=True)
        text = "" if "mute" in os.path.basename(model) else "Hello, world."
        end = 10_000_000 if "badts" in req["audio_path"] else 100
        segments = [{"start_ms": 0, "end_ms": end, "text": text, "confidence": 0.9}] if text else []
        reply = {"type": "transcription", "id": req["id"], "text": text, "segments": segments, "duration_ms": 250}
    print(json.dumps(reply), flush=True)
