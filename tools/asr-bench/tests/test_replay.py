import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import numpy as np
import soundfile

from asr_bench import cli, metrics, replay

FAKE = Path(__file__).with_name("fake_sidecar.py")
FAKE_HARNESS = Path(__file__).with_name("fake_harness.py")


def tone(seconds: float, rate: int = 16000) -> np.ndarray:
    t = np.arange(int(seconds * rate)) / rate
    return (0.3 * np.sin(2 * np.pi * 220 * t)).astype(np.float32)


def row(key, wer, empty=0, truncated=0, ts=0, slice_="long", mode="live"):
    return {"key": key, "slice": slice_, "mode": mode, "wer": wer, "empty": empty, "truncated": truncated, "segment_ts_bad": ts}


class Truncation(unittest.TestCase):
    def test_longest_run_of_dropped_reference_words(self):
        ref = "the cat sat on a mat by the door"
        self.assertEqual(metrics.longest_deletion_run(ref, "the cat sat the door", "english"), 4)
        self.assertEqual(metrics.longest_deletion_run(ref, "the sat on mat by the door", "english"), 1)
        self.assertEqual(metrics.longest_deletion_run(ref, ref, "english"), 0)

    def test_empty_hypothesis_drops_everything(self):
        self.assertEqual(metrics.longest_deletion_run("a quick test", "", "english"), 3)


class Timestamps(unittest.TestCase):
    def test_segments_must_be_present_ordered_and_inside_the_chunk(self):
        seg = lambda s, e: {"start_ms": s, "end_ms": e}
        self.assertTrue(replay.segment_ts_ok([seg(0, 500), seg(500, 1040)], 1000))  # within the 50 ms slack
        self.assertFalse(replay.segment_ts_ok([seg(0, 1100)], 1000))
        self.assertFalse(replay.segment_ts_ok([seg(500, 900), seg(100, 200)], 1000))
        self.assertFalse(replay.segment_ts_ok([], 1000))


class Gate(unittest.TestCase):
    def test_within_a_point_and_no_more_failures_passes(self):
        checks = replay.gate([row("int8:auto", 5.0, 3, 2), row("fa:auto", 5.9, 3, 1)], "int8:auto", 1.0)
        self.assertEqual([(c["key"], c["pass"]) for c in checks], [("fa:auto", True)])

    def test_wer_over_the_margin_fails(self):
        [c] = replay.gate([row("int8:auto", 5.0), row("fa:auto", 6.01)], "int8:auto", 1.0)
        self.assertFalse(c["pass"])
        self.assertIn("WER +1.01", c["why"])

    def test_more_empty_truncated_or_bad_timestamps_fails_on_any_slice(self):
        rows = [row("int8:auto", 5.0, 1, 1, 0), row("fa:auto", 4.0, 1, 1, 0),
                row("int8:auto", 5.0, 1, 1, 0, mode="backfill"), row("fa:auto", 4.0, 2, 2, 1, mode="backfill")]
        checks = replay.gate(rows, "int8:auto", 1.0)
        self.assertEqual([c["pass"] for c in checks], [True, False])
        self.assertEqual(checks[1]["why"], "empty 2 > 1; truncated 2 > 1; segment_ts_bad 1 > 0")

    def test_a_slice_the_candidate_did_not_run_fails(self):
        rows = [row("int8:auto", 5.0), row("int8:auto", 5.0, mode="backfill"), row("fa:auto", 5.0)]
        self.assertEqual([c["pass"] for c in replay.gate(rows, "int8:auto", 1.0)], [True, False])


class BuildCorpus(unittest.TestCase):
    def test_groups_three_utterances_per_chapter_into_each_slice(self):
        with tempfile.TemporaryDirectory() as d:
            cache, out = Path(d, "cache"), Path(d, "corpus")
            for split, count, utts in (("test-clean", 400, ["1-2-0000", "1-2-0001", "1-2-0002", "1-2-0003", "3-4-0000"]),
                                       ("test-other", 200, ["5-6-0000", "5-6-0001", "5-6-0002"])):
                folder = cache / "librispeech" / split
                folder.mkdir(parents=True)
                (folder / "manifest.json").write_text(json.dumps({"count": count, "utterances": utts}))
                for u in utts:
                    gap = np.zeros(8000, dtype=np.float32)
                    soundfile.write(str(folder / f"{u}.wav"), np.concatenate([gap, tone(7.0), gap]), 16000, subtype="PCM_16")
                    (folder / f"{u}.txt").write_text(f"WORDS OF {u}\n")
            info = replay.build_corpus(cache, out)
            self.assertEqual(info["version"], replay.CORPUS_VERSION)
            # 1-2 has one full group (0000-0002; 0003 is left over), 3-4 none, 5-6 one.
            self.assertEqual(info["clips"], {"long": 2, "long44": 2, "mono": 2, "drift": 0})
            self.assertEqual(json.loads((out / "corpus.json").read_text()), info)
            name = "1-2-0000_to_0002"
            self.assertEqual((out / "long" / f"{name}.txt").read_text(), "WORDS OF 1-2-0000 WORDS OF 1-2-0001 WORDS OF 1-2-0002\n")
            long_info = soundfile.info(str(out / "long" / f"{name}.wav"))
            mono_info = soundfile.info(str(out / "mono" / f"{name}.wav"))
            hi_info = soundfile.info(str(out / "long44" / f"{name}.wav"))
            self.assertAlmostEqual(long_info.duration, 24.0, places=2)
            self.assertLess(mono_info.duration, 21.2)  # the pauses are gone
            self.assertEqual((hi_info.samplerate, hi_info.channels), (44100, 2))
            self.assertAlmostEqual(hi_info.duration, 24.0, places=2)


