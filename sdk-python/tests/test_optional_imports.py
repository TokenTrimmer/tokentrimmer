"""The base package must import without the optional (langchain / otel) extras.

`import tokentrimmer` and the whole base API must work with neither the
`langchain` nor the `otel` extra installed. Since CI installs both (via the
`test` extra), we prove the invariant structurally in a clean subprocess: after
`import tokentrimmer`, neither `langchain_core` nor `opentelemetry` may appear in
`sys.modules` — i.e. the base import does not eagerly pull them in. The lazy
`__getattr__` and the `semconv` module are checked in-process.
"""

from __future__ import annotations

import subprocess
import sys

from tokentrimmer import semconv


def test_base_import_does_not_pull_in_optional_extras():
    code = (
        "import sys\n"
        "import tokentrimmer\n"
        "from tokentrimmer import TokenTrimmer, TokenTrimmerMeta, semconv\n"
        # The base import must not have imported the optional frameworks.
        "assert 'langchain_core' not in sys.modules, 'base import pulled in langchain'\n"
        "assert 'opentelemetry' not in sys.modules, 'base import pulled in opentelemetry'\n"
        # semconv is dependency-free and fully usable.
        "assert semconv.TT_SAVED_USD == 'tokentrimmer.saved_usd'\n"
        "print('ok')\n"
    )
    result = subprocess.run(
        [sys.executable, "-c", code],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "ok"


def test_semconv_is_importable_without_extras():
    # This module is pure string constants + a dict mapping — no third-party deps.
    assert semconv.GEN_AI_SYSTEM == "gen_ai.system"
    assert callable(semconv.cost_info_to_attributes)


def test_lazy_attribute_resolves_langchain_callback():
    # Touching the lazily-exported name imports the integration on demand
    # (langchain_core IS installed under the test extra).
    import tokentrimmer

    cb_cls = tokentrimmer.TokenTrimmerCostCallback
    from tokentrimmer.integrations.langchain import TokenTrimmerCostCallback

    assert cb_cls is TokenTrimmerCostCallback
    assert isinstance(tokentrimmer.BudgetExceeded, type)


def test_unknown_top_level_attribute_still_raises():
    import tokentrimmer

    try:
        tokentrimmer.does_not_exist  # noqa: B018
    except AttributeError:
        pass
    else:  # pragma: no cover
        raise AssertionError("expected AttributeError for unknown attribute")
