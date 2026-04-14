"""GitHub VCS metadata enricher using ontology-aligned GraphBuilder."""

import re
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


class GitHubEnricher:
    """Enriches package graph with GitHub repository metadata."""

    def __init__(
        self,
        graph: Graph,
        github_token: str | None = None,
        cache_dir: str | None = None,
        cache_ttl_hours: int = 24,
    ):
        self.graph = graph
        self.builder = GraphBuilder(graph)
        self.token = github_token
        self.cache_dir = Path(cache_dir) if cache_dir else None
        self.cache_ttl = timedelta(hours=cache_ttl_hours)
        self.api_base = "https://api.github.com"

        if self.cache_dir:
            self.cache_dir.mkdir(parents=True, exist_ok=True)

    def enrich(self):
        """Enrich graph with GitHub VCS metadata."""
        click.echo("Starting GitHub VCS enrichment...")

        # Discover GitHub URLs from package homepages
        github_repos = self._discover_github_repos()
        click.echo(f"Found {len(github_repos)} packages with GitHub homepages.")

        processed = set()
        for pkg_uri, owner, repo in github_repos:
            repo_key = f"{owner}/{repo}"
            if repo_key in processed:
                continue
            processed.add(repo_key)

            click.echo(f"  Fetching {repo_key}...")
            self._process_repo(pkg_uri, owner, repo)

        click.echo("GitHub enrichment complete.")

    def _discover_github_repos(self) -> list[tuple[URIRef, str, str]]:
        """Find packages with GitHub homepage URLs."""
        results = []
        github_pattern = re.compile(
            r"https?://github\.com/([^/]+)/([^/]+?)(?:\.git)?/?$"
        )

        for pkg_uri, _, homepage in self.graph.triples((None, PKG.homepage, None)):
            match = github_pattern.match(str(homepage))
            if match:
                owner, repo = match.group(1), match.group(2)
                results.append((pkg_uri, owner, repo))

        return results

    def _api_get(self, endpoint: str) -> dict | list | None:
        """Make authenticated GitHub API request with caching."""
        # Check cache
        if self.cache_dir:
            cache_key = endpoint.replace("/", "_")
            cache_file = self.cache_dir / f"{cache_key}.json"
            if cache_file.exists():
                age = datetime.now() - datetime.fromtimestamp(
                    cache_file.stat().st_mtime
                )
                if age < self.cache_ttl:
                    with open(cache_file) as f:
                        return json.load(f)

        headers = {"Accept": "application/vnd.github.v3+json"}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"

        try:
            url = f"{self.api_base}{endpoint}"
            response = requests.get(url, headers=headers, timeout=30)
            response.raise_for_status()

            # Rate limit awareness
            remaining = int(response.headers.get("X-RateLimit-Remaining", 100))
            if remaining < 100:
                time.sleep(2.0)

            data = response.json()

            # Cache response
            if self.cache_dir:
                cache_file = self.cache_dir / f"{cache_key}.json"
                with open(cache_file, "w") as f:
                    json.dump(data, f)

            return data
        except requests.exceptions.HTTPError as e:
            click.echo(f"    GitHub API error: {e}", err=True)
            return None
        except Exception as e:
            click.echo(f"    Error: {e}", err=True)
            return None

    def _process_repo(self, pkg_uri: URIRef, owner: str, repo_name: str):
        """Fetch repo metadata and commits, add to graph."""
        # Fetch repository metadata
        repo_data = self._api_get(f"/repos/{owner}/{repo_name}")
        if not repo_data:
            return

        # Create repository resource
        repo_url = repo_data.get("html_url", f"https://github.com/{owner}/{repo_name}")
        repo_uri = self.builder.add_repository(
            url=repo_url,
            default_branch=repo_data.get("default_branch"),
            description=repo_data.get("description"),
            stars=repo_data.get("stargazers_count"),
            forks=repo_data.get("forks_count"),
        )

        # Link upstream: resolve BinaryPackage → SourcePackage first (domain constraint)
        source_packages = list(self.graph.triples((pkg_uri, PKG.builtFromSource, None)))
        if source_packages:
            src_uri = source_packages[0][2]
            self.builder.link_upstream(src_uri, repo_name, repo_uri)
        else:
            # If no source package, check if pkg_uri IS a source package
            if (pkg_uri, RDF.type, PKG.SourcePackage) in self.graph:
                self.builder.link_upstream(pkg_uri, repo_name, repo_uri)

        # Fetch recent commits
        commits_data = self._api_get(f"/repos/{owner}/{repo_name}/commits?per_page=50")
        if commits_data:
            for commit_entry in commits_data[:50]:
                commit_info = commit_entry.get("commit", {})
                author_info = commit_info.get("author", {})

                self.builder.add_commit(
                    repo_uri_ref=repo_uri,
                    sha=commit_entry.get("sha", ""),
                    author_name=author_info.get("name"),
                    author_email=author_info.get("email"),
                    timestamp=author_info.get("date"),
                    message=commit_info.get("message"),
                )
