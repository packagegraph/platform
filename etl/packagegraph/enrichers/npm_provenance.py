"""npm SLSA provenance enricher — collects attestations from npm registry."""

import time
import requests
from .base import BaseEnricher
from ..sparql_client import SparqlQueryClient
from ..namespaces import SLSA


class NpmProvenanceEnricher(BaseEnricher):
    """Collects SLSA provenance attestations from npm registry.

    npm packages published with --provenance flag have attestation bundles
    in the package metadata. This enricher extracts and records them.
    """

    def __init__(
        self,
        sparql_client: SparqlQueryClient,
        output_path: str,
    ):
        super().__init__(
            sparql_client=sparql_client,
            output_path=output_path,
            enricher_name='npm-provenance',
            enricher_version='1.0.0',
        )
        self.registry_base = "https://registry.npmjs.org"

    def _query_packages(self):
        """Query Fuseki for npm packages."""
        return self.client.query_packages_by_type("npm:NpmPackage")

    def _process_item(self, item):
        """Process one npm package and check for attestations."""
        pkg_name, version = item

        # Fetch package metadata
        attestations = self._fetch_attestations(pkg_name, version)
        if attestations:
            self._write_provenance_triples(pkg_name, version, attestations)

        time.sleep(0.2)  # Rate limit

    def _fetch_attestations(self, name, version):
        """Fetch attestations from npm registry (package metadata)."""
        # npm attestations are in the package.json dist.attestations field
        # Try package version endpoint first
        url = f"{self.registry_base}/{name}/{version}"

        try:
            response = requests.get(url, timeout=30)
            if response.status_code == 404:
                return None
            response.raise_for_status()
            data = response.json()

            # Check for attestations in dist
            dist = data.get("dist", {})
            attestations = dist.get("attestations")
            if attestations:
                return attestations

            return None
        except Exception as e:
            print(f"  npm error for {name}@{version}: {e}")
            return None

    def _write_provenance_triples(self, pkg_name, version, attestations):
        """Write SLSA provenance triples."""
        RDF_TYPE = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"

        # Create ProvenanceAttestation
        att_uri = f"https://packagegraph.github.io/d/attestation/npm/registry/{pkg_name}/{version}"
        self.writer.write_uri(att_uri, RDF_TYPE, str(SLSA.ProvenanceAttestation))

        # Link to package
        pkg_uri = f"https://packagegraph.github.io/d/pkg/npm/registry/any/{pkg_name}/{version}"
        self.writer.write_uri(pkg_uri, str(SLSA.hasProvenance), att_uri)

        # SLSA Build Level L2 (npm provenance uses GitHub Actions)
        self.writer.write_uri(att_uri, str(SLSA.attestsBuildLevel), str(SLSA.L2))

        # Extract predicate type and bundle info if available
        if isinstance(attestations, dict):
            predicate_type = attestations.get("predicateType", "https://slsa.dev/provenance/v1")
            self.writer.write_lit(att_uri, str(SLSA.predicateType), predicate_type)

        # For now, mark as unverified (verification would check Sigstore Rekor)
        self.writer.write_lit(att_uri, str(SLSA.verificationStatus), "unverified")

        # Note: Full attestation parsing (builder ID, timestamps, etc.) would require
        # parsing the attestation bundle structure, which varies by npm version
