"""asr-bench: time and score Parakeet model variants through the real sidecar."""

from __future__ import annotations

import argparse
import csv
import json
import os
import platform
import subprocess
import sys
from datetime import datetime
from pathlib import Path

from . import build as build_mod
from . import metrics
from .corpus import Corpus, parse_spec
from .sidecar import Sidecar, probe_placement, supports_placement

TOOL_DIR = Path(__file__).resolve().parent.parent
REPO = TOOL_DIR.parent.parent
CACHE = TOOL_DIR / ".cache"
STAGED_SIDECAR = CACHE / "bin" / (build_mod.SIDECAR + (".exe" if sys.platform == "win32" else ""))

CSV_FIELDS = [
    "model", "accel_requested", "accel_loaded", "accel_fallback", "accel_partial", "corpus",
    "clips", "audio_s", "warmup_runs", "scored_runs", "rtfx_steady", "rtfx_min", "rtfx_max",
    "rtfx_cold", "load_s", "cpu_user_s", "cpu_sys_s", "peak_rss_mb", "wer", "cer",
    "scored_utts", "unscored_utts", "errors", "placement_encoder", "placement_decoder_joint",
    "expected_rtfx", "rtfx_vs_expected", "model_dir",
]


def parse_kv(items: list[str], what: str) -> dict[str, str]:
    out = {}
    for item in items:
        k, sep, v = item.partition("=")
        if not sep or not k or not v:
            raise SystemExit(f"--{what} wants NAME=VALUE, got {item!r}")
        out[k] = v
    return out


def placement_cell(sessions, supported: bool, name: str) -> str:
    if not supported:
        return "n/a (sidecar predates YAPSTACK_ORT_PLACEMENT)"
    for s in sessions:
        if s.session == name:
            share = s.heavy_share
            pct = f" ({share:.1%})" if share is not None else ""
            return f"{s.ep} heavy {s.heavy_on_ep}{pct}"
    return "not reported"


def run_row(args, sidecar: Path, model: str, model_dir: Path, accel: str, corpora: list[Corpus], logs: Path, placement_ok: bool):
    tag = f"{model}-{accel}"
    print(f"== {tag}: {model_dir}", flush=True)
    sessions = []
    if placement_ok and not args.no_placement:
        sessions = probe_placement(sidecar, model_dir, accel, logs / f"{tag}.placement.stderr")

    sc = Sidecar(sidecar, model_dir, accel, logs / f"{tag}.stderr")
    info = sc.stats.engine_info
    first = corpora[0].clips[0]
    cold = []
    for _ in range(args.warmup):
        t = sc.transcribe(first.path)
        cold.append(first.seconds / t.infer_s if t.infer_s else None)

    per_corpus = []
    utterances = []
    for corpus in corpora:
        runs, pairs, errors = [], [], 0
        for clip in corpus.clips:
            hyp = None
            for rep in range(args.repeat):
                t = sc.transcribe(clip.path)
                if t.error:
                    errors += 1
                else:
                    runs.append((clip.seconds, t.infer_s))
                    hyp = t.text if hyp is None else hyp
                utterances.append({
                    "model": model, "accel": accel, "corpus": corpus.name, "clip": clip.id,
                    "run": rep, "audio_s": f"{clip.seconds:.3f}", "infer_s": f"{t.infer_s:.3f}",
                    "rtfx": f"{clip.seconds / t.infer_s:.2f}" if t.infer_s else "",
                    "reference": clip.reference or "", "hypothesis": t.text, "error": t.error or "",
                })
            if clip.reference is not None and hyp is not None:
                pairs.append((clip.reference, hyp))
        per_corpus.append((corpus, runs, pairs, errors))
        print(f"   {corpus.name}: {len(corpus.clips)} clips", flush=True)
    stats = sc.close()

    rows = []
    for corpus, runs, pairs, errors in per_corpus:
        r = metrics.rtfx(runs)
        e = metrics.error_rates(pairs, args.normalizer)
        expected = args.expect.get(f"{model}:{accel}")
        row = {
            "model": model,
            "accel_requested": accel,
            "accel_loaded": stats.accel or "",
            "accel_fallback": info.get("accel_fallback", ""),
            "accel_partial": info.get("accel_partial", ""),
            "corpus": corpus.name,
            "clips": len(corpus.clips),
            "audio_s": round(sum(c.seconds for c in corpus.clips), 2),
            "warmup_runs": args.warmup,
            "scored_runs": r.scored_runs,
            "rtfx_steady": _round(r.steady, 1),
            "rtfx_min": _round(r.run_min, 1),
            "rtfx_max": _round(r.run_max, 1),
            "rtfx_cold": _round(cold[0], 1) if cold else "",
            "load_s": round(stats.load_s, 2),
            "cpu_user_s": _round(stats.cpu_user_s, 1),
            "cpu_sys_s": _round(stats.cpu_sys_s, 1),
            "peak_rss_mb": round(stats.peak_rss_bytes / 2**20) if stats.peak_rss_bytes else "",
            "wer": _round(e.wer * 100 if e.wer is not None else None, 2),
            "cer": _round(e.cer * 100 if e.cer is not None else None, 2),
            "scored_utts": e.scored,
            "unscored_utts": len(corpus.clips) - e.scored,
            "errors": errors,
            "placement_encoder": placement_cell(sessions, placement_ok, "encoder"),
            "placement_decoder_joint": placement_cell(sessions, placement_ok, "decoder_joint"),
            "expected_rtfx": expected or "",
            "rtfx_vs_expected": "",
            "model_dir": str(model_dir),
        }
        if expected and r.steady:
            row["rtfx_vs_expected"] = f"{r.steady / float(expected) - 1:+.1%}"
        rows.append(row)
    if stats.exit_status:
        print(f"   warning: sidecar exited with status {stats.exit_status}", flush=True)
    return rows, utterances


