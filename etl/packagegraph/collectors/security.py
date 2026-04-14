"""NVD/OSV security vulnerability enricher using ontology-aligned GraphBuilder."""

import click
import requests
import json
import time
from pathlib import Path
from datetime import datetime, timedelta
from rdflib import Graph, URIRef
from rdflib.namespace import RDF

from ..graph_builder import GraphBuilder
from ..namespaces import PKG


class SecurityEnricher:
    """Enriches package graph with vulnerability data from OSV.dev API.

    Uses OSV.dev as primary source (covers NVD CVEs + ecosystem-specific advisories).
    Version matching is approximate for v1: string-based matching against
    affected version ranges from the advisory.
    """

    def __init__(
        self, graph: Graph, cache_dir: str | None = None, cache_ttl_hours: int = 24
    ):
        self.graph = graph
        self.builder = GraphBuilder(graph)
        self.cache_dir = Path(cache_dir) if cache_dir else None
        self.cache_ttl = timedelta(hours=cache_ttl_hours)
        self.osv_api = "https://api.osv.dev/v1"

        if self.cache_dir:
            self.cache_dir.mkdir(parents=True, exist_ok=True)

    def enrich(self):
        """Enrich graph with vulnerability data for all packages."""
        click.echo("Starting security vulnerability enrichment (OSV.dev)...")

        packages = self._get_packages_with_versions()
        click.echo(f"Found {len(packages)} packages to check for vulnerabilities.")

        for idx, (pkg_name, version_str, version_uri) in enumerate(packages, 1):
            click.echo(f"[{idx}/{len(packages)}] Checking {pkg_name}...")

            vulns = self._query_osv(pkg_name)
            if vulns:
                self._process_vulns(pkg_name, version_str, version_uri, vulns)

            if idx < len(packages):
                time.sleep(0.5)  # Rate limit

        click.echo("Security enrichment complete.")

    def _get_packages_with_versions(self) -> list[tuple[str, str, URIRef]]:
        """Get unique package names with their version URIs from the graph."""
        packages = []
        seen = set()

        for pkg_uri, _, name_lit in self.graph.triples((None, PKG.packageName, None)):
            # Only process BinaryPackages
            if (pkg_uri, RDF.type, PKG.BinaryPackage) not in self.graph:
                continue

            pkg_name = str(name_lit)
            if pkg_name in seen:
                continue
            seen.add(pkg_name)

            # Find version URI
            for _, _, ver_uri in self.graph.triples((pkg_uri, PKG.hasVersion, None)):
                ver_str = ""
                for _, _, vs in self.graph.triples((ver_uri, PKG.versionString, None)):
                    ver_str = str(vs)
                packages.append((pkg_name, ver_str, ver_uri))
                break

        return packages

    def _query_osv(self, package_name: str) -> list[dict] | None:
        """Query OSV API for vulnerabilities affecting a package."""
        # Check cache
        if self.cache_dir:
            cache_file = self.cache_dir / f"{package_name}.json"
            if cache_file.exists():
                age = datetime.now() - datetime.fromtimestamp(
                    cache_file.stat().st_mtime
                )
                if age < self.cache_ttl:
                    with open(cache_file) as f:
                        data = json.load(f)
                        return data.get("vulns", [])

        try:
            url = f"{self.osv_api}/query"
            response = requests.get(
                url, params={"package_name": package_name}, timeout=30
            )
            response.raise_for_status()
            data = response.json()

            # Cache response
            if self.cache_dir:
                cache_file = self.cache_dir / f"{package_name}.json"
                with open(cache_file, "w") as f:
                    json.dump(data, f)

            return data.get("vulns", [])
        except requests.exceptions.HTTPError as e:
            click.echo(f"  OSV API error for {package_name}: {e}", err=True)
            return None
        except Exception as e:
            click.echo(f"  Error querying OSV for {package_name}: {e}", err=True)
            return None

    def _process_vulns(
        self, pkg_name: str, version_str: str, version_uri: URIRef, vulns: list[dict]
    ):
        """Process vulnerability entries and create graph resources."""
        for vuln in vulns:
            vuln_id = vuln.get("id", "")
            if not vuln_id:
                continue

            # Extract severity score
            severity = None
            for sev_entry in vuln.get("severity", []):
                if sev_entry.get("type") == "CVSS_V3":
                    severity = sev_entry.get("score")
                    break

            vuln_uri = self.builder.add_vulnerability(
                cve_id=vuln_id,
                description=vuln.get("summary"),
                severity=severity,
                published=vuln.get("published"),
                modified=vuln.get("modified"),
            )

            # Link to affected version
            # v1: approximate matching — link to our version if the package
            # is listed in affected packages
            for affected in vuln.get("affected", []):
                affected_pkg = affected.get("package", {})
                if affected_pkg.get("name", "").lower() == pkg_name.lower():
                    self.builder.link_vulnerability_to_version(
                        vuln_uri, version_uri, relation="affects"
                    )

                    # Check for fixed versions
                    for range_entry in affected.get("ranges", []):
                        for event in range_entry.get("events", []):
                            if "fixed" in event:
                                click.echo(
                                    f"  {vuln_id} affects {pkg_name}, fixed in {event['fixed']}"
                                )
                    break
