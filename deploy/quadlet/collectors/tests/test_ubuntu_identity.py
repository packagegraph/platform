"""Run the deployed Ubuntu commands against a local repo and inspect their RDF.

Removing --distro ubuntu from ANY entry point must fail on the emitted IDs and
PURLs. The real collector runs; only its repository/output paths are redirected.
The adapter returns a sentinel after collection so no upload command can run.
Requires PyYAML and PG_COLLECT_BIN pointing at the built, current collector.
"""
import gzip
import http.server
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest

import yaml

REPO = Path(__file__).resolve().parents[4]
PKG = "https://purl.org/packagegraph/ontology/core#"
DATA = "https://packagegraph.github.io/d/"
ARCHES = (("ubuntu-noble", "amd64"), ("ubuntu-noble-arm64", "arm64"),
          ("ubuntu-noble-riscv64", "riscv64"))

ADAPTER = '''#!/usr/bin/env python3
import os
import subprocess
import sys
args = sys.argv[1:]
for flag, value in (("--repo", os.environ["FIXTURE_REPO"]),
                    ("-o", os.environ["FIXTURE_OUTPUT"])):
    if args.count(flag) != 1:
        sys.exit("Expected one " + flag)
    args[args.index(flag) + 1] = value
result = subprocess.run([os.environ["PG_COLLECT_BIN"], *args])
sys.exit(93 if result.returncode == 0 else 94)
'''


