"""Koji build metadata enricher using ontology-aligned GraphBuilder.

Uses stdlib xmlrpc.client (no koji library dependency).
"""

import click
import json
import time
import xmlrpc.client
from pathlib import Path
from datetime import datetime, timedelta
from rdflib import Graph, URIRef
from rdflib.namespace import RDF

from ..graph_builder import GraphBuilder
from ..namespaces import PKG, RPM, package_uri as make_package_uri


class KojiEnricher:
    """Enriches RPM package graph with build metadata from Koji."""

    def __init__(
        self,
        graph: Graph,
        koji_hub: str = "https://koji.fedoraproject.org/kojihub",
        distro_name: str = "fedora",
        release_name: str = "",
        cache_dir: str | None = None,
        cache_ttl_days: int = 30
    ):
        self.graph = graph
        self.builder = GraphBuilder(graph)
        self.koji_hub = koji_hub
        self.distro_name = distro_name
        self.release_name = release_name
        self.cache_dir = Path(cache_dir) if cache_dir else None
        self.cache_ttl = timedelta(days=cache_ttl_days)

        if self.cache_dir:
            self.cache_dir.mkdir(parents=True, exist_ok=True)

        self.proxy = xmlrpc.client.ServerProxy(koji_hub)

    def enrich(self):
        """Enrich graph with koji build metadata for RPM packages."""
        click.echo("Starting koji build metadata enrichment...")

        rpm_packages = self._get_rpm_packages()
        click.echo(f"Found {len(rpm_packages)} RPM packages to query koji for.")

        for idx, (pkg_uri, pkg_name, ver_str) in enumerate(rpm_packages, 1):
            click.echo(f"[{idx}/{len(rpm_packages)}] Querying koji for {pkg_name}...")

            # Construct NVR (name-version-release) for koji lookup
            nvr = self._make_nvr(pkg_name, ver_str)
            if not nvr:
                continue

            build_data = self._get_build(nvr)
            if build_data:
                self._process_build(pkg_uri, pkg_name, ver_str, build_data)

            if idx < len(rpm_packages):
                time.sleep(0.5)

        click.echo("Koji enrichment complete.")

    def _get_rpm_packages(self) -> list[tuple[URIRef, str, str]]:
        """Get RPM packages from graph."""
        packages = []
        seen = set()

        for pkg_uri, _, _ in self.graph.triples((None, RDF.type, RPM.BinaryRPM)):
            # Get package name
            for _, _, name_lit in self.graph.triples((pkg_uri, PKG.packageName, None)):
                pkg_name = str(name_lit)
                if pkg_name in seen:
                    continue
                seen.add(pkg_name)

                # Get version string
                ver_str = ""
                for _, _, ver_uri in self.graph.triples((pkg_uri, PKG.hasVersion, None)):
                    for _, _, vs in self.graph.triples((ver_uri, PKG.versionString, None)):
                        ver_str = str(vs)
                        break
                    break

                packages.append((pkg_uri, pkg_name, ver_str))
                break

        return packages

    def _make_nvr(self, name: str, version_str: str) -> str | None:
        """Construct NVR from package name and version string.

        RPM version strings are typically like '5.2.15-1.fc41.x86_64'.
        NVR format is 'name-version-release' (no arch).
        """
        # Strip arch suffix if present
        parts = version_str.rsplit(".", 1)
        if len(parts) == 2 and parts[1] in ("x86_64", "i686", "noarch", "aarch64", "s390x", "ppc64le"):
            version_str = parts[0]

        if not version_str:
            return None

        return f"{name}-{version_str}"

    def _get_build(self, nvr: str) -> dict | None:
        """Get build info from koji, with caching."""
        # Check cache
        if self.cache_dir:
            cache_file = self.cache_dir / f"{nvr}.json"
            if cache_file.exists():
                age = datetime.now() - datetime.fromtimestamp(cache_file.stat().st_mtime)
                if age < self.cache_ttl:
                    with open(cache_file) as f:
                        return json.load(f)

        try:
            build_data = self.proxy.getBuild(nvr)
            if not build_data:
                return None

            # Cache response (builds are immutable)
            if self.cache_dir:
                cache_file = self.cache_dir / f"{nvr}.json"
                with open(cache_file, "w") as f:
                    json.dump(build_data, f, default=str)

            return build_data
        except Exception as e:
            click.echo(f"  Koji error for {nvr}: {e}", err=True)
            return None

    def _process_build(self, pkg_uri: URIRef, pkg_name: str, ver_str: str, build_data: dict):
        """Create BuildActivity and link dependencies."""
        build_uri = self.builder.add_build_activity(
            distro=self.distro_name,
            release=self.release_name,
            name=pkg_name,
            version=ver_str,
            owner=build_data.get("owner_name"),
            start_time=str(build_data.get("start_time", "")),
            end_time=str(build_data.get("completion_time", "")),
            build_system="koji"
        )

        # Link package to build
        self.builder.link_build_to_package(build_uri, pkg_uri)

        # Get build dependencies (buildroot RPMs)
        build_id = build_data.get("build_id")
        if build_id:
            self._add_build_deps(build_uri, build_id)

    def _add_build_deps(self, build_uri: URIRef, build_id: int):
        """Query koji for build dependencies and add to graph."""
        try:
            build_rpms = self.proxy.listBuildRPMs(build_id)
            if not build_rpms:
                return

            for rpm in build_rpms:
                dep_name = rpm.get("name", "")
                dep_ver = rpm.get("version", "")
                dep_rel = rpm.get("release", "")
                dep_arch = rpm.get("arch", "x86_64")

                if dep_name:
                    dep_version_str = f"{dep_ver}-{dep_rel}.{dep_arch}"
                    dep_uri = make_package_uri(
                        self.distro_name, self.release_name,
                        dep_arch, dep_name, dep_version_str
                    )
                    self.builder.link_build_dependency(build_uri, dep_uri)

        except Exception as e:
            click.echo(f"  Error fetching build deps: {e}", err=True)
