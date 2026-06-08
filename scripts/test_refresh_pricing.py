"""Unit tests for refresh-pricing pure logic (no network).

Run: python3 scripts/test_refresh_pricing.py

`refresh-pricing.py` is hyphenated, so it's loaded via importlib rather than a
plain import. Importing is side-effect-free — `main()` is `__main__`-guarded.
"""
import importlib.util
import tempfile
import unittest
from pathlib import Path

_SPEC = importlib.util.spec_from_file_location(
    "refresh_pricing", Path(__file__).resolve().parent / "refresh-pricing.py"
)
refresh_pricing = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(refresh_pricing)
per_m = refresh_pricing.per_m
latest_entries = refresh_pricing.latest_entries


class TestPerM(unittest.TestCase):
    def test_none_is_none(self):
        self.assertIsNone(per_m(None))

    def test_empty_string_is_none(self):
        self.assertIsNone(per_m(""))

    def test_zero_string_is_zero(self):
        self.assertEqual(per_m("0"), 0.0)

    def test_zero_int_is_zero(self):
        self.assertEqual(per_m(0), 0.0)

    def test_per_token_converts_to_per_million(self):
        # $0.000002 / token -> $2.00 / 1M tokens
        self.assertEqual(per_m("0.000002"), 2.0)

    def test_unparseable_is_none(self):
        self.assertIsNone(per_m("not-a-number"))


class TestLatestEntries(unittest.TestCase):
    def _catalog(self, body: str) -> Path:
        f = tempfile.NamedTemporaryFile(
            mode="w", suffix=".toml", delete=False, encoding="utf-8"
        )
        f.write(body)
        f.close()
        path = Path(f.name)
        self.addCleanup(lambda: path.unlink(missing_ok=True))
        return path

    def test_returns_only_newest_entry_per_model(self):
        path = self._catalog(
            """
[[entry]]
provider = "openai"
model = "gpt-4o"
effective_at = "2026-01-01"
input_per_million = 5.0

[[entry]]
provider = "openai"
model = "gpt-4o"
effective_at = "2026-05-01"
input_per_million = 2.5
"""
        )
        rows = latest_entries(path)
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["effective_at"], "2026-05-01")
        self.assertEqual(rows[0]["input_per_million"], 2.5)

    def test_distinct_models_both_returned_sorted(self):
        path = self._catalog(
            """
[[entry]]
provider = "openai"
model = "gpt-4o"
effective_at = "2026-01-01"

[[entry]]
provider = "anthropic"
model = "claude-haiku-4-5"
effective_at = "2026-01-01"
"""
        )
        rows = latest_entries(path)
        self.assertEqual(len(rows), 2)
        # sorted by (provider, model): anthropic before openai
        self.assertEqual(rows[0]["provider"], "anthropic")
        self.assertEqual(rows[1]["provider"], "openai")


if __name__ == "__main__":
    unittest.main()
