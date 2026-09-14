"""Exercise the staged release, not textual assertions about the runbook."""
import configparser
import pathlib
import subprocess
import sys
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "checkpoint-release.py"
IMAGE = "ghcr.io/packagegraph/etl@sha256:" + "a" * 64


class ReleaseTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name) / "release"

    def run_tool(self, mode, image=IMAGE):
        return subprocess.run(
            [sys.executable, str(SCRIPT), mode, image, str(self.root)],
            text=True, capture_output=True, timeout=10,
        )

    def stage(self):
        result = self.run_tool("stage")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_matching_release_verifies(self):
        self.stage()
        self.assertEqual(self.run_tool("verify").returncode, 0)
        # Independently check the generated release, not merely agreement
        # between stage() and verify() using the same renderer.
        for template in ("pg-collect@.container", "pg-collect-rhel@.container"):
            config = configparser.ConfigParser(interpolation=None, strict=False)
            config.read(self.root / template)
            self.assertEqual(config["Container"]["Image"], IMAGE)
            self.assertNotIn("AutoUpdate", config["Container"])

    def test_floating_image_is_rejected_before_writing(self):
        self.assertNotEqual(self.run_tool("stage", "ghcr.io/packagegraph/etl:devel-latest").returncode, 0)
        self.assertFalse(self.root.exists())

    def test_stage_does_not_overwrite_existing_installation(self):
        self.stage()
        self.assertNotEqual(self.run_tool("stage").returncode, 0)
        self.assertEqual(self.run_tool("verify").returncode, 0)

    def test_mismatched_image_fails_verification(self):
        self.stage()
        unit = self.root / "pg-collect@.container"
        unit.write_text(unit.read_text().replace(IMAGE, "ghcr.io/packagegraph/etl:devel-latest"))
        self.assertNotEqual(self.run_tool("verify").returncode, 0)

    def test_missing_or_old_wrapper_fails_verification(self):
        self.stage()
        wrapper = self.root / "scripts/collectors/fedora-44-full.sh"
        wrapper.write_text(wrapper.read_text().replace("pg-collect checkpoint commit", "# omitted commit"))
        self.assertNotEqual(self.run_tool("verify").returncode, 0)
        wrapper.unlink()
        self.assertNotEqual(self.run_tool("verify").returncode, 0)

    def test_drop_in_cannot_silently_override_image_pin(self):
        self.stage()
        for directory in ("container.d", "pg-collect@.container.d", "pg-collect-rhel@.container.d",
                          "pg-collect@fedora-44-full.container.d"):
            dropin = self.root / directory
            dropin.mkdir()
            config = dropin / "override.conf"
            config.write_text("[Container]\nImage=ghcr.io/packagegraph/etl:devel-latest\n")
            self.assertNotEqual(self.run_tool("verify").returncode, 0, directory)
            config.unlink()
            dropin.rmdir()


if __name__ == "__main__":
    unittest.main()
