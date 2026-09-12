# Releasing `tokentrimmer` to PyPI

The release workflow is configured to publish the Python SDK to PyPI as
[`tokentrimmer`](https://pypi.org/project/tokentrimmer/) via the [`release-pypi.yml`](../.github/workflows/release-pypi.yml) workflow,
which triggers on a `py-v*` tag and authenticates via **PyPI Trusted Publishing
(OIDC)** — there is **no API token to store**.

A read-only registry check on 2026-09-12 returned HTTP 404 for this package.
Clean-installed local wheels are verified; registry publication and provenance
are still pending. A 404 does not establish ownership or reserve the name.

## One-time setup (USER-GATED — do this once before the first release)

1. **Reserve the project name.** Confirm `tokentrimmer` is available (or already
   owned by you) on PyPI: https://pypi.org/project/tokentrimmer/. If it is taken
   by someone else, pick a new `name` in `pyproject.toml` and in
   `sdk-python/tokentrimmer/__init__.py` (`__version__` is separate) before
   continuing.

2. **Create the GitHub `pypi` environment.** In the repo:
   *Settings → Environments → New environment* named exactly **`pypi`**
   (the workflow's `environment: pypi` gate). Optionally add required reviewers
   so a human must approve each publish.

3. **Configure the PyPI Trusted Publisher.** On PyPI:
   *Your projects → tokentrimmer → Settings → Publishing*
   (or, for the very first publish, *Account → Publishing → Add a pending
   publisher*). Add a **GitHub Actions** publisher with exactly:

   | Field            | Value                       |
   | ---------------- | --------------------------- |
   | Owner            | `TokenTrimmer`              |
   | Repository       | `tokentrimmer`              |
   | Workflow name    | `release-pypi.yml`          |
   | Environment name | `pypi`                      |

   (For the first-ever upload, use a **pending publisher** — PyPI will create the
   project on the first successful run. After that it becomes a normal publisher.)

## Cutting a release

1. Bump the version in **both**:
   - `sdk-python/pyproject.toml` → `[project] version`
   - `sdk-python/tokentrimmer/__init__.py` → `__version__`
   (keep them in sync; the workflow only asserts the *tag* matches `pyproject.toml`).
2. Commit on `main`.
3. Tag and push: the tag is `py-v<version>` and **must** match `pyproject.toml`
   exactly (the workflow fails the build otherwise).

   ```bash
   git tag py-v0.1.0
   git push origin py-v0.1.0
   ```

The workflow then verifies the tag/version match, builds the sdist + wheel with
`python -m build`, asserts `tokentrimmer/py.typed` shipped in the wheel, and
publishes to PyPI via `pypa/gh-action-pypi-publish` (OIDC).

## Local verification (no publish)

```bash
python -m build --outdir dist sdk-python   # builds sdist + wheel
python -m twine check dist/*               # validates metadata + README rendering
uv run --no-project python sdk-python/scripts/verify-installed.py
```

The installed check builds a wheel and tests isolated environments with OpenAI
1.70.0, latest supported 1.x and latest supported 2.x. It uses in-process HTTP
fixtures for metadata, output limits, native raw helpers and stream closure;
`python -I` prevents accidentally testing source instead of the installed wheel.
This is not PyPI publication/provenance or hosted endpoint acceptance.

> Do **not** run `twine upload` locally — publishing is the workflow's job via
> Trusted Publishing, and a manual upload would not have provenance.
