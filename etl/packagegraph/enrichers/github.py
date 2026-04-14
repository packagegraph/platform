"""GitHub VCS enricher — queries Fuseki for homepages, calls GitHub API, writes N-Triples."""

import re
import time
from datetime import datetime
import requests
from .base import BaseEnricher
from .cache import CacheManager
from ..sparql_client import SparqlQueryClient
from ..namespaces import VCS, PKG, FOAF, DATA, repo_uri, maintainer_uri

# Sentinel cached for repos that return 404
_NOT_FOUND = {"_status": "not_found"}


class GitHubEnricher(BaseEnricher):
    """Enriches the package graph with GitHub repository metadata.

    Reads package homepages from Fuseki via SPARQL.
    Writes VCS triples to an N-Triples file for loading via pg-collect load.
    """

    def __init__(
        self,
        sparql_client: SparqlQueryClient,
        output_path: str,
        github_token: str | None = None,
        cache_dir: str | None = None,
        cache_ttl_hours: int = 24,
    ):
        super().__init__(
            sparql_client=sparql_client,
            output_path=output_path,
            enricher_name='github',
            enricher_version='2.0.0',
        )
        self.token = github_token
        self.api_base = "https://api.github.com"

        # Create CacheManager with backward-compatible TTL
        if cache_dir:
            self.cache: CacheManager | None = CacheManager(
                cache_dir=cache_dir,
                enricher_name='github',
                minio_endpoint=None  # Minio integration deferred for backward compat
            )
            self.cache_ttl_hours: int = cache_ttl_hours
        else:
            self.cache = None
            self.cache_ttl_hours = cache_ttl_hours

        # Track processed repos to avoid duplicates
        self._processed_repos: set[str] = set()

    def _query_packages(self):
        """Query Fuseki for packages with GitHub homepages."""
        return self.client.query_github_homepages()

    def _process_item(self, item):
        """Process one package-homepage pair."""
        pkg_uri_str, homepage = item

        github_re = re.compile(r"https?://github\.com/([^/]+)/([^/]+?)(?:\.git)?/?$")
        match = github_re.match(homepage)
        if not match:
            return

        owner, repo_name = match.group(1), match.group(2)
        repo_key = f"{owner}/{repo_name}"

        # Skip if already processed
        if repo_key in self._processed_repos:
            return
        self._processed_repos.add(repo_key)

        self._process_repo(pkg_uri_str, owner, repo_name)

    def _preflight_check(self):
        """Validate GitHub API access before processing items.

        Called by enrich() before the main loop. Fails fast on auth errors
        instead of making 73K failing requests.
        """
        headers = {"Accept": "application/vnd.github.v3+json"}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        try:
            response = requests.get(
                f"{self.api_base}/rate_limit", headers=headers, timeout=10
            )
            if response.status_code == 401:
                raise RuntimeError(
                    "GitHub API authentication failed (401). "
                    "Check GITHUB_TOKEN secret — it may be a placeholder, expired, or revoked."
                )
            response.raise_for_status()
            rate = response.json().get("rate", {})
            remaining = rate.get("remaining", 0)
            limit = rate.get("limit", 0)
            print(f"GitHub API preflight OK: {remaining}/{limit} requests remaining")
            if limit <= 60:
                print(
                    "WARNING: Using unauthenticated rate limit (60/hr). "
                    "Set GITHUB_TOKEN for 5000/hr."
                )
        except RuntimeError:
            raise
        except Exception as e:
            raise RuntimeError(f"GitHub API preflight failed: {e}") from e

    def _api_get(self, endpoint: str):
        """Fetch from GitHub API with caching, backoff, and error classification."""
        url = f"{self.api_base}{endpoint}"

        # Check cache first if available
        if self.cache:
            cached = self.cache.get(url)
            if cached is not None:
                return cached

        # Fetch from API with retry/backoff
        headers = {"Accept": "application/vnd.github.v3+json"}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"

        max_retries = 3
        for attempt in range(max_retries + 1):
            try:
                response = requests.get(url, headers=headers, timeout=30)

                # Classify response
                if response.status_code == 401:
                    raise RuntimeError(
                        "GitHub token revoked or expired mid-run (401). Aborting."
                    )

                if response.status_code == 403:
                    # Rate limited — wait for reset
                    reset_time = int(response.headers.get("X-RateLimit-Reset", "0"))
                    if reset_time:
                        wait = max(reset_time - int(time.time()), 1)
                        print(f"    Rate limited. Waiting {wait}s for reset...")
                        time.sleep(min(wait, 900))  # Cap at 15 min
                        continue
                    # Secondary rate limit (no reset header)
                    delay = min(60 * (2 ** attempt), 300)
                    print(f"    Secondary rate limit. Backing off {delay}s...")
                    time.sleep(delay)
                    continue

                if response.status_code == 404:
                    return _NOT_FOUND

                if response.status_code >= 500:
                    if attempt < max_retries:
                        delay = min(5 * (2 ** attempt), 60)
                        print(f"    Server error {response.status_code}, retrying in {delay}s...")
                        time.sleep(delay)
                        continue
                    print(f"    GitHub API server error {response.status_code} after {max_retries} retries")
                    return None

                response.raise_for_status()

                # Adaptive rate limiting based on remaining quota
                remaining = int(response.headers.get("X-RateLimit-Remaining", "100"))
                if remaining < 50:
                    time.sleep(5.0)
                elif remaining < 200:
                    time.sleep(1.0)
                elif remaining < 500:
                    time.sleep(0.2)

                data = response.json()

                # Store in cache
                if self.cache:
                    self.cache.put(
                        url=url,
                        data=data,
                        source_url=url,
                        api_version='v3',
                        ttl_hours=self.cache_ttl_hours
                    )

                return data

            except RuntimeError:
                raise
            except requests.exceptions.Timeout:
                if attempt < max_retries:
                    delay = min(10 * (2 ** attempt), 60)
                    print(f"    Timeout, retrying in {delay}s...")
                    time.sleep(delay)
                    continue
                print(f"    Timeout after {max_retries} retries: {url}")
                return None
            except Exception as e:
                print(f"    GitHub API error: {e}")
                return None

        return None

    def _process_repo(self, pkg_uri_str, owner, repo_name):
        """Process one repository and write triples via self.writer."""
        RDF_TYPE = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
        repo_data = self._api_get(f"/repos/{owner}/{repo_name}")
        if repo_data is None:
            return

        repo_url = f"https://github.com/{owner}/{repo_name}"
        r_uri = str(repo_uri(repo_url))

        # Record dead/private repos — only the repo endpoint 404 means the repo is gone
        if repo_data is _NOT_FOUND:
            self.writer.write_uri(r_uri, RDF_TYPE, str(VCS.Repository))
            self.writer.write_uri(r_uri, str(VCS.repositoryURL), repo_url)
            self.writer.write_lit(r_uri, str(VCS.repositoryStatus), "not-found")
            self.writer.write_lit(r_uri, str(VCS.statusCheckedAt), datetime.now().isoformat())
            # Cache so we don't re-check on future runs
            if self.cache:
                self.cache.put(
                    url=f"{self.api_base}/repos/{owner}/{repo_name}",
                    data=_NOT_FOUND,
                    source_url=f"{self.api_base}/repos/{owner}/{repo_name}",
                    api_version='v3',
                    ttl_hours=self.cache_ttl_hours,
                )
            return

        repo_url = repo_data.get("html_url", repo_url)
        r_uri = str(repo_uri(repo_url))

        self.writer.write_uri(r_uri, RDF_TYPE, str(VCS.Repository))
        self.writer.write_uri(r_uri, str(VCS.repositoryURL), repo_url)
        if repo_data.get("default_branch"):
            self.writer.write_lit(r_uri, str(VCS.defaultBranch), repo_data["default_branch"])
        if repo_data.get("description"):
            self.writer.write_lit(r_uri, str(VCS.repositoryDescription), repo_data["description"])
        if repo_data.get("stargazers_count") is not None:
            self.writer.write_int(r_uri, str(VCS.starCount), repo_data["stargazers_count"])
        if repo_data.get("forks_count") is not None:
            self.writer.write_int(r_uri, str(VCS.forkCount), repo_data["forks_count"])

        # Fetch recent commits — 404 here just means no commits, not a dead repo
        commits = self._api_get(f"/repos/{owner}/{repo_name}/commits?per_page=50")
        if isinstance(commits, list):
            for entry in commits[:50]:
                sha = entry.get("sha", "")
                if not sha:
                    continue
                c_uri = str(DATA[f"commit/{sha[:12]}"])
                info = entry.get("commit", {})
                author = info.get("author", {})

                self.writer.write_uri(c_uri, RDF_TYPE, str(VCS.Commit))
                self.writer.write_lit(c_uri, str(VCS.commitHash), sha)
                self.writer.write_uri(r_uri, str(VCS.hasCommit), c_uri)

                if author.get("date"):
                    self.writer.write_lit(c_uri, str(VCS.commitDate), author["date"])
                if info.get("message"):
                    self.writer.write_lit(c_uri, str(VCS.commitMessage), info["message"][:500])
                if author.get("name") and author.get("email"):
                    m_uri = str(maintainer_uri(author["email"]))
                    self.writer.write_uri(c_uri, str(VCS.authoredBy), m_uri)
                    self.writer.write_uri(m_uri, RDF_TYPE, str(PKG.Maintainer))
                    self.writer.write_lit(m_uri, str(FOAF.name), author["name"])
