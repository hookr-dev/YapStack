"""Corpora: a local folder of WAV + reference text, or a cached public slice.

A folder corpus is every `*.wav` under a directory (recursively). A clip's
reference is the `.txt` file with the same stem next to it; a clip without
one is still timed, just not scored.
"""

from __future__ import annotations

import io
import json
import re
import tarfile
import urllib.request
from dataclasses import dataclass
from pathlib import Path

import soundfile

LIBRISPEECH_URL = "https://www.openslr.org/resources/12/{split}.tar.gz"
LIBRISPEECH_SPLITS = ("test-clean", "test-other", "dev-clean", "dev-other")
_FLAC_RE = re.compile(r"^LibriSpeech/[\w-]+/(\d+)/(\d+)/(\d+-\d+-\d+)\.flac$")
_TRANS_RE = re.compile(r"^LibriSpeech/[\w-]+/(\d+)/(\d+)/\d+-\d+\.trans\.txt$")
_UTT_ID_RE = re.compile(r"^\d+-\d+-\d+$")


@dataclass(frozen=True)
class Clip:
    id: str
    path: Path
    seconds: float
    reference: str | None


@dataclass(frozen=True)
class Corpus:
    name: str
    clips: list[Clip]


def load_folder(name: str, root: Path) -> Corpus:
    root = root.expanduser().resolve()
    if not root.is_dir():
        raise SystemExit(f"corpus folder not found: {root}")
    clips = []
    for wav in sorted(root.rglob("*.wav")):
        ref_path = wav.with_suffix(".txt")
        ref = ref_path.read_text(encoding="utf-8").strip() if ref_path.is_file() else None
        info = soundfile.info(str(wav))
        clips.append(
            Clip(
                id=wav.relative_to(root).with_suffix("").as_posix(),
                path=wav,
                seconds=info.frames / info.samplerate,
                reference=ref,
            )
        )
    if not clips:
        raise SystemExit(f"no .wav files under {root}")
    return Corpus(name, clips)


def fetch_librispeech(cache: Path, split: str, count: int) -> Path:
    """The first `count` utterances of a LibriSpeech split, in archive order,
    as 16 kHz PCM16 WAV + `.txt` under `cache`. The archive is streamed and
    the download stops once `count` utterances have both audio and text, so a
    small slice costs a small download. Re-runs reuse the cache."""
    if split not in LIBRISPEECH_SPLITS:
        raise SystemExit(f"unknown LibriSpeech split {split!r}; one of {LIBRISPEECH_SPLITS}")
    out = cache / "librispeech" / split
    manifest = out / "manifest.json"
    if manifest.is_file() and json.loads(manifest.read_text())["count"] >= count:
        return out
    out.mkdir(parents=True, exist_ok=True)

    audio: dict[str, Path] = {}
    texts: dict[str, str] = {}
    order: list[str] = []
    url = LIBRISPEECH_URL.format(split=split)
    print(f"fetching {count} utterances of LibriSpeech {split} from {url}")
    with urllib.request.urlopen(url) as resp, tarfile.open(fileobj=resp, mode="r|gz") as tar:
        for member in tar:
            if not member.isfile():
                continue
            if m := _FLAC_RE.match(member.name):
                utt = m.group(3)
                # Our own path, never the archive's: nothing outside `out`.
                wav = out / f"{utt}.wav"
                data, rate = soundfile.read(io.BytesIO(tar.extractfile(member).read()))
                soundfile.write(str(wav), data, rate, subtype="PCM_16")
                audio[utt] = wav
                order.append(utt)
            elif _TRANS_RE.match(member.name):
                for line in tar.extractfile(member).read().decode("utf-8").splitlines():
                    utt, _, text = line.partition(" ")
                    if _UTT_ID_RE.match(utt):
                        texts[utt] = text.strip()
            if sum(1 for u in order if u in texts) >= count:
                break

    kept = [u for u in order if u in texts][:count]
    for utt in kept:
        (out / f"{utt}.txt").write_text(texts[utt] + "\n", encoding="utf-8")
    for utt in set(audio) - set(kept):
        audio[utt].unlink(missing_ok=True)
    manifest.write_text(json.dumps({"split": split, "url": url, "count": len(kept), "utterances": kept}, indent=1))
    return out


def load_librispeech(cache: Path, split: str, count: int) -> Corpus:
    folder = fetch_librispeech(cache, split, count)
    kept = json.loads((folder / "manifest.json").read_text())["utterances"][:count]
    by_id = {c.id: c for c in load_folder(split, folder).clips}
    return Corpus(f"librispeech-{split}-{count}", [by_id[u] for u in kept])


def parse_spec(spec: str, cache: Path) -> Corpus:
    """`dir:PATH`, `librispeech:SPLIT:N`, or a bare path (same as `dir:`)."""
    kind, sep, rest = spec.partition(":")
    if sep and kind == "librispeech":
        split, _, n = rest.partition(":")
        return load_librispeech(cache, split, int(n or 100))
    if sep and kind == "dir":
        path = Path(rest)
    else:
        path = Path(spec)
    return load_folder(path.name or str(path), path)
