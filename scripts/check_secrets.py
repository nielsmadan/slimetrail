import argparse
import os
import shutil
import subprocess
import tempfile
from pathlib import Path


def scan(root, mode):
    with tempfile.TemporaryDirectory(prefix="slimetrail-secrets-") as directory:
        temporary = Path(directory)
        config = temporary / "gitleaks.toml"
        config.write_text("[extend]\nuseDefault = true\n")
        ignore = temporary / "empty-ignore"
        ignore.write_text("")
        environment = dict(os.environ)
        environment.pop("GITLEAKS_CONFIG", None)
        environment.pop("GITLEAKS_CONFIG_TOML", None)
        flags = [
            "--config",
            str(config),
            "--gitleaks-ignore-path",
            str(ignore),
            "--ignore-gitleaks-allow",
            "--redact=100",
            "--no-banner",
            "--no-color",
        ]
        if mode == "worktree":
            source = temporary / "source"
            source.mkdir()
            names = subprocess.check_output(
                ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
                cwd=root,
                timeout=30,
            )
            for name in set(filter(None, names.split(b"\0"))):
                relative = Path(os.fsdecode(name))
                path = root / relative
                if path.is_symlink() or not path.is_file():
                    continue
                target = source / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(path, target)
            command = ["gitleaks", "dir", str(source), *flags]
        else:
            extra = (
                ["--pre-commit", "--staged"]
                if mode == "staged"
                else ["--log-opts=--all"]
            )
            command = ["gitleaks", "git", str(root), *flags, *extra]
        return subprocess.run(
            command, env=environment, cwd=temporary, timeout=240, check=False
        ).returncode


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=["worktree", "staged", "history"])
    args = parser.parse_args()
    raise SystemExit(scan(Path(__file__).resolve().parents[1], args.mode))
