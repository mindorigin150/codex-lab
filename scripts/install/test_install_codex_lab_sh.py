#!/usr/bin/env python3

import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import textwrap
import unittest


INSTALL_SCRIPT = Path(__file__).with_name("install-codex-lab.sh")


class InstallCodexLabShTest(unittest.TestCase):
    def test_new_config_uses_agents_namespace(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)

            result = run_installer(root, write_fake_codex(root))

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                (root / ".codex-lab" / "config.toml").read_text(),
                '[features.multi_agent_v2]\ntool_namespace = "agents"\n',
            )
            self.assertEqual(
                (root / ".codex-lab" / "config.toml").stat().st_mode & 0o777,
                0o600,
            )
            self.assertEqual(list((root / ".codex-lab").glob(".config.toml.*")), [])

    def test_preserves_existing_multi_agent_table_without_namespace(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            original = (
                '[features.multi_agent_v2]\nenabled = true\n\n[model]\nname = "test"\n'
            )
            config = write_config(root, original)

            result = run_installer(root, write_fake_codex(root))

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(config.read_text(), original)

    def test_preserves_legacy_boolean_feature_config(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            original = "[features]\nmulti_agent_v2 = true\n"
            config = write_config(root, original)

            result = run_installer(root, write_fake_codex(root))

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(config.read_text(), original)

    def test_preserves_legacy_collaboration_namespace_for_runtime_normalization(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            original = (
                '[features.multi_agent_v2]\ntool_namespace = "collaboration" # legacy\n'
            )
            config = write_config(root, original)

            result = run_installer(root, write_fake_codex(root))

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(config.read_text(), original)

    def test_preserves_dotted_custom_namespace_config(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            original = 'features.multi_agent_v2.tool_namespace = "my_lab_tools"\n'
            config = write_config(root, original)

            result = run_installer(root, write_fake_codex(root))

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(config.read_text(), original)

    def test_refuses_to_write_through_dangling_config_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            lab_home = root / ".codex-lab"
            shared_config = root / ".codex" / "config.toml"
            lab_home.mkdir(parents=True)
            (lab_home / "config.toml").symlink_to(shared_config)

            result = run_installer(root, write_fake_codex(root))

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("dangling symlink", result.stderr)
            self.assertFalse(shared_config.exists())
            self.assertFalse((root / ".local" / "lib" / "codex-lab").exists())

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
            bundled_bwrap = release / "codex-resources" / "bwrap"
            self.assertTrue(bundled_bwrap.is_file())
            self.assertEqual(bundled_bwrap.stat().st_mode & 0o777, 0o755)
            self.assertTrue((release / "manifest.txt").is_file())
            self.assertIn(
                "bwrap_source_kind=explicit",
                (release / "manifest.txt").read_text(),
            )
            self.assertIn(
                "export CODEX_PREFER_BUNDLED_BWRAP=1",
                launcher.read_text(),
            )
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
            config = root / ".codex-lab" / "config.toml"
            self.assertEqual(config.read_text().count("tool_namespace"), 1)
            self.assertEqual(list(config.parent.glob("config.toml.bak-*")), [])

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

    def test_discovers_official_standalone_bwrap(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            official_bwrap = (
                root
                / ".codex"
                / "packages"
                / "standalone"
                / "current"
                / "codex-resources"
                / "bwrap"
            )
            write_fake_bwrap(official_bwrap, marker="official")

            result = run_installer(
                root,
                write_fake_codex(root),
                include_bwrap_arg=False,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            release = release_dir(root)
            self.assertEqual(
                (release / "codex-resources" / "bwrap").read_text(),
                official_bwrap.read_text(),
            )
            self.assertIn(
                "bwrap_source_kind=official-codex",
                (release / "manifest.txt").read_text(),
            )

    def test_explicit_bwrap_overrides_official_install(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            explicit_bwrap = write_fake_bwrap(root / "explicit-bwrap", marker="explicit")
            official_bwrap = (
                root
                / ".codex"
                / "packages"
                / "standalone"
                / "current"
                / "codex-resources"
                / "bwrap"
            )
            write_fake_bwrap(official_bwrap, marker="official")

            result = run_installer(
                root,
                write_fake_codex(root),
                bwrap=explicit_bwrap,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                (release_dir(root) / "codex-resources" / "bwrap").read_text(),
                explicit_bwrap.read_text(),
            )

    def test_uses_compatible_bwrap_from_path_as_last_resort(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            tool_bin = root / "tool-bin"
            path_bwrap = write_fake_bwrap(tool_bin / "bwrap", marker="path")

            result = run_installer(
                root,
                write_fake_codex(root),
                include_bwrap_arg=False,
                extra_env={"PATH": f"{tool_bin}:{os.environ['PATH']}"},
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                (release_dir(root) / "codex-resources" / "bwrap").read_text(),
                path_bwrap.read_text(),
            )
            self.assertIn(
                "bwrap_source_kind=path",
                (release_dir(root) / "manifest.txt").read_text(),
            )

    def test_refuses_to_mutate_release_with_different_bwrap(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            fake_binary = write_fake_codex(root)
            first_bwrap = write_fake_bwrap(root / "first-bwrap", marker="first")
            second_bwrap = write_fake_bwrap(root / "second-bwrap", marker="second")

            first = run_installer(root, fake_binary, bwrap=first_bwrap)
            second = run_installer(root, fake_binary, bwrap=second_bwrap)

            self.assertEqual(first.returncode, 0, first.stderr)
            self.assertNotEqual(second.returncode, 0)
            self.assertIn("different bundled bwrap", second.stderr)
            self.assertEqual(
                (release_dir(root) / "codex-resources" / "bwrap").read_text(),
                first_bwrap.read_text(),
            )

    def test_rejects_incompatible_explicit_bwrap(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            incompatible_bwrap = root / "incompatible-bwrap"
            incompatible_bwrap.write_text("#!/bin/sh\necho incompatible\n")
            incompatible_bwrap.chmod(0o755)

            result = run_installer(
                root,
                write_fake_codex(root),
                bwrap=incompatible_bwrap,
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("does not support --perms", result.stderr)
            self.assertNotIn("sudo apt", result.stderr.lower())

    def test_rejects_binary_with_different_embedded_bwrap_digest(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)

            result = run_installer(
                root,
                write_fake_codex(root),
                extra_env={"FAKE_BWRAP_DIGEST": "0" * 64},
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Codex binary and bundled bwrap do not match", result.stderr)
            self.assertFalse(release_dir(root).exists())

    def test_rechecks_digest_after_binary_is_staged(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)

            result = run_installer(
                root,
                write_fake_codex(root),
                extra_env={"FAKE_STAGED_BWRAP_DIGEST": "0" * 64},
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Codex binary and bundled bwrap do not match", result.stderr)
            self.assertIn("/bin/codex", result.stderr)
            self.assertFalse(release_dir(root).exists())

    def test_fails_without_bwrap_source_and_does_not_require_sudo(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            tool_bin = root / "tool-bin"
            tool_bin.mkdir()
            for command in ("cat", "dirname", "uname"):
                executable = shutil.which(command)
                self.assertIsNotNone(executable)
                (tool_bin / command).symlink_to(executable)

            result = run_installer(
                root,
                write_fake_codex(root),
                include_bwrap_arg=False,
                extra_env={"PATH": str(tool_bin)},
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("No compatible bubblewrap", result.stderr)
            self.assertIn("--bwrap PATH", result.stderr)
            self.assertNotIn("sudo apt", result.stderr.lower())


def write_fake_codex(root: Path) -> Path:
    fake_binary = root / "fake-codex"
    fake_binary.write_text(
        textwrap.dedent(
            """\
            #!/bin/sh
            if [ "$*" = "debug bwrap-digest" ]; then
              if [ "${0##*/}" = "codex" ] && [ -n "${FAKE_STAGED_BWRAP_DIGEST:-}" ]; then
                printf '%s\n' "$FAKE_STAGED_BWRAP_DIGEST"
                exit 0
              fi
              printf '%s\n' "$FAKE_BWRAP_DIGEST"
              exit 0
            fi
            printf '%s|%s|%s\n' "$CODEX_HOME" "$CODEX_SQLITE_HOME" "$*"
            """
        )
    )
    fake_binary.chmod(0o755)
    return fake_binary


def write_fake_bwrap(path: Path, marker: str = "fake") -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        textwrap.dedent(
            f"""\
            #!/bin/sh
            if [ "${{1:-}}" = "--help" ]; then
              echo "{marker} bwrap --perms"
            fi
            exit 0
            """
        )
    )
    path.chmod(0o755)
    return path


def release_dir(root: Path) -> Path:
    return root / ".local" / "lib" / "codex-lab" / "releases" / "test-release"


def write_config(root: Path, contents: str) -> Path:
    config = root / ".codex-lab" / "config.toml"
    config.parent.mkdir(parents=True)
    config.write_text(contents)
    return config


def run_installer(
    root: Path,
    fake_binary: Path,
    *,
    bwrap: Path | None = None,
    include_bwrap_arg: bool = True,
    extra_env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    args = [
        INSTALL_SCRIPT,
        "--binary",
        str(fake_binary),
    ]
    selected_bwrap: Path | None = None
    if include_bwrap_arg:
        selected_bwrap = bwrap or write_fake_bwrap(root / "fake-bwrap")
        args.extend(["--bwrap", str(selected_bwrap)])
    args.extend(
        [
            "--release-id",
            "test-release",
            "--skip-doctor",
            "--no-strip",
        ]
    )
    effective_env = {**os.environ, "HOME": str(root), **(extra_env or {})}
    if selected_bwrap is None:
        official_bwrap = (
            root
            / ".codex"
            / "packages"
            / "standalone"
            / "current"
            / "codex-resources"
            / "bwrap"
        )
        if official_bwrap.is_file():
            selected_bwrap = official_bwrap
        else:
            path_bwrap = shutil.which("bwrap", path=effective_env.get("PATH"))
            if path_bwrap is not None:
                selected_bwrap = Path(path_bwrap)
    if selected_bwrap is not None and selected_bwrap.is_file():
        effective_env.setdefault(
            "FAKE_BWRAP_DIGEST",
            hashlib.sha256(selected_bwrap.read_bytes()).hexdigest(),
        )

    return subprocess.run(
        args,
        check=False,
        capture_output=True,
        text=True,
        env=effective_env,
    )


if __name__ == "__main__":
    unittest.main()