class Repository(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = None
        if self.path == "/dists/noble/Release":
            body = b"Origin: Ubuntu\nSuite: noble\nCodename: noble\n"
        for _, arch in ARCHES:
            if self.path == f"/dists/noble/main/binary-{arch}/Packages.gz":
                body = gzip.compress((
                    "Package: libdemo\nVersion: 1:2.0-1ubuntu1\n"
                    f"Architecture: {arch}\nSource: demo (1:2.0-1)\n"
                    "Description: local fixture\n\n"
                    "Package: demo-doc\nVersion: 1:2.0-1ubuntu1\n"
                    "Architecture: all\nSource: demo (1:2.0-1)\n"
                    "Description: architecture-independent fixture\n\n"
                ).encode())
        self.send_response(200 if body is not None else 404)
        self.send_header("Content-Length", str(len(body or b"")))
        self.end_headers()
        self.wfile.write(body or b"")

    def log_message(self, *_):
        pass


class UbuntuIdentityTest(unittest.TestCase):
    longMessage = False

    @classmethod
    def setUpClass(cls):
        cls.binary = Path(os.environ["PG_COLLECT_BIN"]).resolve(strict=True)
        cls.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Repository)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join()

    @staticmethod
    def sandbox_wrapper(source_path, dest, scratch):
        """Copy a wrapper, repointing only its /tmp/ scratch prefix.

        The wrapper mints and sweeps a run directory under /tmp (see
        scripts/README.md). Left alone, this test would create and sweep
        directories in the developer's real /tmp. The rewrite is asserted
        reversible below, so it cannot hide a change to the command itself.
        """
        original = Path(source_path).read_text()
        rewritten = original.replace("/tmp/", str(scratch).rstrip("/") + "/")
        Path(dest).write_text(rewritten)
        Path(dest).chmod(0o755)
        return original, rewritten

    def test_the_sandbox_rewrite_is_prefix_only(self):
        with tempfile.TemporaryDirectory(prefix="ubuntu-identity-") as temporary:
            scratch = Path(temporary) / "scratch"
            scratch.mkdir()
            for name, _ in ARCHES:
                with self.subTest(collector=name):
                    source = REPO / "deploy/quadlet/collectors/scripts" / f"{name}.sh"
                    original, rewritten = self.sandbox_wrapper(
                        source, Path(temporary) / f"{name}.sh", scratch)
                    self.assertNotEqual(original, rewritten, "nothing was rewritten")
                    self.assertEqual(
                        rewritten.replace(str(scratch).rstrip("/") + "/", "/tmp/"),
                        original,
                        f"{name}: the sandbox changed more than the /tmp/ prefix")

    def check_entrypoint(self, command, arch, extra_env=None):
        with tempfile.TemporaryDirectory(prefix="ubuntu-identity-") as temporary:
            root = Path(temporary)
            adapter = root / "pg-collect"
            adapter.write_text(ADAPTER)
            adapter.chmod(0o755)
            output = root / "candidate.nt"
            env = dict(os.environ, PATH=f"{root}:{os.environ['PATH']}",
                       PG_COLLECT_BIN=str(self.binary),
                       FIXTURE_REPO=f"http://127.0.0.1:{self.server.server_port}",
                       FIXTURE_OUTPUT=str(output))
            env.update(extra_env or {})
            result = subprocess.run(command, env=env, capture_output=True,
                                    text=True, timeout=30)
            self.assertEqual(result.returncode, 93, result.stdout + result.stderr)
            rdf = output.read_text()
            statements = set(rdf.splitlines())

            def purl(subject, value):
                self.assertIn(
                    f'<{DATA}{subject}> <{PKG}purl> "{value}"'
                    '^^<http://www.w3.org/2001/XMLSchema#anyURI> .', statements,
                    f"Missing expected Ubuntu PURL: {subject} -> {value}")

            purl(f"pkg/ubuntu/noble/{arch}/libdemo", f"pkg:deb/ubuntu/libdemo?arch={arch}")
            purl(f"pkg/ubuntu/noble/{arch}/libdemo/1%3A2.0-1ubuntu1",
                 f"pkg:deb/ubuntu/libdemo@1:2.0-1ubuntu1?arch={arch}")
            purl("src/ubuntu/noble/demo/1%3A2.0-1",
                 "pkg:deb/ubuntu/demo@1:2.0-1?arch=source")
            purl(f"pkg/ubuntu/noble/{arch}/demo-doc", "pkg:deb/ubuntu/demo-doc?arch=all")
            purl(f"pkg/ubuntu/noble/{arch}/demo-doc/1%3A2.0-1ubuntu1",
                 "pkg:deb/ubuntu/demo-doc@1:2.0-1ubuntu1?arch=all")
            self.assertIn(f"<{DATA}distro/ubuntu>", rdf)
            self.assertIn(f"<{DATA}release/ubuntu/noble>", rdf)
            self.assertIn(f"<{DATA}ver/ubuntu/noble/libdemo/1%3A2.0-1ubuntu1>", rdf)
            # DQ IDs name the Debian-format collector, not the distribution.
            # Reject Debian package coordinates, not that legitimate provenance.
            for kind in ("pkg", "src", "ver", "release"):
                self.assertNotIn(f"<{DATA}{kind}/debian/noble", rdf,
                                 f"Emitted a Debian {kind} identifier for Ubuntu")
            self.assertNotIn("pkg:deb/debian/", rdf, "Emitted a Debian PURL for Ubuntu")

    def test_quadlet_wrappers_emit_ubuntu_identity(self):
        for name, arch in ARCHES:
            with self.subTest(collector=name):
                source = REPO / "deploy/quadlet/collectors/scripts" / f"{name}.sh"
                with tempfile.TemporaryDirectory(prefix="ubuntu-wrapper-") as box:
                    scratch = Path(box) / "scratch"
                    scratch.mkdir()
                    script = Path(box) / f"{name}.sh"
                    self.sandbox_wrapper(source, script, scratch)
                    self.check_entrypoint(["sh", str(script)], arch)

    def test_kubernetes_jobs_emit_ubuntu_identity(self):
        for name, arch in ARCHES:
            with self.subTest(collector=name):
                path = REPO / "deploy/overlays/dev/jobs" / f"collect-{name}.yaml"
                job = yaml.safe_load(path.read_text())
                container, = job["spec"]["jobTemplate"]["spec"]["template"]["spec"]["containers"]
                env = {item["name"]: item["value"] for item in container["env"]}
                self.assertEqual(env["GRAPH_URI"], "https://packagegraph.github.io/graph/"
                                 + "ubuntu/noble" + ("" if arch == "amd64" else f"/{arch}"))
                self.check_entrypoint(container["command"] + container["args"], arch, env)


if __name__ == "__main__":
    unittest.main()
