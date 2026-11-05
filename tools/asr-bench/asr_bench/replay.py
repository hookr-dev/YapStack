"""App-style replay: cut long recordings with the app's own chunking, then
score every model on the same chunks.

The chunking is the desktop crate's code, not a copy of it: the `#[ignore]`d
test `replay_chunk_boundaries`
(apps/desktop/src-tauri/src/commands/live_transcription/replay_harness.rs)
runs the Silero VAD state machine (and, for backfill, the job-quantum split)
over each WAV and writes the chunk WAVs plus `manifest.jsonl`.
"""

from __future__ import annotations

import json
import os
import subprocess
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import soundfile

from . import metrics
from .corpus import fetch_librispeech
from .sidecar import Sidecar

# Bump when the slices are built differently: numbers from different
# corpora aren't comparable.
CORPUS_VERSION = "librispeech-replay-v1"
CORPUS_SOURCE = (("test-clean", 400), ("test-other", 200))
CORPUS_FILE = "corpus.json"
SLICES = ("long", "long44", "mono", "drift")
MODES = ("live", "backfill", "rewind")
GATE_MODES = ("live", "backfill")
HARNESS_TEST = "commands::live_transcription::replay_harness::replay_chunk_boundaries"

# The app skips chunks shorter than this (MIN_CHUNK_DURATION_SECS in
# live_transcription.rs) and never sends them to the sidecar.
MIN_CHUNK_SECS = 0.35
# A clip is truncated when its transcript drops this many consecutive
# reference words: about a chunk's worth of speech missing.
TRUNCATION_RUN = 4
# Slack for a segment ending past its chunk (frame rounding).
TIMESTAMP_SLACK_MS = 50
GROUP = 3
MIN_GROUP_SECS = 20.0
DRIFT_SECS = 300.0


def _trim(x: np.ndarray, threshold: float = 0.01, pad: int = 160) -> np.ndarray:
    idx = np.nonzero(np.abs(x) > threshold)[0]
    if len(idx) == 0:
        return x
    return x[max(0, idx[0] - pad) : idx[-1] + pad]


def _to_44k_stereo(x: np.ndarray) -> np.ndarray:
    from scipy.signal import resample_poly

    up = resample_poly(x, 441, 160).astype(np.float32)
    return np.stack([up, up], axis=1)


def _write(path: Path, audio: np.ndarray, rate: int, reference: str) -> None:
    soundfile.write(str(path.with_suffix(".wav")), audio, rate, subtype="PCM_16")
    path.with_suffix(".txt").write_text(reference + "\n", encoding="utf-8")


def build_corpus(cache: Path, out: Path) -> dict:
    """The replay slices, from cached LibriSpeech (16 kHz, archive order):

    - long: 3 consecutive utterances of one chapter, natural gaps kept, at
      least 20 s (16 kHz mono).
    - mono: the same groups with each utterance's silence trimmed and the
      three butted together: a monologue with no pause at the joins.
    - long44: long resampled to 44.1 kHz stereo, a typical capture format.
    - drift: consecutive long clips joined into 5-minute 44.1 kHz stereo
      buffers, the size of a "Last 300 s" backfill.
    """
    groups = []
    for split, count in CORPUS_SOURCE:
        folder = fetch_librispeech(cache, split, count)
        utts = json.loads((folder / "manifest.json").read_text())["utterances"][:count]
        by_chapter: dict[tuple[str, str], list[str]] = defaultdict(list)
        for u in utts:
            speaker, chapter, _ = u.split("-")
            by_chapter[(speaker, chapter)].append(u)
        for items in by_chapter.values():
            for i in range(0, len(items) - (GROUP - 1), GROUP):
                groups.append([(folder / f"{u}.wav", (folder / f"{u}.txt").read_text(encoding="utf-8").strip(), u) for u in items[i : i + GROUP]])

    for s in SLICES:
        (out / s).mkdir(parents=True, exist_ok=True)
    long_clips = []
    for g in groups:
        name = f"{g[0][2]}_to_{g[-1][2].split('-')[-1]}"
        audio = [soundfile.read(str(w), dtype="float32")[0] for w, _, _ in g]
        reference = " ".join(t for _, t, _ in g)
        joined = np.concatenate(audio)
        if len(joined) / 16000 < MIN_GROUP_SECS:
            continue
        _write(out / "long" / name, joined, 16000, reference)
        _write(out / "mono" / name, np.concatenate([_trim(a) for a in audio]), 16000, reference)
        _write(out / "long44" / name, _to_44k_stereo(joined), 44100, reference)
        long_clips.append((name, joined, reference))

    long_clips.sort(key=lambda c: c[0])
    batch, secs, n = [], 0.0, 0
    for clip in long_clips:
        batch.append(clip)
        secs += len(clip[1]) / 16000
        if secs >= DRIFT_SECS:
            audio = np.concatenate([c[1] for c in batch])
            _write(out / "drift" / f"drift{n:02d}", _to_44k_stereo(audio), 44100, " ".join(c[2] for c in batch))
            n += 1
            batch, secs = [], 0.0

    info = {
        "version": CORPUS_VERSION,
        "source": [f"librispeech:{s}:{c}" for s, c in CORPUS_SOURCE],
        "clips": {s: len(list((out / s).glob("*.wav"))) for s in SLICES},
    }
    (out / CORPUS_FILE).write_text(json.dumps(info, indent=1) + "\n")
    return info


