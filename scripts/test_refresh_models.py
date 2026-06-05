"""Unit tests for refresh_models pure logic (no network). Run: python3 scripts/test_refresh_models.py"""
import sys
import tomllib
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from refresh_models import Drift, apply_window_fixes, detect_window_drift, slug_for  # noqa: E402


class TestSlug(unittest.TestCase):
    def test_openrouter_rows_map_to_themselves(self):
        self.assertEqual(
            slug_for("openrouter", "anthropic/claude-sonnet-4-6"),
            "anthropic/claude-sonnet-4-6",
        )

    def test_native_uses_slug_map(self):
        self.assertEqual(slug_for("openai", "gpt-4o"), "openai/gpt-4o")

    def test_unmapped_is_none(self):
        self.assertIsNone(slug_for("groq", "mixtral-8x7b-32768"))


class TestDetect(unittest.TestCase):
    def test_detects_only_changed_known_models(self):
        rows = [
            {"provider": "openai", "model": "gpt-4o", "max_input_tokens": 128000},
            {"provider": "anthropic", "model": "claude-haiku-4-5", "max_input_tokens": 200000},
            {"provider": "groq", "model": "mixtral-8x7b-32768", "max_input_tokens": 32768},
        ]
        ctx = {"openai/gpt-4o": 130000, "anthropic/claude-haiku-4.5": 200000}
        drift = detect_window_drift(rows, ctx)
        self.assertEqual(drift, [Drift("openai", "gpt-4o", 128000, 130000)])

    def test_missing_from_source_is_not_drift(self):
        rows = [{"provider": "openai", "model": "gpt-4o", "max_input_tokens": 128000}]
        self.assertEqual(detect_window_drift(rows, {}), [])


class TestApply(unittest.TestCase):
    SAMPLE = (
        "# header comment\n\n"
        '[[model]]\nprovider = "openai"\nmodel = "gpt-4o"\n'
        'max_input_tokens = 128000\nmax_output_tokens = 16000\ncapabilities = ["text"]\n\n'
        '[[model]]\nprovider = "openai"\nmodel = "gpt-4o-mini"\n'
        'max_input_tokens = 128000\nmax_output_tokens = 16000\ncapabilities = ["text"]\n'
    )

    def test_rewrites_only_the_target_block_and_preserves_rest(self):
        out = apply_window_fixes(self.SAMPLE, [Drift("openai", "gpt-4o", 128000, 130000)])
        self.assertIn("max_input_tokens = 130000", out)
        self.assertEqual(out.count("max_input_tokens = 128000"), 1)  # only gpt-4o-mini left
        self.assertIn("# header comment", out)  # comments preserved
        parsed = tomllib.loads(out)
        self.assertEqual(parsed["model"][0]["max_input_tokens"], 130000)
        self.assertEqual(parsed["model"][1]["max_input_tokens"], 128000)

    def test_missing_block_raises(self):
        # A fix for a (provider, model) not present must fail loud, not no-op.
        with self.assertRaises(ValueError):
            apply_window_fixes(self.SAMPLE, [Drift("nope", "nope", 1, 2)])


if __name__ == "__main__":
    unittest.main()
