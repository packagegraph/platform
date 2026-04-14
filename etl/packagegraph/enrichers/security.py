"""Security vulnerability enricher — queries Fuseki, calls OSV.dev, writes N-Triples."""
import json
import time
from pathlib import Path
from datetime import datetime, timedelta
import requests
from ..sparql_client import SparqlQueryClient
from ..namespaces import SEC, cve_uri, version_uri


class SecurityEnricher:
    def __init__(self, sparql_client: SparqlQueryClient, output_path: str,
                 cache_dir: str | None = None, cache_ttl_hours: int = 24):
        self.client = sparql_client
        self.output_path = output_path
        self.cache_dir = Path(cache_dir) if cache_dir else None
        self.cache_ttl = timedelta(hours=cache_ttl_hours)
        self.osv_api = "https://api.osv.dev/v1"
        if self.cache_dir:
            self.cache_dir.mkdir(parents=True, exist_ok=True)

    def enrich(self):
        """Query packages from Fuseki, check OSV, write vulnerability triples."""
        print("Querying Fuseki for package names and versions...")
        packages = self.client.query_package_names_and_versions()
        # Deduplicate by name
        seen = set()
        unique = []
        for name, version in packages:
            if name not in seen:
                seen.add(name)
                unique.append((name, version))
        print(f"Found {len(unique)} unique packages to check.")

        with open(self.output_path, "w") as f:
            for idx, (pkg_name, ver_str) in enumerate(unique, 1):
                if idx % 100 == 0:
                    print(f"  [{idx}/{len(unique)}] Checking {pkg_name}...")
                vulns = self._query_osv(pkg_name)
                if vulns:
                    self._write_vuln_triples(f, pkg_name, ver_str, vulns)
                time.sleep(0.5)
        print(f"Security enrichment complete. Output: {self.output_path}")

    def _query_osv(self, package_name: str) -> list[dict] | None:
        if self.cache_dir:
            cache_file = self.cache_dir / f"{package_name}.json"
            if cache_file.exists():
                age = datetime.now() - datetime.fromtimestamp(cache_file.stat().st_mtime)
                if age < self.cache_ttl:
                    with open(cache_file) as f:
                        return json.load(f).get("vulns", [])
        try:
            response = requests.post(
                f"{self.osv_api}/query",
                json={"package": {"name": package_name, "ecosystem": "Debian"}},
                timeout=30,
            )
            response.raise_for_status()
            data = response.json()
            if self.cache_dir:
                cache_file = self.cache_dir / f"{package_name}.json"
                with open(cache_file, "w") as f:
                    json.dump(data, f)
            return data.get("vulns", [])
        except Exception as e:
            print(f"  OSV error for {package_name}: {e}")
            return None

    def _write_vuln_triples(self, f, pkg_name, version_str, vulns):
        RDF_TYPE = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
        for vuln in vulns:
            vuln_id = vuln.get("id", "")
            if not vuln_id:
                continue
            v_uri = str(cve_uri(vuln_id))
            f.write(f"<{v_uri}> <{RDF_TYPE}> <{SEC}Vulnerability> .\n")
            _write_lit(f, v_uri, f"{SEC}cveId", vuln_id)
            if vuln.get("summary"):
                _write_lit(f, v_uri, f"{SEC}vulnerabilityDescription", vuln["summary"][:1000])
            for sev in vuln.get("severity", []):
                if sev.get("type") == "CVSS_V3":
                    _write_lit(f, v_uri, f"{SEC}severity", sev["score"])
            if vuln.get("published"):
                _write_lit(f, v_uri, f"{SEC}publishedDate", vuln["published"])
            for affected in vuln.get("affected", []):
                if affected.get("package", {}).get("name", "").lower() == pkg_name.lower():
                    ver_uri = str(version_uri("debian", "trixie", pkg_name, version_str))
                    f.write(f"<{v_uri}> <{SEC}affectsVersion> <{ver_uri}> .\n")
                    break


def _escape_nt(s: str) -> str:
    return s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n").replace("\r", "\\r").replace("\t", "\\t")


def _write_lit(f, subj, pred, val):
    f.write(f'<{subj}> <{pred}> "{_escape_nt(str(val))}" .\n')