def corpus_info(corpus_dir: Path) -> dict:
    path = corpus_dir / CORPUS_FILE
    if not path.is_file():
        raise SystemExit(f"no {CORPUS_FILE} in {corpus_dir}; build it with `uv run asr-bench replay-corpus`")
    return json.loads(path.read_text())


def slice_wavs(corpus_dir: Path, name: str, limit: int | None) -> list[Path]:
    wavs = sorted((corpus_dir / name).glob("*.wav"))
    if not wavs:
        raise SystemExit(f"slice {name!r} has no clips under {corpus_dir / name}")
    return wavs[:limit] if limit else wavs


def harness_command() -> list[str]:
    return ["cargo", "test", "-p", "yapstack-desktop", "--lib", "--", "--ignored", "--exact", HARNESS_TEST]


def replay(repo: Path, wavs: list[Path], mode: str, out: Path) -> list[dict]:
    """Cut `wavs` with the app's `mode` chunking into `out`; returns the
    manifest rows."""
    out.mkdir(parents=True, exist_ok=True)
    listing = out / "clips.txt"
    listing.write_text("".join(f"{w.resolve()}\n" for w in wavs))
    env_vars = {"YAPSTACK_REPLAY_LIST": str(listing), "YAPSTACK_REPLAY_OUT": str(out), "YAPSTACK_REPLAY_MODE": mode}
    log = out / "harness.log"
    with open(log, "wb") as fh:
        code = subprocess.run(harness_command(), cwd=repo, env={**os.environ, **env_vars}, stdout=fh, stderr=subprocess.STDOUT).returncode
    manifest = out / "manifest.jsonl"
    if code or not manifest.is_file():
        tail = log.read_text(errors="replace").splitlines()[-20:]
        raise SystemExit(f"replay harness failed ({mode}, exit {code}); see {log}:\n" + "\n".join(tail))
    return [json.loads(line) for line in manifest.read_text().splitlines() if line.strip()]


def timestamps_ok(segments: list[dict], chunk_ms: float) -> bool:
    """Every segment in order and inside the chunk."""
    if not segments:
        return False
    prev_end = 0
    for s in segments:
        start, end = s["start_ms"], s["end_ms"]
        if not (0 <= start < end <= chunk_ms + TIMESTAMP_SLACK_MS and start >= prev_end - 1):
            return False
        prev_end = end
    return True


@dataclass
class Scored:
    row: dict
    chunks: list[dict]


