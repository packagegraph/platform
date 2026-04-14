"""License claim enricher — records GitHub license detection as attributed claims."""

import re
from packagegraph.enrichers.base import BaseEnricher
from packagegraph.enrichers.cache import CacheManager
from packagegraph.namespaces import PKG, license_uri


class LicenseEnricher(BaseEnricher):
    """Enriches package graph with license claims from GitHub API.

    Queries GitHub for license metadata and records it as attributed claims
    with PROV-O provenance. Does NOT assert licenses are correct — records
    'GitHub reports license=MIT for this repo as of {timestamp}'.
    """

    def __init__(
        self,
        sparql_client,
        output_path: str,
        cache_manager: CacheManager,
        enricher_version: str,
        github_token: str | None = None,
    ):
        super().__init__(
            sparql_client=sparql_client,
            output_path=output_path,
            enricher_name='license',
            enricher_version=enricher_version,
        )
        self.cache = cache_manager
        self.token = github_token
        self.api_base = 'https://api.github.com'

    def _query_packages(self):
        """Query Fuseki for packages with GitHub homepages."""
        return self.client.query_github_homepages()

    def _process_item(self, item):
        """Process one package-homepage pair and extract license."""
        pkg_uri, homepage = item

        # Extract owner/repo from GitHub URL
        github_re = re.compile(r'https?://github\.com/([^/]+)/([^/]+?)(?:\.git)?/?$')
        match = github_re.match(homepage)
        if not match:
            return

        owner, repo_name = match.group(1), match.group(2)
        endpoint = f'/repos/{owner}/{repo_name}'

        # Get repo data from cache or API
        repo_data = self.cache.get(f'{self.api_base}{endpoint}')
        if not repo_data:
            # Not in cache - this enricher assumes GitHub enricher has already run
            return

        license_data = repo_data.get('license')
        if not license_data or not license_data.get('spdx_id'):
            return

        spdx_id = license_data['spdx_id']
        if spdx_id == 'NOASSERTION' or not spdx_id:
            return

        # Validate SPDX ID is not obviously invalid
        if len(spdx_id) > 100 or '<' in spdx_id or '>' in spdx_id:
            return

        # Create license URI and write claim triples
        lic_uri = license_uri(spdx_id)
        self.writer.write_uri(lic_uri, 'http://www.w3.org/1999/02/22-rdf-syntax-ns#type', str(PKG.License))
        self.writer.write_lit(lic_uri, str(PKG.spdxExpression), spdx_id)

        if license_data.get('name'):
            self.writer.write_lit(lic_uri, str(PKG.licenseName), license_data['name'])

        # Link package to license
        self.writer.write_uri(pkg_uri, str(PKG.hasLicense), lic_uri)

        # Attribution to GitHub API (part of PROV-O provenance handled by base class)
