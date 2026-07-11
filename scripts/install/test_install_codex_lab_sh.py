#!/usr/bin/env python3

import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest


INSTALL_SCRIPT = Path(__file__).with_name("install-codex-lab.sh")


class InstallCodexLabShTest(unittest.TestCase):
    def test_installs_versioned_binary_and_shares_conversation_storage(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            fake_binary = write_fake_codex(root)

            result = run_installer(root, fake_binary)

            self.assertEqual(result.returncode, 0, result.stderr)
            lab_home = root / ".codex-lab"
            shared_home = root / ".codex"
            install_root = root / ".local" / "lib" / "codex-lab"
            launcher = root / ".local" / "bin" / "codex-lab"
            release = install_root / "releases" / "test-release"

            self.assertEqual(
                os.readlink(install_root / "current"), "releases/test-release"
            )
            self.assertTrue((release / "bin" / "codex").is_file())
            self.assertTrue((release / "manifest.txt").is_file())
            self.assertEqual(
                os.readlink(lab_home / "sessions"), str(shared_home / "sessions")
            )
            self.assertEqual(
                os.readlink(lab_home / "archived_sessions"),
                str(shared_home / "archived_sessions"),
            )

            launched = subprocess.run(
                [launcher, "--version"],
                check=False,
                capture_output=True,
                text=True,
                env={**os.environ, "HOME": str(root)},
            )
            self.assertEqual(launched.returncode, 0, launched.stderr)
            self.assertEqual(
                launched.stdout.strip(),
                f"{lab_home}|{shared_home}|--version",
            )

    def test_reinstall_is_idempotent(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            fake_binary = write_fake_codex(root)

            first = run_installer(root, fake_binary)
            second = run_installer(root, fake_binary)

            self.assertEqual(first.returncode, 0, first.stderr)
            self.assertEqual(second.returncode, 0, second.stderr)
            backups = list((root / ".local" / "bin").glob("codex-lab.bak-*"))
            self.assertEqual(backups, [])

    def test_refuses_to_replace_existing_lab_conversations(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            fake_binary = write_fake_codex(root)
            sessions = root / ".codex-lab" / "sessions"
            sessions.mkdir(parents=True)
            (sessions / "rollout.jsonl").write_text("existing conversation")

            result = run_installer(root, fake_binary)

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("already contains data", result.stderr)
            self.assertTrue((sessions / "rollout.jsonl").is_file())


def write_fake_codex(root: Path) -> Path:
    fake_binary = root / "fake-codex"
    fake_binary.write_text(
        textwrap.dedent(
            """\
            #!/bin/sh
            printf '%s|%s|%s\n' "$CODEX_HOME" "$CODEX_SQLITE_HOME" "$*"
            """
        )
    )
    fake_binary.chmod(0o755)
    return fake_binary


def run_installer(
    root: Path,
    fake_binary: Path,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            INSTALL_SCRIPT,
            "--binary",
            str(fake_binary),
            "--release-id",
            "test-release",
            "--skip-doctor",
            "--no-strip",
        ],
        check=False,
        capture_output=True,
        text=True,
        env={**os.environ, "HOME": str(root)},
    )


if __name__ == "__main__":
    unittest.main()
