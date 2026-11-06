#!/usr/bin/env python3
"""Stands in for the `replay_chunk_boundaries` test: the same env vars and
manifest, but each clip is cut into two halves, plus a 0.1 s sliver the
bench must skip. A clip named `*nochunks*` gets no chunks at all."""
import json
import os
from pathlib import Path

import soundfile

out = Path(os.environ["YAPSTACK_REPLAY_OUT"])
assert os.environ["YAPSTACK_REPLAY_MODE"] in ("live", "backfill", "rewind")
with open(out / "manifest.jsonl", "w") as manifest:
    for wav in Path(os.environ["YAPSTACK_REPLAY_LIST"]).read_text().split():
        if "nochunks" in Path(wav).stem:
            continue  # the chunker found no speech in it
        audio, sr = soundfile.read(wav, dtype="float32")
        half = len(audio) // 2
        sliver = int(0.1 * sr)
        for i, (start, end) in enumerate([(0, half), (half, len(audio) - sliver), (len(audio) - sliver, len(audio))]):
            path = out / f"{Path(wav).stem}__{i:03}.wav"
            soundfile.write(str(path), audio[start:end], sr, subtype="PCM_16")
            manifest.write(json.dumps({"clip": wav, "idx": i, "start": start, "end": end, "sr": sr, "path": str(path)}) + "\n")
