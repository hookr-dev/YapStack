"""Drive the transcription sidecar over its JSON-line IPC (stdin/stdout).

The wire format is `SidecarRequest` / `SidecarResponse` in
`crates/yapstack-common/src/types.rs`; this module only speaks it.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

PLACEMENT_ENV = "YAPSTACK_ORT_PLACEMENT"
_PLACEMENT_FIELD_RE = re.compile(r'(\w+)=("[^"]*"|\S+)')


@dataclass
class Transcribed:
    text: str
    infer_s: float
    wall_s: float
    error: str | None = None


@dataclass
class ProcessStats:
    load_s: float
    accel: str | None
    engine_info: dict
    cpu_user_s: float | None = None
    cpu_sys_s: float | None = None
    peak_rss_bytes: int | None = None
    exit_status: int | None = None


@dataclass
class SessionPlacement:
    session: str
    ep: str
    heavy_on_ep: str
    cpu_nodes: int | None
    raw: dict = field(default_factory=dict)

    @property
    def heavy_share(self) -> float | None:
        on, _, total = self.heavy_on_ep.partition("/")
        try:
            return int(on) / int(total) if int(total) else None
        except ValueError:
            return None


def child_env(accel: str, sidecar: Path, extra: dict[str, str] | None = None) -> dict[str, str]:
    env = dict(os.environ)
    # The sidecar's own default log filter keeps ORT at warn. An inherited
    # RUST_LOG / ORT_LOG would change timing (and ort trace aborts at exit).
    for k in ("RUST_LOG", "ORT_LOG", PLACEMENT_ENV, "YAPSTACK_PARAKEET_ACCEL"):
        env.pop(k, None)
    env["YAPSTACK_PARAKEET_ACCEL"] = accel
    env["NO_COLOR"] = "1"
    if sys.platform == "darwin":
        # A plain `cargo build` binary has no rpath to the Dawn dylib that
        # sits next to it; `asr-bench build` adds one, this covers --sidecar.
        env["DYLD_FALLBACK_LIBRARY_PATH"] = str(sidecar.parent)
    env.update(extra or {})
    return env


def supports_placement(sidecar: Path) -> bool:
    """Feature-detect `YAPSTACK_ORT_PLACEMENT` (added after the sidecar
    shipped): an older binary simply ignores the variable, so look for the
    name in the binary itself."""
    needle = PLACEMENT_ENV.encode()
    with open(sidecar, "rb") as fh:
        tail = b""
        while chunk := fh.read(1 << 20):
            if needle in tail + chunk:
                return True
            tail = chunk[-len(needle) :]
    return False


def parse_placement(stderr: str) -> list[SessionPlacement]:
    out = []
    for line in stderr.splitlines():
        if 'marker="ort_placement"' not in line and "marker=ort_placement" not in line:
            continue
        fields = {k: v.strip('"') for k, v in _PLACEMENT_FIELD_RE.findall(line)}
        cpu = fields.get("cpu_nodes")
        out.append(
            SessionPlacement(
                session=fields.get("session", "?"),
                ep=fields.get("ep", "?"),
                heavy_on_ep=fields.get("heavy_on_ep", "?"),
                cpu_nodes=int(cpu) if cpu and cpu.isdigit() else None,
                raw=fields,
            )
        )
    return out


class Sidecar:
    """One sidecar process. Requests go one at a time, so each response's
    timing belongs to exactly one request."""

    def __init__(self, sidecar: Path, model: Path, accel: str, stderr_path: Path, engine: str = "parakeet", extra_env=None):
        self._stderr = open(stderr_path, "wb")
        self._next_id = 1
        t0 = time.perf_counter()
        self.proc = subprocess.Popen(
            [str(sidecar), "--engine", engine, "--model", str(model)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self._stderr,
            env=child_env(accel, sidecar, extra_env),
        )
        # The model loads from --model before the IPC loop starts, so the
        # first answer to any request marks the end of the load.
        info = self.request({"type": "query_engine_info"})
        self.stats = ProcessStats(load_s=time.perf_counter() - t0, accel=info.get("accel"), engine_info=info)

    def request(self, msg: dict) -> dict:
        rid = self._next_id
        self._next_id += 1
        self.proc.stdin.write((json.dumps({**msg, "id": rid}) + "\n").encode())
        self.proc.stdin.flush()
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError(f"sidecar exited (status {self.proc.poll()}) before answering request {rid}")
            resp = json.loads(line)
            if resp.get("type") == "progress":
                continue
            if resp.get("id") == rid:
                return resp

    def transcribe(self, wav: Path) -> Transcribed:
        t0 = time.perf_counter()
        resp = self.request({"type": "transcribe", "audio_path": str(wav), "language": None})
        wall = time.perf_counter() - t0
        if resp.get("type") == "error":
            return Transcribed("", 0.0, wall, error=resp.get("message", "error"))
        # duration_ms is the sidecar's own inference time (WAV decode excluded).
        return Transcribed(resp.get("text", ""), resp["duration_ms"] / 1000.0, wall)

    def close(self) -> ProcessStats:
        try:
            self.proc.stdin.write(b'{"type":"shutdown"}\n')
            self.proc.stdin.close()
        except BrokenPipeError:
            pass
        status, usage = _wait(self.proc)
        self.proc.stdout.close()
        self._stderr.close()
        self.stats.exit_status = status
        if usage is not None:
            self.stats.cpu_user_s = usage.ru_utime
            self.stats.cpu_sys_s = usage.ru_stime
            # ru_maxrss is bytes on macOS, KiB on Linux.
            self.stats.peak_rss_bytes = usage.ru_maxrss * (1 if sys.platform == "darwin" else 1024)
        return self.stats


def _wait(proc: subprocess.Popen):
    """Reap the child ourselves so we get *its* rusage (peak RSS, CPU time)
    rather than the max over every child this process has had."""
    if hasattr(os, "wait4"):
        _, status, usage = os.wait4(proc.pid, 0)
        proc.returncode = os.waitstatus_to_exitcode(status)
        return proc.returncode, usage
    return proc.wait(), None


def probe_placement(sidecar: Path, model: Path, accel: str, stderr_path: Path) -> list[SessionPlacement]:
    """A separate load-only pass with placement logging on, so the timed
    pass runs exactly as the app does."""
    sc = Sidecar(sidecar, model, accel, stderr_path, extra_env={PLACEMENT_ENV: "1"})
    sc.close()
    return parse_placement(stderr_path.read_text(errors="replace"))