class EndToEnd(unittest.TestCase):
    def corpus(self, root: Path, names=("a", "b", "c")) -> Path:
        corpus = root / "corpus"
        (corpus / "long").mkdir(parents=True)
        (corpus / "corpus.json").write_text(json.dumps({"version": "test-v1", "source": ["fake"], "clips": {"long": 3}}))
        for name in names:
            soundfile.write(str(corpus / "long" / f"{name}.wav"), tone(2.0), 16000, subtype="PCM_16")
            (corpus / "long" / f"{name}.txt").write_text("hello world hello world")
        return corpus

    def run_replay(self, root: Path, *extra: str, names=("a", "b", "c")) -> int:
        with mock.patch.object(replay, "harness_command", return_value=[sys.executable, str(FAKE_HARNESS)]):
            return cli.main(["replay", "--sidecar", str(FAKE), "--corpus-dir", str(self.corpus(root, names)), "--slice", "long",
                             "--mode", "live,backfill", "--limit", "2", "--out", str(root / "out"), *extra])

    def test_a_clip_with_no_chunks_scores_as_fully_deleted(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            code = self.run_replay(root, "--model", f"m={root}", names=("a", "b_nochunks"))
            self.assertEqual(code, 0)
            r = json.loads((root / "out" / "replay.json").read_text())["rows"][0]
            self.assertEqual((r["clips"], r["chunks"], r["truncated"], r["wer"]), (2, 3, 1, 50.0))

    def test_a_sidecar_error_fails_the_run(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            code = self.run_replay(root, "--model", f"m={root}", names=("a", "b_fail"))
            self.assertEqual(code, 1)
            r = json.loads((root / "out" / "replay.json").read_text())["rows"][0]
            self.assertEqual((r["errors"], r["empty"]), (2, 2))

    def test_scores_every_model_on_the_same_chunks_and_passes_the_gate(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root / "int8").mkdir()
            (root / "fa").mkdir()
            code = self.run_replay(root, "--model", f"int8={root / 'int8'}", "--model", f"fa={root / 'fa'}", "--baseline", "int8:auto")
            self.assertEqual(code, 0)
            result = json.loads((root / "out" / "replay.json").read_text())
            self.assertEqual(result["limit"], 2)
            self.assertEqual(len(result["rows"]), 4)  # 2 models x 2 modes
            r = result["rows"][0]
            self.assertEqual((r["slice"], r["mode"], r["key"], r["accel_loaded"]), ("long", "live", "int8:auto", "auto"))
            self.assertEqual((r["clips"], r["chunks"], r["skipped"], r["wer"]), (2, 6, 2, 0.0))
            self.assertEqual((r["empty"], r["truncated"], r["segment_ts_bad"], r["errors"]), (0, 0, 0, 0))
            self.assertTrue(all(c["pass"] for c in result["gate"]))
            md = (root / "out" / "replay.md").read_text()
            self.assertIn("`test-v1, first 2 clips per slice`", md)
            self.assertIn("AE-G1: PASS", md)

    def test_a_model_that_loses_speech_fails_the_gate(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root / "int8").mkdir()
            (root / "mute").mkdir()
            code = self.run_replay(root, "--model", f"int8={root / 'int8'}", "--model", f"mute={root / 'mute'}", "--baseline", "int8:auto")
            self.assertEqual(code, 1)
            result = json.loads((root / "out" / "replay.json").read_text())
            mute = [r for r in result["rows"] if r["key"] == "mute:auto"][0]
            self.assertEqual((mute["empty"], mute["truncated"], mute["wer"]), (4, 2, 100.0))
            self.assertFalse(any(c["pass"] for c in result["gate"]))
            self.assertIn("AE-G1: FAIL", (root / "out" / "replay.md").read_text())

    def test_a_failed_harness_stops_the_run(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            with mock.patch.object(replay, "harness_command", return_value=[sys.executable, "-c", "raise SystemExit(3)"]), \
                    self.assertRaises(SystemExit) as e:
                cli.main(["replay", "--sidecar", str(FAKE), "--model", f"m={root}", "--corpus-dir", str(self.corpus(root)),
                          "--slice", "long", "--out", str(root / "out")])
            self.assertIn("exit 3", str(e.exception))

    def test_unknown_baseline_is_rejected(self):
        with tempfile.TemporaryDirectory() as d, self.assertRaises(SystemExit):
            cli.main(["replay", "--sidecar", str(FAKE), "--model", f"m={d}", "--baseline", "m:cpu"])


if __name__ == "__main__":
    unittest.main()
