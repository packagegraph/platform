"""Exercise the staged release, not textual assertions about the runbook."""
import configparser
import importlib.util
import pathlib
import subprocess
import sys
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "checkpoint-release.py"
IMAGE = "ghcr.io/packagegraph/etl@sha256:" + "a" * 64


def load_script():
    """Import the tool as a module; its filename is not a valid identifier."""
    spec = importlib.util.spec_from_file_location("checkpoint_release", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


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

    def install_seeds(self):
        """Simulate the host's seed install.

        Seed lists are not in this repository, so `stage` cannot produce them
        and `verify` only checks that the host has some. Every test that runs
        `verify` installs them, so a failure is attributable to the thing that
        test is actually about rather than to a missing seeds directory.
        """
        seeds = self.root / "seeds"
        seeds.mkdir(exist_ok=True)
        (seeds / "maven-roots.txt").write_text("com.example:example\n")

    def stage(self):
        result = self.run_tool("stage")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.install_seeds()

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

    def test_missing_or_empty_seeds_fails_verification(self):
        # pg-collect@.container bind mounts /etc/containers/systemd/seeds
        # unconditionally, and podman refuses to start a container whose
        # bind-mount source is missing. The template is shared, so installing it
        # over an absent seeds directory fails every collector.
        self.stage()
        seed = self.root / "seeds/maven-roots.txt"
        seed.write_text("")
        self.assertNotEqual(self.run_tool("verify").returncode, 0, "empty seed list")
        seed.unlink()
        self.assertNotEqual(self.run_tool("verify").returncode, 0, "no seed list")
        (self.root / "seeds").rmdir()
        self.assertNotEqual(self.run_tool("verify").returncode, 0, "no seeds directory")

    def test_staging_does_not_require_seeds(self):
        # Seeds are a host precondition, not a release artifact; requiring them
        # at stage time would make it impossible to build a release anywhere
        # except the collector host.
        result = self.run_tool("stage")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / "seeds").exists())

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


class RenderTest(unittest.TestCase):
    """Directives are matched as systemd reads them, not by string prefix."""

    def render(self, body):
        module = load_script()
        with tempfile.TemporaryDirectory() as source:
            module.SOURCE = pathlib.Path(source)
            (module.SOURCE / "scripts").mkdir()
            for name in module.TEMPLATES:
                (module.SOURCE / name).write_text(body)
            # release_files also collects wrappers; supply the minimum that
            # satisfies its non-empty check so rendering is what's under test.
            for name in ("x-full.sh", "test-wrapper-checkpoint-contract.sh"):
                (module.SOURCE / "scripts" / name).write_text("pg-collect rpm-full\n")
            return module.release_files(IMAGE)[module.TEMPLATES[0]].decode()

    def test_indented_directives_are_still_pinned_and_stripped(self):
        rendered = self.render("[Container]\n  Image=ghcr.io/packagegraph/etl:devel-latest\n"
                               "  AutoUpdate=registry\n")
        self.assertIn(f"Image={IMAGE}", rendered)
        self.assertNotIn("AutoUpdate", rendered)
        self.assertNotIn("devel-latest", rendered)

    def test_spaces_around_equals_are_still_pinned_and_stripped(self):
        rendered = self.render("[Container]\nImage = ghcr.io/packagegraph/etl:devel-latest\n"
                               "AutoUpdate = registry\n")
        self.assertIn(f"Image={IMAGE}", rendered)
        self.assertNotIn("AutoUpdate", rendered)
        self.assertNotIn("devel-latest", rendered)

    def test_a_commented_out_directive_is_not_treated_as_one(self):
        # '#Image=' must not count toward the exactly-one check, or a template
        # with a commented example would be rejected or double-counted.
        rendered = self.render("[Container]\n#Image=ghcr.io/packagegraph/etl:old\n"
                               "Image=ghcr.io/packagegraph/etl:devel-latest\n")
        self.assertIn(f"Image={IMAGE}", rendered)
        self.assertIn("#Image=ghcr.io/packagegraph/etl:old", rendered)


if __name__ == "__main__":
    unittest.main()