def _round(v, n):
    return "" if v is None else round(v, n)


def loadavg() -> str:
    try:
        return " ".join(f"{x:.1f}" for x in os.getloadavg())
    except (AttributeError, OSError):
        return ""


def host_description() -> str:
    parts = [platform.system(), platform.release(), platform.machine()]
    if sys.platform == "darwin":
        for key in ("machdep.cpu.brand_string", "hw.memsize"):
            out = subprocess.run(["sysctl", "-n", key], capture_output=True, text=True).stdout.strip()
            if key == "hw.memsize" and out.isdigit():
                out = f"{int(out) // 2**30} GB"
            if out:
                parts.append(out)
    return ", ".join(parts)


def sidecar_description(sidecar: Path) -> str:
    info_path = sidecar.parent / "BUILD_INFO.json"
    desc = f"`{sidecar}` sha256 `{build_mod.sha256(sidecar)[:16]}`"
    if info_path.is_file() and sidecar.resolve() == STAGED_SIDECAR.resolve():
        info = json.loads(info_path.read_text())
        desc += f", built from `{info['commit'][:10]}`{' (dirty)' if info['dirty'] else ''} with `{info['features']}` at {info['built_at']}"
    return desc


MD_COLUMNS = [
    ("Model", "model"), ("EP asked", "accel_requested"), ("EP loaded", "accel_loaded"),
    ("Corpus", "corpus"), ("Clips", "clips"), ("Runs", "scored_runs"), ("Steady RTFx", "rtfx_steady"),
    ("Min–max", None), ("Cold RTFx", "rtfx_cold"), ("Load s", "load_s"), ("CPU user s", "cpu_user_s"),
    ("Peak RSS MB", "peak_rss_mb"), ("WER %", "wer"), ("CER %", "cer"),
    ("Encoder placement", "placement_encoder"), ("vs expected", None),
]


def markdown(rows, args, sidecar: Path) -> str:
    lines = [
        f"# ASR bench, {datetime.now().strftime('%Y-%m-%d %H:%M')}",
        "",
        f"- Host: {host_description()}",
        f"- Load average at start: {args.loadavg}. Other work on the machine skews RTFx; compare runs from a quiet machine only.",
        f"- Sidecar: {sidecar_description(sidecar)}",
        f"- Warm-up transcribes per process (excluded): {args.warmup}; repeats per clip: {args.repeat}; normaliser: `{args.normalizer}`",
        "- RTFx = audio seconds / sidecar inference seconds over the scored runs. CPU time and peak RSS are for the whole process (load + every run).",
        "",
        "| " + " | ".join(h for h, _ in MD_COLUMNS) + " |",
        "|" + "---|" * len(MD_COLUMNS),
    ]
    for row in rows:
        cells = []
        for header, key in MD_COLUMNS:
            if header == "Min–max":
                cells.append(f"{row['rtfx_min']}–{row['rtfx_max']}" if row["rtfx_min"] != "" else "")
            elif header == "vs expected":
                cells.append(f"{row['rtfx_vs_expected']} of {row['expected_rtfx']}" if row["expected_rtfx"] else "")
            else:
                v = row[key]
                if key == "accel_loaded" and row["accel_fallback"]:
                    v = f"{v} (fallback: {row['accel_fallback']})"
                elif key == "accel_loaded" and row["accel_partial"] is True:
                    v = f"{v} (partial)"
                cells.append(str(v))
        lines.append("| " + " | ".join(cells) + " |")
    return "\n".join(lines) + "\n"


