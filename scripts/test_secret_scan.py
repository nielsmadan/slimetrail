import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from check_secrets import scan


class SecretScanTests(unittest.TestCase):
    def test_worktree_snapshot_excludes_ignored_files_and_symlinks(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", str(root)], check=True, timeout=10)
            (root / ".gitignore").write_text(".local/\n")
            (root / "source.rs").write_text("public fixture")
            (root / ".local").mkdir()
            (root / ".local/private").write_text("private fixture")
            (root / "link").symlink_to(root / ".local/private")

            def inspect(command, **kwargs):
                if command[0] != "gitleaks":
                    return original(command, **kwargs)
                source = Path(command[2])
                self.assertEqual(
                    sorted(
                        p.relative_to(source).as_posix()
                        for p in source.rglob("*")
                        if p.is_file()
                    ),
                    [".gitignore", "source.rs"],
                )
                self.assertEqual((source / "source.rs").read_text(), "public fixture")
                self.assertIn("--redact=100", command)
                return subprocess.CompletedProcess(command, 7)

            original = subprocess.run
            with patch("check_secrets.subprocess.run", side_effect=inspect):
                self.assertEqual(scan(root, "worktree"), 7)


if __name__ == "__main__":
    unittest.main()
