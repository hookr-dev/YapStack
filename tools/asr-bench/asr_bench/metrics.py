"""WER/CER with a fixed normaliser, and RTFx aggregation."""

from __future__ import annotations

from dataclasses import dataclass

import jiwer
from whisper_normalizer.basic import BasicTextNormalizer
from whisper_normalizer.english import EnglishTextNormalizer

# The Whisper / Open ASR Leaderboard normalisers, pinned in pyproject.toml so
# every run scores the same way. "english" also canonicalises numbers and
# British/American spellings; "basic" only lowercases and strips punctuation
# and is the one to use for non-English references.
NORMALIZERS = {
    "english": EnglishTextNormalizer(),
    "basic": BasicTextNormalizer(),
}


def normalize(text: str, kind: str) -> str:
    return " ".join(NORMALIZERS[kind](text).split())


@dataclass
class ErrorRates:
    wer: float | None
    cer: float | None
    scored: int
    skipped_empty_ref: int


def error_rates(pairs: list[tuple[str, str]], kind: str) -> ErrorRates:
    """Corpus-level WER/CER (total edits / total reference words), not a mean
    of per-utterance rates. `pairs` is (reference, hypothesis). Pairs whose
    reference normalises to nothing can't be scored and are counted apart."""
    refs, hyps, skipped = [], [], 0
    for ref, hyp in pairs:
        r = normalize(ref, kind)
        if not r:
            skipped += 1
            continue
        refs.append(r)
        hyps.append(normalize(hyp, kind))
    if not refs:
        return ErrorRates(None, None, 0, skipped)
    return ErrorRates(jiwer.wer(refs, hyps), jiwer.cer(refs, hyps), len(refs), skipped)


@dataclass
class Rtfx:
    steady: float | None
    run_min: float | None
    run_max: float | None
    scored_runs: int


def rtfx(runs: list[tuple[float, float]]) -> Rtfx:
    """`runs` is (audio seconds, inference seconds) for every scored
    transcribe. Steady RTFx is total audio over total inference time, so long
    clips weigh more than short ones, as they do in real use."""
    runs = [(a, s) for a, s in runs if s > 0]
    if not runs:
        return Rtfx(None, None, None, 0)
    per_run = [a / s for a, s in runs]
    total = sum(a for a, _ in runs) / sum(s for _, s in runs)
    return Rtfx(total, min(per_run), max(per_run), len(runs))