def cmd_run(args) -> int:
    models = parse_kv(args.model, "model")
    args.expect = parse_kv(args.expect, "expect")
    accels = [a for spec in args.accel for a in spec.split(",") if a]
    only = {o for spec in args.only for o in spec.split(",") if o}
    sidecar = Path(args.sidecar).resolve() if args.sidecar else STAGED_SIDECAR
    if not sidecar.is_file():
        raise SystemExit(f"no sidecar at {sidecar}; run `uv run asr-bench build` or pass --sidecar")
    if "src-tauri/binaries" in sidecar.as_posix() or "/target/debug/" in sidecar.as_posix():
        print(f"warning: {sidecar} is not a fresh release build; timings will not be comparable", file=sys.stderr)
    for name, d in models.items():
        if not Path(d).expanduser().is_dir():
            raise SystemExit(f"model {name}: not a directory: {d}")

    corpora = [parse_spec(spec, CACHE) for spec in args.corpus]
    args.loadavg = loadavg()
    cores = os.cpu_count() or 1
    if args.loadavg and float(args.loadavg.split()[0]) > cores / 4:
        print(f"warning: load average {args.loadavg} on {cores} cores; timings will be noisy", file=sys.stderr)
    out = Path(args.out).resolve() if args.out else TOOL_DIR / "results" / datetime.now().strftime("%Y%m%d-%H%M%S")
    logs = out / "logs"
    logs.mkdir(parents=True, exist_ok=True)
    placement_ok = supports_placement(sidecar)
    if not placement_ok:
        print("note: this sidecar has no YAPSTACK_ORT_PLACEMENT support; placement columns will read n/a", flush=True)

    rows, utterances = [], []
    for name, d in models.items():
        for accel in accels:
            if only and f"{name}:{accel}" not in only:
                continue
            r, u = run_row(args, sidecar, name, Path(d).expanduser().resolve(), accel, corpora, logs, placement_ok)
            rows += r
            utterances += u

    with open(out / "results.csv", "w", newline="") as fh:
        w = csv.DictWriter(fh, fieldnames=CSV_FIELDS)
        w.writeheader()
        w.writerows(rows)
    with open(out / "utterances.csv", "w", newline="") as fh:
        w = csv.DictWriter(fh, fieldnames=list(utterances[0]) if utterances else ["model"])
        w.writeheader()
        w.writerows(utterances)
    md = markdown(rows, args, sidecar)
    (out / "results.md").write_text(md)
    print("\n" + md)
    print(f"wrote {out}/results.csv, results.md, utterances.csv, logs/")

    misses = [r for r in rows if r["rtfx_vs_expected"] and abs(float(r["rtfx_vs_expected"].rstrip("%")) / 100) > args.tolerance]
    for r in misses:
        print(f"OUTSIDE ±{args.tolerance:.0%}: {r['model']}:{r['accel_requested']} {r['rtfx_steady']} vs {r['expected_rtfx']}", file=sys.stderr)
    return 1 if misses else 0


def cmd_build(args) -> int:
    build_mod.build(REPO, STAGED_SIDECAR.parent, args.features or build_mod.default_features())
    return 0


def cmd_fetch(args) -> int:
    for spec in args.corpus:
        c = parse_spec(spec, CACHE)
        print(f"{c.name}: {len(c.clips)} clips, {sum(x.seconds for x in c.clips):.0f} s")
    return 0


def main(argv=None) -> int:
    p = argparse.ArgumentParser(prog="asr-bench", description=__doc__)
    sub = p.add_subparsers(dest="cmd", required=True)

    b = sub.add_parser("build", help="cargo build --release the sidecar and stage it in .cache/bin")
    b.add_argument("--features", help=f"cargo features (default here: {build_mod.default_features()})")
    b.set_defaults(fn=cmd_build)

    f = sub.add_parser("fetch", help="download and cache public corpora without running anything")
    f.add_argument("--corpus", action="append", required=True)
    f.set_defaults(fn=cmd_fetch)

    r = sub.add_parser("run", help="run every model × accel row over the corpora")
    r.add_argument("--model", action="append", required=True, metavar="NAME=DIR", help="a Parakeet model dir; repeatable")
    r.add_argument("--accel", action="append", default=[], metavar="EP[,EP]", help="YAPSTACK_PARAKEET_ACCEL values: auto, cpu, webgpu, coreml (default: cpu)")
    r.add_argument("--only", action="append", default=[], metavar="NAME:EP[,..]", help="restrict the model × accel product to these rows")
    r.add_argument("--corpus", action="append", required=True, metavar="SPEC", help="dir:PATH or librispeech:SPLIT:N; repeatable")
    r.add_argument("--sidecar", help=f"sidecar binary (default: {STAGED_SIDECAR})")
    r.add_argument("--warmup", type=int, default=1, help="excluded transcribes of the first clip per process (default 1)")
    r.add_argument("--repeat", type=int, default=1, help="scored transcribes per clip (default 1)")
    r.add_argument("--normalizer", choices=sorted(metrics.NORMALIZERS), default="english")
    r.add_argument("--no-placement", action="store_true", help="skip the YAPSTACK_ORT_PLACEMENT load pass")
    r.add_argument("--expect", action="append", default=[], metavar="NAME:EP=RTFX", help="reference steady RTFx; exit 1 if outside --tolerance")
    r.add_argument("--tolerance", type=float, default=0.10)
    r.add_argument("--out", help="output dir (default: results/<timestamp>, gitignored)")
    r.set_defaults(fn=cmd_run)

    args = p.parse_args(argv)
    if args.cmd == "run" and not args.accel:
        args.accel = ["cpu"]
    return args.fn(args)


if __name__ == "__main__":
    sys.exit(main())
