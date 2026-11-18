import re
import subprocess
import sys
from pathlib import Path

if (
    len(sys.argv) != 2
    or re.fullmatch(
        r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", sys.argv[1]
    )
    is None
):
    raise SystemExit("Expected a MAJOR.MINOR.PATCH version.")

path = Path("Cargo.toml")
content = path.read_text()
package = re.search(r"(?ms)^\[package\]\s*\n(.*?)(?=^\[|\Z)", content)
if package is None:
    raise SystemExit("Cargo.toml has no [package] section.")
updated, count = re.subn(
    r'^version = "[^"]+"$',
    f'version = "{sys.argv[1]}"',
    package[1],
    flags=re.MULTILINE,
)
if count != 1:
    raise SystemExit("Expected one version in Cargo.toml's [package] section.")
path.write_text(content[: package.start(1)] + updated + content[package.end(1) :])
subprocess.run(
    ["cargo", "metadata", "--offline", "--format-version", "1"],
    stdout=subprocess.DEVNULL,
    check=True,
    timeout=120,
)
