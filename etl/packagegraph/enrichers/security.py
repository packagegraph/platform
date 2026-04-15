"""Security vulnerability enricher — queries Fuseki, calls OSV.dev, writes N-Triples."""

import time
import requests
from .base import BaseEnricher
from .cache import CacheManager
from ..sparql_client import SparqlQueryClient
from ..namespaces import SEC, cve_uri, version_uri

# Mapping from our ecosystem identifiers to OSV API ecosystem names and RDF types
OSV_ECOSYSTEMS = {
    "debian": {"osv_name": "Debian", "rdf_type": "deb:BinaryPackage", "distro": "debian", "release": "trixie"},
    "alpine": {"osv_name": "Alpine", "rdf_type": "alpine:AlpinePackage", "distro": "alpine", "release": "v3.20"},
    "npm": {"osv_name": "npm", "rdf_type": "npm:NpmPackage", "distro": "npm", "release": "registry"},
    "pypi": {"osv_name": "PyPI", "rdf_type": "pypi:PythonPackage", "distro": "pypi", "release": "pypi"},
    "cargo": {"osv_name": "crates.io", "rdf_type": "cargo:Crate", "distro": "cargo", "release": "crates.io"},
    "gomod": {"osv_name": "Go", "rdf_type": "gomod:GoModule", "distro": "gomod", "release": "proxy"},
}


class SecurityEnricher(BaseEnricher):
    """Enriches package graph with vulnerability data from OSV.dev API.

    Queries OSV for vulnerabilities and records them as attributed claims.
    Supports multiple ecosystems (Debian, Alpine, npm, PyPI, crates.io, Go).
    """

    def __init__(
        self,
        sparql_client: SparqlQueryClient,
        output_path: str,
        cache_dir: str | None = None,
        cache_ttl_hours: int = 24,
        ecosystem: str = "debian",
    ):
        super().__init__(
            sparql_client=sparql_client,
            output_path=output_path,
            enricher_name='security',
            enricher_version='2.0.0',
        )
        self.osv_api = "https://api.osv.dev/v1"
        self.ecosystem = ecosystem

        # Validate ecosystem
        if ecosystem not in OSV_ECOSYSTEMS:
            raise ValueError(f"Unsupported ecosystem: {ecosystem}. Supported: {list(OSV_ECOSYSTEMS.keys())}")

        self.ecosystem_config = OSV_ECOSYSTEMS[ecosystem]

        # Create CacheManager if cache_dir provided
        if cache_dir:
            self.cache: CacheManager | None = CacheManager(
                cache_dir=cache_dir,
                enricher_name='security',
                minio_endpoint=None
            )
            self.cache_ttl_hours: int = cache_ttl_hours
        else:
            self.cache = None
            self.cache_ttl_hours = cache_ttl_hours

    def _query_packages(self):
        """Query Fuseki for packages of this ecosystem's RDF type."""
        packages = self.client.query_packages_by_type(self.ecosystem_config["rdf_type"])
        # Deduplicate by name
        seen = set()
        unique = []
        for name, version in packages:
            if name not in seen:
                seen.add(name)
                unique.append((name, version))
        return unique

    def _process_item(self, item):
        """Process one package and check for vulnerabilities."""
        pkg_name, ver_str = item
        vulns = self._query_osv(pkg_name)
        if vulns:
            self._write_vuln_triples(pkg_name, ver_str, vulns)
        time.sleep(0.5)

    def _query_osv(self, package_name: str) -> list[dict] | None:
        """Query OSV API for vulnerabilities affecting package."""
        url = f"{self.osv_api}/query"
        params = {"package": {"name": package_name, "ecosystem": self.ecosystem_config["osv_name"]}}

        # Check cache
        if self.cache:
            cache_key_str = f"{url}#{package_name}"
            cached = self.cache.get(cache_key_str)
            if cached:
                return cached.get("vulns", [])

        # Fetch from API
        try:
            response = requests.post(url, json=params, timeout=30)
            response.raise_for_status()
            data = response.json()

            # Store in cache
            if self.cache:
                self.cache.put(
                    url=cache_key_str,
                    data=data,
                    source_url=url,
                    api_version='v1',
                    ttl_hours=self.cache_ttl_hours
                )

            return data.get("vulns", [])
        except Exception as e:
            print(f"  OSV error for {package_name}: {e}")
            return None

    def _write_vuln_triples(self, pkg_name, version_str, vulns):
        """Write vulnerability triples using self.writer."""
        RDF_TYPE = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
        for vuln in vulns:
            vuln_id = vuln.get("id", "")
            if not vuln_id:
                continue
            v_uri = str(cve_uri(vuln_id))

            self.writer.write_uri(v_uri, RDF_TYPE, str(SEC.Vulnerability))
            self.writer.write_lit(v_uri, str(SEC.cveId), vuln_id)

            if vuln.get("summary"):
                self.writer.write_lit(v_uri, str(SEC.vulnerabilityDescription), vuln["summary"][:1000])

            # Enhanced: Extract CVSS vector and score
            for sev in vuln.get("severity", []):
                if sev.get("type") == "CVSS_V3":
                    score_str = sev.get("score", "")
                    # If score is a CVSS vector string
                    if score_str.startswith("CVSS:"):
                        self.writer.write_lit(v_uri, str(SEC.cvssVector), score_str)
                    else:
                        # It's a numeric score string
                        self.writer.write_lit(v_uri, str(SEC.severity), score_str)

            # Enhanced: Extract CWE ID
            db_specific = vuln.get("database_specific", {})
            cwe_ids = db_specific.get("cwe_ids", [])
            if cwe_ids and isinstance(cwe_ids, list):
                # Take first CWE ID
                self.writer.write_lit(v_uri, str(SEC.cweId), cwe_ids[0])

            if vuln.get("published"):
                self.writer.write_lit(v_uri, str(SEC.publishedDate), vuln["published"])

            # Enhanced: Extract fixed version from ranges
            for affected in vuln.get("affected", []):
                if (
                    affected.get("package", {}).get("name", "").lower()
                    == pkg_name.lower()
                ):
                    # Use ecosystem-specific version URI
                    ver_uri = str(version_uri(
                        self.ecosystem_config["distro"],
                        self.ecosystem_config["release"],
                        pkg_name,
                        version_str
                    ))
                    self.writer.write_uri(v_uri, str(SEC.affectsVersion), ver_uri)

                    # Extract fixed version
                    for range_entry in affected.get("ranges", []):
                        for event in range_entry.get("events", []):
                            if "fixed" in event:
                                fixed_ver = event["fixed"]
                                fixed_uri = str(version_uri(
                                    self.ecosystem_config["distro"],
                                    self.ecosystem_config["release"],
                                    pkg_name,
                                    fixed_ver
                                ))
                                self.writer.write_uri(v_uri, str(SEC.fixedInVersion), fixed_uri)
                                break
                    break
