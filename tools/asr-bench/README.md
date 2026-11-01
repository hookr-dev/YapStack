# asr-bench

A dev tool that times and scores Parakeet model variants through the **real, release-built transcription sidecar**. It speaks the sidecar's existing JSON-line IPC (`SidecarRequest` / `SidecarResponse` in `crates/yapstack-common/src/types.rs`), exactly as the desktop app does, so the numbers come from the same code path users run.

For every model × execution-provider row it reports:

- **RTFx**: audio seconds divided by the sidecar's own inference time (`duration_ms` in the `transcription` reply), summed over the scored runs. The first `--warmup` transcribes in each process are excluded, and the first one is reported separately as the cold run.
- **Load time, CPU time and peak RSS** of the sidecar process. These come from `wait4` rusage, so they cover the whole process: model load plus every run.
- **The EP that loaded**: `accel` from `engine_info`, plus `accel_fallback` and `accel_partial` when the sidecar sends them.
- **Node placement**: a separate load-only pass with `YAPSTACK_ORT_PLACEMENT=1`, which reports the heavy (MatMul/Conv-family) ops on the EP per ORT session. This is feature-detected. A sidecar built before that variable existed shows `n/a`.
- **WER and CER**: corpus-level (total edits over total reference words), after the pinned Whisper / Open ASR Leaderboard normaliser (`--normalizer english`, the default, or `basic` for non-English references).

It writes `results.csv`, `results.json`, `results.md`, `utterances.csv` (per run: reference, hypothesis, timing) and the raw sidecar stderr under `results/<timestamp>/`. That folder is gitignored. Nothing it downloads, builds or writes is committed.

macOS and Linux only. Windows rows come from the canary builds instead.

## Why Python

The tool only drives a binary and scores text, and `jiwer` and the Whisper normaliser are the reference implementations for that. A Rust bench would either need its own Cargo workspace or join the root one. Linking parakeet-rs there would hit the `ort-sys` `links` conflict, and it would bypass the sidecar we actually ship.

## Setup

