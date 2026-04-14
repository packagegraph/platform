"""VCS activity claim enricher — records GitHub activity metrics as attributed claims."""

import re
from packagegraph.enrichers.base import BaseEnricher
from packagegraph.enrichers.cache import CacheManager
from packagegraph.namespaces import VCS, DATA, repo_uri


class VCSActivityEnricher(BaseEnricher):
    """Enriches package graph with VCS activity claims from GitHub API.

    Queries GitHub for releases, contributor counts, and repository metrics.
    All data is recorded as point-in-time attributed claims — the DataSnapshot
    timestamp makes clear these are snapshots, not permanent assertions.
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
            enricher_name='vcs_activity',
            enricher_version=enricher_version,
        )
        self.cache = cache_manager
        self.token = github_token
        self.api_base = 'https://api.github.com'

    def _query_packages(self):
        """Query Fuseki for packages with GitHub homepages."""
        return self.client.query_github_homepages()

    def _process_item(self, item):
        """Process one package-homepage pair and extract VCS activity."""
        pkg_uri, homepage = item

        # Extract owner/repo from GitHub URL
        github_re = re.compile(r'https?://github\.com/([^/]+)/([^/]+?)(?:\.git)?/?$')
        match = github_re.match(homepage)
        if not match:
            return

        owner, repo_name = match.group(1), match.group(2)
        repo_url = f'https://github.com/{owner}/{repo_name}'
        r_uri = repo_uri(repo_url)

        # Get releases data
        releases_endpoint = f'/repos/{owner}/{repo_name}/releases'
        releases_data = self.cache.get(f'{self.api_base}{releases_endpoint}')

        if releases_data and isinstance(releases_data, list):
            for release in releases_data:
                tag_name = release.get('tag_name')
                if not tag_name:
                    continue

                release_uri = DATA[f"release/{owner}/{repo_name}/{tag_name}"]

                # Release instance
                self.writer.write_uri(
                    str(release_uri),
                    'http://www.w3.org/1999/02/22-rdf-syntax-ns#type',
                    str(VCS.Release)
                )
                self.writer.write_lit(str(release_uri), str(VCS.tagName), tag_name)

                if release.get('published_at'):
                    self.writer.write_lit(str(release_uri), str(VCS.releaseDate), release['published_at'])

                if release.get('name'):
                    self.writer.write_lit(str(release_uri), str(VCS.releaseName), release['name'])

                # Prerelease flag
                is_prerelease = release.get('prerelease', False)
                self.writer.write_lit(str(release_uri), str(VCS.isPreRelease), str(is_prerelease).lower())

                # Link to repository
                self.writer.write_uri(r_uri, str(VCS.hasRelease), str(release_uri))

        # Get repo metadata for activity metrics
        repo_endpoint = f'/repos/{owner}/{repo_name}'
        repo_data = self.cache.get(f'{self.api_base}{repo_endpoint}')

        if repo_data and isinstance(repo_data, dict):
            # First commit date (created_at is a proxy)
            if repo_data.get('created_at'):
                self.writer.write_lit(r_uri, str(VCS.firstCommitDate), repo_data['created_at'])

            # Last activity (pushed_at)
            if repo_data.get('pushed_at'):
                self.writer.write_lit(r_uri, str(VCS.lastActivity), repo_data['pushed_at'])

            # Point-in-time counts (clearly attributed via DataSnapshot timestamp)
            if repo_data.get('subscribers_count') is not None:
                self.writer.write_int(r_uri, str(VCS.subscriberCount), repo_data['subscribers_count'])

            if repo_data.get('open_issues_count') is not None:
                self.writer.write_int(r_uri, str(VCS.openIssueCount), repo_data['open_issues_count'])
