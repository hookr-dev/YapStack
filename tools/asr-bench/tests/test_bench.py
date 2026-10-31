import csv
import tempfile
import unittest
from pathlib import Path

import numpy as np
import soundfile

from asr_bench import cli, metrics
from asr_bench.corpus import load_folder, parse_spec
from asr_bench.sidecar import Sidecar, parse_placement, supports_placement

FAKE = Path(__file__).with_name("fake_sidecar.py")


def write_wav(path: Path, seconds: float, rate: int = 16000):
    soundfile.write(str(path), np.zeros(int(seconds * rate), dtype=np.float32), rate, subtype="PCM_16")


class Metrics(unittest.TestCase):
    def test_normaliser_ignores_case_punctuation_and_number_style(self):
        self.assertEqual(metrics.normalize("Meet at 4:30, OK?", "english"), metrics.normalize("meet at 4 30 ok", "english"))

    def test_corpus_level_wer_weights_by_reference_words(self):
        e = metrics.error_rates([("the cat sat down", "the cat sat down"), ("big dog", "big")], "english")
        self.assertAlmostEqual(e.wer, 1 / 6)
        self.assertEqual(e.scored, 2)

    def test_empty_reference_is_skipped_not_scored(self):
        e = metrics.error_rates([("...", "hello")], "basic")
        self.assertIsNone(e.wer)
        self.assertEqual(e.skipped_empty_ref, 1)

    def test_steady_rtfx_is_total_audio_over_total_time(self):
        r = metrics.rtfx([(10.0, 1.0), (2.0, 1.0)])
        self.assertAlmostEqual(r.steady, 6.0)
        self.assertEqual((r.run_min, r.run_max), (2.0, 10.0))


class Placement(unittest.TestCase):
    LINE = ('2026-10-31T12:00:00Z  INFO yapstack_transcription_sidecar::engines::parakeet: ort node placement '
            'marker="ort_placement" session="encoder" ep="webgpu" heavy_on_ep=72/366 cpu_nodes=1014 '
            'nodes_by_ep={"CPUExecutionProvider": 1014, "WebGpuExecutionProvider": 900}')

    def test_parses_the_session_line(self):
        [s] = parse_placement("noise\n" + self.LINE + "\nmore noise")
        self.assertEqual((s.session, s.ep, s.heavy_on_ep, s.cpu_nodes), ("encoder", "webgpu", "72/366", 1014))
        self.assertAlmostEqual(s.heavy_share, 72 / 366)

    def test_feature_detect_reads_the_binary(self):
        with tempfile.TemporaryDirectory() as d:
            old, new = Path(d, "old"), Path(d, "new")
            old.write_bytes(b"\0" * 3_000_000)
            # Straddle the 1 MiB read boundary.
            new.write_bytes(b"\0" * ((1 << 20) - 5) + b"YAPSTACK_ORT_PLACEMENT" + b"\0" * 10)
            self.assertFalse(supports_placement(old))
            self.assertTrue(supports_placement(new))


class Corpus(unittest.TestCase):
    def test_folder_pairs_wav_with_same_stem_txt(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root / "sub").mkdir()
            write_wav(root / "a.wav", 1.5)
            (root / "a.txt").write_text("Hello there.\n")
            write_wav(root / "sub" / "b.wav", 0.5, rate=48000)
            c = parse_spec(f"dir:{root}", root / ".cache")
            self.assertEqual([x.id for x in c.clips], ["a", "sub/b"])
            self.assertEqual(c.clips[0].reference, "Hello there.")
            self.assertIsNone(c.clips[1].reference)
            self.assertAlmostEqual(c.clips[1].seconds, 0.5)

    def test_empty_folder_is_an_error(self):
        with tempfile.TemporaryDirectory() as d, self.assertRaises(SystemExit):
            load_folder("x", Path(d))


class EndToEnd(unittest.TestCase):
    def test_drives_the_ipc_and_writes_csv_and_markdown(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            corpus, model = root / "corpus", root / "model"
            corpus.mkdir()
            model.mkdir()
            write_wav(corpus / "a.wav", 2.0)
            (corpus / "a.txt").write_text("hello world")
            write_wav(corpus / "fail.wav", 1.0)
            out = root / "out"
            code = cli.main([
                "run", "--sidecar", str(FAKE), "--model", f"m={model}", "--accel", "cpu,webgpu",
                "--only", "m:webgpu", "--corpus", f"dir:{corpus}", "--repeat", "2",
                "--expect", "m:webgpu=8", "--out", str(out),
            ])
            self.assertEqual(code, 0)
            with open(out / "results.csv") as fh:
                [row] = list(csv.DictReader(fh))
            self.assertEqual(row["accel_loaded"], "webgpu")
            self.assertEqual(row["scored_runs"], "2")  # the failing clip is not timed
            self.assertEqual(row["errors"], "2")
            self.assertEqual(row["rtfx_steady"], "8.0")
            self.assertEqual(row["wer"], "0.0")
            # The fake names YAPSTACK_ORT_PLACEMENT, so it is detected as supporting it.
            self.assertEqual(row["placement_encoder"], "webgpu heavy 72/366 (19.7%)")
            self.assertEqual(row["placement_decoder_joint"], "not reported")
            self.assertIn("| m | webgpu | webgpu |", (out / "results.md").read_text())

    def test_rtfx_outside_tolerance_fails_the_run(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            write_wav(root / "a.wav", 2.0)
            code = cli.main(["run", "--sidecar", str(FAKE), "--model", f"m={root}", "--corpus", str(root),
                             "--expect", "m:cpu=20", "--out", str(root / "out")])
            self.assertEqual(code, 1)

    def test_placement_pass_reads_the_session_logger(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            sc = Sidecar(FAKE, root, "webgpu", root / "err", extra_env={"YAPSTACK_ORT_PLACEMENT": "1"})
            stats = sc.close()
            self.assertEqual(stats.accel, "webgpu")
            self.assertEqual(stats.exit_status, 0)
            [s] = parse_placement((root / "err").read_text())
            self.assertEqual(s.heavy_on_ep, "72/366")


if __name__ == "__main__":
    unittest.main()
