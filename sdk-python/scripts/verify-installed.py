"""Build and clean-install the wheel against each supported OpenAI release line.

No publication or gateway/provider requests. uv installs dependencies in disposable
virtual environments; the tests use only an in-process httpx.MockTransport.
"""
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

sdk = Path(__file__).resolve().parents[1]


def run(*args, cwd=sdk):
    subprocess.run([str(arg) for arg in args], cwd=cwd, check=True)


with tempfile.TemporaryDirectory(prefix="tt-python-installed-") as temp:
    root = Path(temp)
    run("uv", "build", "--wheel", "--out-dir", root)
    wheel, = root.glob("*.whl")
    versions = sys.argv[1:] or ["==1.70.0", ">=1.109.1,<2", ">=2.41.1,<3"]
    for index, version in enumerate(versions):
        consumer = root / str(index)
        consumer.mkdir()
        run("uv", "venv", consumer / "venv")
        python = consumer / "venv" / ("Scripts/python.exe" if sys.platform == "win32" else "bin/python")
        run("uv", "pip", "install", "--python", python, wheel, "openai" + version)
        shutil.copy2(sdk / "test-installed" / "tests" / "client.py", consumer / "client.py")
        # -I ignores PYTHONPATH and the working directory; cannot import src/.
        run(python, "-I", "client.py", cwd=consumer)
