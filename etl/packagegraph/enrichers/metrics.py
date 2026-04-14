"""Metrics claim enricher — records GitHub language detection as attributed claims."""

import re
from packagegraph.enrichers.base import BaseEnricher
from packagegraph.enrichers.cache import CacheManager
from packagegraph.namespaces import MET, PKG, DATA, language_uri


class MetricsEnricher(BaseEnricher):
    """Enriches package graph with language composition claims from GitHub API.

    Queries GitHub /languages endpoint for byte counts per language and records
    as attributed claims. GitHub's language detection is Linguist-based — it's
    a claim, not ground truth (can misidentify vendored/generated code).
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
            enricher_name='metrics',
            enricher_version=enricher_version,
        )
        self.cache = cache_manager
        self.token = github_token
        self.api_base = 'https://api.github.com'

    def _query_packages(self):
        """Query Fuseki for packages with GitHub homepages."""
        return self.client.query_github_homepages()

    def _process_item(self, item):
        """Process one package-homepage pair and extract language metrics."""
        pkg_uri, homepage = item

        # Extract owner/repo from GitHub URL
        github_re = re.compile(r'https?://github\.com/([^/]+)/([^/]+?)(?:\.git)?/?$')
        match = github_re.match(homepage)
        if not match:
            return

        owner, repo_name = match.group(1), match.group(2)
        endpoint = f'/repos/{owner}/{repo_name}/languages'

        # Get language data from cache
        lang_data = self.cache.get(f'{self.api_base}{endpoint}')
        if not lang_data or not isinstance(lang_data, dict):
            return

        if not lang_data:
            return

        # Calculate total bytes
        total_bytes = sum(lang_data.values())
        if total_bytes == 0:
            return

        # Create CodeMetrics resource
        code_metrics_uri = DATA[f"metrics/{pkg_uri.split('/')[-1]}"]
        self.writer.write_uri(
            str(code_metrics_uri),
            'http://www.w3.org/1999/02/22-rdf-syntax-ns#type',
            str(MET.CodeMetrics)
        )

        # Link package to metrics
        self.writer.write_uri(pkg_uri, str(PKG.hasCodeMetrics), str(code_metrics_uri))

        # For each language, create instances and metrics
        for language_name, byte_count in lang_data.items():
            lang_uri = language_uri(language_name)

            # ProgrammingLanguage instance
            self.writer.write_uri(
                lang_uri,
                'http://www.w3.org/1999/02/22-rdf-syntax-ns#type',
                str(MET.ProgrammingLanguage)
            )
            self.writer.write_lit(lang_uri, str(MET.languageName), language_name)

            # Link package to language
            self.writer.write_uri(pkg_uri, str(MET.implementedIn), lang_uri)

            # Create LanguageMetrics instance
            lang_metrics_uri = DATA[f"langmetrics/{pkg_uri.split('/')[-1]}/{language_name}"]
            self.writer.write_uri(
                str(lang_metrics_uri),
                'http://www.w3.org/1999/02/22-rdf-syntax-ns#type',
                str(MET.LanguageMetrics)
            )

            # Language proportion
            proportion = byte_count / total_bytes
            self.writer.write_lit(str(lang_metrics_uri), str(MET.languageProportion), f"{proportion:.4f}")

            # Link CodeMetrics to LanguageMetrics
            self.writer.write_uri(str(code_metrics_uri), str(MET.hasLanguageMetrics), str(lang_metrics_uri))

            # Link LanguageMetrics to ProgrammingLanguage
            self.writer.write_uri(str(lang_metrics_uri), str(MET.language), lang_uri)
