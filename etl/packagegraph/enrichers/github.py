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
        minio_endpoint: str | None = None,
        minio_bucket: str = 'packagegraph',
        minio_access_key: str | None = None,
        minio_secret_key: str | None = None,
    ):
        super().__init__(
            sparql_client=sparql_client,
            output_path=output_path,
            enricher_name='github',
            enricher_version='2.0.0',
        )
        self.token = github_token
        self.api_base = "https://api.github.com"

        if cache_dir:
            self.cache: CacheManager | None = CacheManager(
                cache_dir=cache_dir,
                enricher_name='github',
                minio_endpoint=minio_endpoint,
                minio_bucket=minio_bucket,
                minio_access_key=minio_access_key,
                minio_secret_key=minio_secret_key,
            )
            self.cache_ttl_hours: int = cache_ttl_hours
        else:
            self.cache = None
            self.cache_ttl_hours = cache_ttl_hours

        # Track processed repos to avoid duplicates
        self._processed_repos: set[str] = set()

    def _query_packages(self):
        """Query Fuseki for packages with GitHub homepages, deduplicated.

        Incremental: skips repos already enriched within the freshness threshold.
        The enrichment graph tracks per-repo `pkg:enrichedAt` timestamps, and
        the named graph implicitly scopes these to this enricher.

        Freshness threshold is controlled by ENRICHER_FRESHNESS_DAYS env var
        (default: 7 days). Set to 0 to force re-enrichment of everything.
        """
        import os
        freshness_days = int(os.environ.get('ENRICHER_FRESHNESS_DAYS', '7'))
        github_re = re.compile(r"https?://github\.com/([^/]+)/([^/]+?)(?:\.git)?/?$")

        # Query repos already enriched within the freshness window
        fresh_count = 0
        if freshness_days > 0:
            try:
                cutoff = (datetime.now() - __import__('datetime').timedelta(days=freshness_days)).isoformat()
                existing = self.client.query(f"""
                    PREFIX vcs: <https://purl.org/packagegraph/ontology/vcs#>
                    PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
                    SELECT ?url WHERE {{
                      GRAPH <https://packagegraph.github.io/graph/enrichment/github-vcs> {{
                        ?r vcs:repositoryURL ?url .
                        ?r pkg:enrichedAt ?ts .
                        FILTER(?ts > "{cutoff}"^^<http://www.w3.org/2001/XMLSchema#dateTime>)
                      }}
                    }}
                """)
                for b in existing:
                    m = github_re.match(b["url"]["value"])
                    if m:
                        self._processed_repos.add(f"{m.group(1)}/{m.group(2)}")
                fresh_count = len(self._processed_repos)
                if fresh_count:
                    print(f"Skipping {fresh_count} repos enriched within {freshness_days} days")
            except Exception as e:
                print(f"Warning: could not query enrichment graph: {e}")

        # Get all packages with GitHub homepages, deduplicate by repo
        all_items = self.client.query_github_homepages()
        seen = set()
        unique_items = []
        for pkg_uri, homepage in all_items:
            m = github_re.match(homepage)
            if not m:
                continue
            repo_key = f"{m.group(1)}/{m.group(2)}"
            if repo_key not in seen and repo_key not in self._processed_repos:
                seen.add(repo_key)
                unique_items.append((pkg_uri, homepage))

        total_unique = len(seen) + fresh_count
        print(f"Found {len(all_items)} packages, {total_unique} unique repos, {len(unique_items)} to process ({fresh_count} fresh, skipped)")
        return unique_items

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

                # Pace API calls to spread evenly across the rate limit window.
                # Uses X-RateLimit-Reset and X-RateLimit-Remaining to calculate
                # exact sleep needed instead of conservative fixed thresholds.
                remaining = int(response.headers.get("X-RateLimit-Remaining", "5000"))
                reset_at = int(response.headers.get("X-RateLimit-Reset", "0"))
                if remaining > 0 and reset_at > 0:
                    seconds_until_reset = max(reset_at - int(time.time()), 1)
                    # Spread remaining calls evenly, leave 5% buffer
                    sleep_per_call = seconds_until_reset / (remaining * 0.95)
                    # Clamp: never sleep more than 10s, never less than 0.05s
                    sleep_per_call = max(0.05, min(sleep_per_call, 10.0))
                    time.sleep(sleep_per_call)
                elif remaining <= 0:
                    # Exhausted — wait for reset
                    wait = max(reset_at - int(time.time()), 60)
                    print(f"    Rate limit exhausted. Waiting {wait}s for reset...")
                    time.sleep(wait)

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
            self.writer.write_lit(r_uri, str(PKG.enrichedAt), datetime.now().isoformat())
            self.writer.flag_quality_issue(
                r_uri, "dead-repo", "homepage",
                repo_url, "enrich-github-vcs"
            )
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
                    email = author["email"]
                    if ' ' not in email and '@' in email and '\\' not in email:
                        m_uri = str(maintainer_uri(email))
                        self.writer.write_uri(c_uri, str(VCS.authoredBy), m_uri)
                        self.writer.write_uri(m_uri, RDF_TYPE, str(PKG.Maintainer))
                        self.writer.write_lit(m_uri, str(FOAF.name), author["name"])
                    else:
                        self.writer.flag_quality_issue(
                            r_uri, "malformed-email", "commit.author.email",
                            email, "enrich-github-vcs"
                        )

        # Record when this repo was enriched — used for incremental freshness checks.
        # Scoped to this enricher via the named graph (graph/enrichment/github-vcs).
        self.writer.write_lit(r_uri, str(PKG.enrichedAt), datetime.now().isoformat())