Needs [`uv`](https://docs.astral.sh/uv/) and, for `build`, the Rust toolchain. Every command below runs from `tools/asr-bench/`.

```bash
uv run asr-bench build     # cargo build --release the sidecar, stage it in .cache/bin with BUILD_INFO.json
```

Always bench a fresh release build. The copy in `apps/desktop/src-tauri/binaries/` can be months old (it matches `target/debug`) and runs about 1.7× slower. The tool warns if you point `--sidecar` at it.

## Models

Every variant is a directory in the layout parakeet-rs loads: `encoder*.onnx`, `decoder_joint-model*.onnx`, `vocab.txt`. Keep them outside the repo. The shipped int8 bundle is already in the app's data dir, `~/Library/Application Support/dev.yapstack.app/models/parakeet-tdt-v3-int8`. The others come from Hugging Face at pinned commits:

```bash
hf() { uvx --from 'huggingface_hub[cli]==0.35.3' hf download "$@"; }
M=~/asr-models

# fp32 (reference): encoder with 2.4 GB external data
hf istupakov/parakeet-tdt-0.6b-v3-onnx --revision 8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce \
  --include 'encoder-model.onnx*' 'decoder_joint-model.onnx' 'vocab.txt' --local-dir $M/fp32

# int4 (weight-only MatMulNBits)
hf efederici/parakeet-tdt-0.6b-v3-onnx-int4 --revision 9828da0021d81df726b3d5ddb2d6d04354942749 \
  --include 'encoder-model.int4.onnx' 'decoder_joint-model.int8.onnx' 'vocab.txt' --local-dir $M/int4

# fp16 encoder: parakeet-rs can't find its fp16 decoder, and its vocab.txt differs,
# so pair it with the istupakov int8 decoder and vocab
hf ysdede/parakeet-tdt-0.6b-v3-onnx --revision e5356bf6e24bdb7a269536b0e9e7869bc034b893 \
  --include 'encoder-model.fp16.onnx' --local-dir $M/fp16
hf istupakov/parakeet-tdt-0.6b-v3-onnx --revision 8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce \
  --include 'decoder_joint-model.int8.onnx' 'vocab.txt' --local-dir $M/fp16
```

## Corpora

`--corpus` is repeatable and takes either of two forms:

- **`librispeech:SPLIT:N`**: the first N utterances (in archive order) of `test-clean`, `test-other`, `dev-clean` or `dev-other`. The archive is streamed from openslr.org, and the download stops after N utterances. Clips are cached as 16 kHz WAV plus `.txt` under `.cache/librispeech/` (gitignored); `uv run asr-bench fetch --corpus …` pre-fetches them.
- **`dir:PATH`**: any folder of WAV files. A clip is scored when a `.txt` with the same stem sits next to it; a clip without one is timed only.

### Making an owner corpus

Put it in `tools/asr-bench/corpus/` (gitignored) or anywhere outside the repo. **Never commit audio.**

```
corpus/owner/
  meeting-01.wav      # any rate or channel count; the sidecar resamples to 16 kHz mono
  meeting-01.txt      # what was actually said, as plain text on one or more lines
  laptop-mic/
    standup-03.wav
    standup-03.txt
```

- **Audio.** Use WAV in PCM16 or float. Convert other formats first, for example `afconvert -f WAVE -d LEI16@16000 in.m4a out.wav` or `ffmpeg -i in.m4a -ar 16000 -ac 1 out.wav`.
- **Clip length.** Keep clips to roughly the length the live chunker sends (a few seconds up to about 10 s), or to whole short files if you're benchmarking import. The sidecar transcribes each file in one call.
- **References.** Write what was said, verbatim, and don't paraphrase. Casing and punctuation don't matter, since both sides are normalised, and numbers may be digits or words. Leave out speaker names and timestamps.
- **Scope.** Each subfolder is just part of the clip id; slice the results by `corpus` or by clip prefix in `utterances.csv`.

## Running

```bash
S=~/asr-models
uv run asr-bench run \
  --model int8="$HOME/Library/Application Support/dev.yapstack.app/models/parakeet-tdt-v3-int8" \
  --model fp16=$S/fp16 --model int4=$S/int4 --model fp32=$S/fp32 \
  --accel cpu,webgpu \
  --corpus librispeech:test-other:200 --corpus dir:corpus/owner
```

- `--accel` takes any `YAPSTACK_PARAKEET_ACCEL` value (`auto`, `cpu`, `webgpu`, `coreml`); the rows are model × accel.
- `--only int8:cpu,fp16:webgpu` restricts the rows to that list.
- `--repeat N` transcribes each clip N times. The hypothesis is taken from the first run, and the timing from all of them.
- The tool removes `RUST_LOG` and `ORT_LOG` from the sidecar's environment, so the sidecar runs with its own default log filter, as in the app.
- Run on a quiet machine. The report records the load average, and the tool warns when it is high: a parallel `cargo build` easily halves RTFx.

### Self-check

The 2026-10-31 probe (`docs/plans/research/q4-delivery/asr.md` §2.2) used a 16.2 s TTS clip, one cold plus five steady transcribes per process. `--expect` makes the run exit non-zero if a row's steady RTFx falls outside `--tolerance` (default ±10%) or has no measurement. Any failed transcription, warm-up included, marks its row `incomplete` (error count in every report) and also exits non-zero:

```bash
uv run asr-bench run --model int8=… --model fp16=… --model int4=… --accel cpu,webgpu \
  --corpus dir:/path/to/probe-clip --warmup 1 --repeat 5 \
  --expect int8:cpu=32.5 --expect int8:webgpu=16.9 --expect fp16:webgpu=42.5 --expect int4:webgpu=23
```

`int4:webgpu` was noisy in the probe (20–26×).

## Tests

```bash
uv run python -m unittest discover -s tests
```

The tests use a fake sidecar (`tests/fake_sidecar.py`) that speaks the same IPC, so they need no models.
