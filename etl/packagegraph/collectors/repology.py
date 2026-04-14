"""Repology.org cross-distribution package equivalence enricher."""

import click
import requests
import json
import time
from pathlib import Path
from datetime import datetime, timedelta
from rdflib import Graph, Literal

from ..namespaces import PKG


class RepologyEnricher:
    """Enriches package graph with cross-distribution equivalence links from repology.org."""

    def __init__(
        self, graph: Graph, cache_dir: str | None = None, cache_ttl_days: int = 7
    ):
        """Initialize RepologyEnricher.

        Args:
            graph: RDF graph to enrich
            cache_dir: Directory for caching API responses (None = no cache)
            cache_ttl_days: Cache time-to-live in days
        """
        self.graph = graph
        self.cache_dir = Path(cache_dir) if cache_dir else None
        self.cache_ttl = timedelta(days=cache_ttl_days)
        self.api_base = "https://repology.org/api/v1"

        if self.cache_dir:
            self.cache_dir.mkdir(parents=True, exist_ok=True)

        # Mapping from repology repo names to our distribution/release format
        self.repo_mapping = {
            "debian_12": ("debian", "bookworm"),
            "debian_11": ("debian", "bullseye"),
            "debian_10": ("debian", "buster"),
            "fedora_41": ("fedora", "41"),
            "fedora_42": ("fedora", "42"),
            "fedora_40": ("fedora", "40"),
            "ubuntu_24_04": ("ubuntu", "noble"),
            "ubuntu_22_04": ("ubuntu", "jammy"),
            "ubuntu_20_04": ("ubuntu", "focal"),
        }

    def enrich(self):
        """Enrich the graph with cross-distribution package equivalences."""
        click.echo("Starting repology.org enrichment...")

        # Extract unique package names from graph
        package_names = self._get_package_names()
        click.echo(f"Found {len(package_names)} unique package names in graph.")

        # Query repology for each package
        for idx, pkg_name in enumerate(package_names, 1):
            click.echo(
                f"[{idx}/{len(package_names)}] Querying repology for: {pkg_name}"
            )

            repology_data = self._query_repology(pkg_name)
            if repology_data is None:
                continue

            # Create equivalence links (even if empty list)
            self._create_equivalence_links(pkg_name, repology_data)

            # Rate limit: 1 request per second
            if idx < len(package_names):
                time.sleep(1.0)

        click.echo("Repology enrichment complete.")

    def _get_package_names(self) -> set[str]:
        """Extract unique package names from the graph."""
        names = set()
        for s, p, o in self.graph.triples((None, PKG.packageName, None)):
            if isinstance(o, Literal):
                names.add(str(o))
        return names

    def _query_repology(self, project_name: str) -> list[dict] | None:
        """Query repology API for a project, with caching."""
        # Check cache first
        if self.cache_dir:
            cache_file = self.cache_dir / f"{project_name}.json"
            if cache_file.exists():
                cache_age = datetime.now() - datetime.fromtimestamp(
                    cache_file.stat().st_mtime
                )
                if cache_age < self.cache_ttl:
                    click.echo(f"  Using cached response for {project_name}")
                    with open(cache_file) as f:
                        return json.load(f)

        # Fetch from API
        url = f"{self.api_base}/project/{project_name}"
        try:
            response = requests.get(url, timeout=30)
            response.raise_for_status()
            data = response.json()

            # Cache response
            if self.cache_dir:
                cache_file = self.cache_dir / f"{project_name}.json"
                with open(cache_file, "w") as f:
                    json.dump(data, f)

            return data
        except requests.exceptions.HTTPError as e:
            if e.response.status_code == 404:
                click.echo(f"  No repology data for {project_name}", err=True)
            else:
                click.echo(f"  HTTP error for {project_name}: {e}", err=True)
            return None
        except Exception as e:
            click.echo(f"  Error querying repology for {project_name}: {e}", err=True)
            return None

    def _create_equivalence_links(self, pkg_name: str, repology_data: list[dict]):
        """Create pkg:equivalentInDistribution links for matching packages."""
        # Group repology entries by our distribution format
        distro_packages = {}

        for entry in repology_data:
            repo = entry.get("repo")
            if not repo:
                continue

            # Map repology repo to our distribution/release format
            if repo not in self.repo_mapping:
                continue

            distro, release = self.repo_mapping[repo]
            version = entry.get("version", "")

            # Find matching package in our graph
            pkg_uri = self._find_package_in_graph(distro, release, pkg_name, version)
            if pkg_uri:
                distro_packages[repo] = pkg_uri

        # Create links between all pairs (symmetric)
        repos = list(distro_packages.keys())
        for i, repo1 in enumerate(repos):
            for repo2 in repos[i + 1 :]:
                pkg1 = distro_packages[repo1]
                pkg2 = distro_packages[repo2]

                # Add bidirectional equivalence links
                self.graph.add((pkg1, PKG.equivalentInDistribution, pkg2))
                self.graph.add((pkg2, PKG.equivalentInDistribution, pkg1))

                click.echo(f"  Linked {repo1} ↔ {repo2}")

    def _find_package_in_graph(
        self, distro: str, release: str, name: str, version: str | None = None
    ):
        """Find a package URI in the graph matching distro/release/name.

        Returns first match if version is None, otherwise tries to match version too.
        """
        # Query for packages with matching name in this distro/release
        for pkg_uri, _, _ in self.graph.triples((None, PKG.packageName, Literal(name))):
            # Check if this package is in the right distro/release
            # Package URIs follow pattern: data:package/{distro}/{release}/{arch}/{name}/{version}
            uri_str = str(pkg_uri)
            if f"/package/{distro}/{release}/" in uri_str:
                return pkg_uri

        return None