def score(sc: Sidecar, manifest: list[dict], normalizer: str) -> Scored:
    """Transcribe each chunk the app would send and score per clip.

    - empty: a chunk sent to the sidecar came back with no text (an error
      counts as empty too, and is also counted under errors).
    - truncated: a clip whose transcript drops `TRUNCATION_RUN` or more
      consecutive reference words.
    - timestamp_bad: a chunk with text whose segments are missing, out of
      order, or end past the chunk.
    """
    by_clip: dict[str, list[tuple[int, str]]] = defaultdict(list)
    chunks = []
    empty = errors = skipped = ts_bad = 0
    for r in manifest:
        secs = (r["end"] - r["start"]) / r["sr"]
        text, error, ts = "", None, ""
        if secs < MIN_CHUNK_SECS:
            skipped += 1
        else:
            t = sc.transcribe(Path(r["path"]))
            text, error = t.text.strip(), t.error
            errors += bool(error)
            if not text:
                empty += 1
            else:
                ts = timestamps_ok(t.segments, secs * 1000)
                ts_bad += not ts
        by_clip[r["clip"]].append((r["idx"], text))
        chunks.append({"clip": r["clip"], "idx": r["idx"], "secs": round(secs, 3), "skipped": secs < MIN_CHUNK_SECS,
                       "text": text, "error": error or "", "timestamps_ok": ts})

    pairs, truncated = [], 0
    for clip, parts in by_clip.items():
        reference = Path(clip).with_suffix(".txt").read_text(encoding="utf-8").strip()
        hypothesis = " ".join(t for _, t in sorted(parts) if t)
        pairs.append((reference, hypothesis))
        truncated += metrics.longest_deletion_run(reference, hypothesis, normalizer) >= TRUNCATION_RUN
    e = metrics.error_rates(pairs, normalizer)
    row = {
        "clips": len(by_clip),
        "chunks": len(manifest),
        "skipped": skipped,
        "wer": None if e.wer is None else round(e.wer * 100, 2),
        "empty": empty,
        "truncated": truncated,
        "timestamp_bad": ts_bad,
        "errors": errors,
    }
    return Scored(row, chunks)


def gate(rows: list[dict], baseline: str, wer_margin: float) -> list[dict]:
    """AE-G1 against `baseline` (a row key, NAME:EP): on every slice x mode
    the baseline ran, each other model's WER is within `wer_margin` points
    of it, and its empty, truncated and bad-timestamp counts are no higher
    (same replayed chunks; owner ruling 2026-11-04, ADR 0008)."""
    base = {(r["slice"], r["mode"]): r for r in rows if r["key"] == baseline}
    if not base:
        return [{"key": baseline, "slice": "", "mode": "", "pass": False, "why": f"no rows for baseline {baseline}"}]
    out = []
    for key in dict.fromkeys(r["key"] for r in rows if r["key"] != baseline):
        mine = {(r["slice"], r["mode"]): r for r in rows if r["key"] == key}
        for sm, b in base.items():
            r = mine.get(sm)
            check = {"key": key, "slice": sm[0], "mode": sm[1]}
            if r is None or r["wer"] is None or b["wer"] is None:
                out.append({**check, "pass": False, "why": "not measured"})
                continue
            delta = round(r["wer"] - b["wer"], 2)
            fails = []
            if delta > wer_margin:
                fails.append(f"WER {delta:+.2f} pt")
            for k in ("empty", "truncated", "timestamp_bad"):
                if r[k] > b[k]:
                    fails.append(f"{k} {r[k]} > {b[k]}")
            out.append({**check, "wer_delta": delta, "pass": not fails, "why": "; ".join(fails)})
    return out


def markdown(rows: list[dict], checks: list[dict] | None, header: list[str], baseline: str | None, wer_margin: float) -> str:
    lines = header + [
        "",
        "| Slice | Mode | Model | Loaded | Clips | Chunks | WER % | Empty | Truncated | Timestamp bad | Errors |",
        "|---|---|---|---|---|---|---|---|---|---|---|",
    ]
    for r in rows:
        wer = "" if r["wer"] is None else f"{r['wer']:.2f}"
        lines.append(f"| {r['slice']} | {r['mode']} | {r['key']} | {r['accel_loaded']} | {r['clips']} | {r['chunks']} | "
                     f"{wer} | {r['empty']} | {r['truncated']} | {r['timestamp_bad']} | {r['errors']} |")
    if checks is not None:
        lines += [
            "",
            f"AE-G1 vs `{baseline}`: WER ≤ baseline + {wer_margin:g} pt, empty / truncated / timestamp-bad ≤ baseline, on every slice x mode.",
            "",
            "| Slice | Mode | Model | Δ WER pt | Pass | Why |",
            "|---|---|---|---|---|---|",
        ]
        for c in checks:
            delta = "" if c.get("wer_delta") is None else f"{c['wer_delta']:+.2f}"
            lines.append(f"| {c['slice']} | {c['mode']} | {c['key']} | {delta} | "
                         f"{'yes' if c['pass'] else 'NO'} | {c['why']} |")
        lines += ["", f"AE-G1: {'PASS' if all(c['pass'] for c in checks) else 'FAIL'}"]
    return "\n".join(lines) + "\n"
