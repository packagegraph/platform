"""Vendor security advisory enrichers (RHSA, DSA)."""

import time
import requests
from datetime import datetime, timedelta
from .base import BaseEnricher
from .cache import CacheManager
from ..sparql_client import SparqlQueryClient
from ..namespaces import SEC, cve_uri


class RHSAEnricher(BaseEnricher):
    """Red Hat Security Advisory enricher."""

    def __init__(
        self,
        sparql_client: SparqlQueryClient,
        output_path: str,
        cache_dir: str | None = None,
        cache_ttl_hours: int = 168,  # 1 week (advisories are mostly immutable)
        days_back: int = 365,
    ):
        super().__init__(
            sparql_client=sparql_client,
            output_path=output_path,
            enricher_name='advisory-rhsa',
            enricher_version='1.0.0',
        )
        self.api_base = "https://access.redhat.com/hydra/rest/securitydata"
        self.days_back = days_back

        if cache_dir:
            self.cache: CacheManager | None = CacheManager(
                cache_dir=cache_dir,
                enricher_name='advisory-rhsa',
                minio_endpoint=None
            )
            self.cache_ttl_hours = cache_ttl_hours
        else:
            self.cache = None
            self.cache_ttl_hours = cache_ttl_hours

    def _query_packages(self):
        """Return list of pages to fetch (pagination-based, not package-based)."""
        # Advisory enricher doesn't query Fuseki for packages — it fetches all advisories
        # from RHSA API and matches them to packages later
        return [1]  # Start with page 1, pagination handled in _process_item

    def _process_item(self, item):
        """Fetch RHSA advisories with pagination."""
        after_date = (datetime.now() - timedelta(days=self.days_back)).strftime("%Y-%m-%d")

        page = 1
        while page < 100:  # Safety limit
            url = f"{self.api_base}/cve.json?after={after_date}&per_page=100&page={page}"

            # Check cache
            if self.cache:
                cached = self.cache.get(url)
                if cached:
                    cves = cached
                else:
                    # Fetch from API
                    try:
                        response = requests.get(url, timeout=60)
                        response.raise_for_status()
                        cves = response.json()
                        self.cache.put(
                            url=url,
                            data=cves,
                            source_url=url,
                            api_version='v1',
                            ttl_hours=self.cache_ttl_hours
                        )
                    except Exception as e:
                        print(f"  RHSA API error (page {page}): {e}")
                        break
            else:
                try:
                    response = requests.get(url, timeout=60)
                    response.raise_for_status()
                    cves = response.json()
                except Exception as e:
                    print(f"  RHSA API error (page {page}): {e}")
                    break

            if not cves or not isinstance(cves, list):
                break

            print(f"  Processing page {page}: {len(cves)} CVEs")

            for cve_data in cves:
                self._write_advisory_triples(cve_data)

            if len(cves) < 100:
                break

            page += 1
            time.sleep(1.0)  # Rate limit

    def _write_advisory_triples(self, cve_data):
        """Write RHSA advisory triples."""
        RDF_TYPE = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
        cve_id = cve_data.get("CVE")
        if not cve_id:
            return

        # RHSA ID from bugzilla_description or cve_id
        rhsa_id = cve_data.get("bugzilla_description", cve_id)
        advisory_uri = f"https://packagegraph.github.io/d/advisory/rhsa/{rhsa_id.replace(' ', '_')}"

        self.writer.write_uri(advisory_uri, RDF_TYPE, str(SEC.SecurityAdvisory))
        self.writer.write_lit(advisory_uri, str(SEC.advisoryId), rhsa_id)

        if cve_data.get("severity"):
            self.writer.write_lit(advisory_uri, str(SEC.advisorySeverity), cve_data["severity"])

        if cve_data.get("public_date"):
            self.writer.write_lit(advisory_uri, str(SEC.advisoryDate), cve_data["public_date"])

        self.writer.write_lit(advisory_uri, str(SEC.advisoryType), "security")

        # Link to CVE
        vuln_uri = str(cve_uri(cve_id))
        self.writer.write_uri(advisory_uri, str(SEC.addressesVulnerability), vuln_uri)

        # Link to affected packages
        for pkg in cve_data.get("affected_packages", []):
            # Package names from RHSA are often in format "component/version"
            # For now, skip package links (would need RPM package lookup)
            pass


class DSAEnricher(BaseEnricher):
    """Debian Security Advisory enricher."""

    def __init__(
        self,
        sparql_client: SparqlQueryClient,
        output_path: str,
        cache_dir: str | None = None,
        cache_ttl_hours: int = 168,
    ):
        super().__init__(
            sparql_client=sparql_client,
            output_path=output_path,
            enricher_name='advisory-dsa',
            enricher_version='1.0.0',
        )
        self.tracker_url = "https://security-tracker.debian.org/tracker/data/json"

        if cache_dir:
            self.cache: CacheManager | None = CacheManager(
                cache_dir=cache_dir,
                enricher_name='advisory-dsa',
                minio_endpoint=None
            )
            self.cache_ttl_hours = cache_ttl_hours
        else:
            self.cache = None
            self.cache_ttl_hours = cache_ttl_hours

    def _query_packages(self):
        """Return single-item list (one download for whole tracker)."""
        return [1]

    def _process_item(self, item):
        """Download and process Debian security tracker JSON."""
        # Check cache
        if self.cache:
            cached = self.cache.get(self.tracker_url)
            if cached:
                tracker_data = cached
            else:
                tracker_data = self._fetch_tracker()
        else:
            tracker_data = self._fetch_tracker()

        if not tracker_data:
            return

        print(f"  Processing {len(tracker_data)} DSA entries")

        for pkg_name, pkg_data in tracker_data.items():
            for cve_id, cve_info in pkg_data.items():
                if not cve_id.startswith("CVE-"):
                    continue
                self._write_dsa_triples(pkg_name, cve_id, cve_info)

    def _fetch_tracker(self):
        """Fetch Debian security tracker JSON."""
        try:
            print(f"  Fetching {self.tracker_url}")
            response = requests.get(self.tracker_url, timeout=120)
            response.raise_for_status()
            data = response.json()

            if self.cache:
                self.cache.put(
                    url=self.tracker_url,
                    data=data,
                    source_url=self.tracker_url,
                    api_version='json',
                    ttl_hours=self.cache_ttl_hours
                )

            return data
        except Exception as e:
            print(f"  DSA tracker error: {e}")
            return None

    def _write_dsa_triples(self, pkg_name, cve_id, cve_info):
        """Write DSA advisory triples."""
        RDF_TYPE = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
        # DSA ID (if present in cve_info)
        dsa_id = cve_info.get("debianbug", cve_id)
        advisory_uri = f"https://packagegraph.github.io/d/advisory/dsa/{cve_id.replace('-', '_')}"

        self.writer.write_uri(advisory_uri, RDF_TYPE, str(SEC.SecurityAdvisory))
        self.writer.write_lit(advisory_uri, str(SEC.advisoryId), dsa_id)
        self.writer.write_lit(advisory_uri, str(SEC.advisoryType), "security")

        # Link to CVE
        vuln_uri = str(cve_uri(cve_id))
        self.writer.write_uri(advisory_uri, str(SEC.addressesVulnerability), vuln_uri)

        # Link to package identity (not version — would need Fuseki lookup)
        # For now, just note the package name in a data quality annotation
