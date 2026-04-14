"""GitHub VCS enricher — queries Fuseki for homepages, calls GitHub API, writes N-Triples."""

import re
import json
import time
from pathlib import Path
from datetime import datetime, timedelta
import requests
from ..sparql_client import SparqlQueryClient
from ..namespaces import VCS, PKG, FOAF, DATA, repo_uri, maintainer_uri


class GitHubEnricher:
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
        self.client = sparql_client
        self.output_path = output_path
        self.token = github_token
        self.cache_dir = Path(cache_dir) if cache_dir else None
        self.cache_ttl = timedelta(hours=cache_ttl_hours)
        self.api_base = "https://api.github.com"
        if self.cache_dir:
            self.cache_dir.mkdir(parents=True, exist_ok=True)

    def enrich(self):
        print("Querying Fuseki for packages with GitHub homepages...")
        pairs = self.client.query_github_homepages()
        print(f"Found {len(pairs)} packages with GitHub URLs.")

        github_re = re.compile(r"https?://github\.com/([^/]+)/([^/]+?)(?:\.git)?/?$")
        processed = set()

        with open(self.output_path, "w") as f:
            for pkg_uri_str, homepage in pairs:
                match = github_re.match(homepage)
                if not match:
                    continue
                owner, repo_name = match.group(1), match.group(2)
                repo_key = f"{owner}/{repo_name}"
                if repo_key in processed:
                    continue
                processed.add(repo_key)
                print(f"  Fetching {repo_key}...")
                self._process_repo(f, pkg_uri_str, owner, repo_name)

        print(f"GitHub enrichment complete. Output: {self.output_path}")

    def _api_get(self, endpoint: str):
        cache_key = endpoint.replace("/", "_")
        if self.cache_dir:
            cache_file = self.cache_dir / f"{cache_key}.json"
            if cache_file.exists():
                age = datetime.now() - datetime.fromtimestamp(
                    cache_file.stat().st_mtime
                )
                if age < self.cache_ttl:
                    with open(cache_file) as cf:
                        return json.load(cf)

        headers = {"Accept": "application/vnd.github.v3+json"}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        try:
            response = requests.get(
                f"{self.api_base}{endpoint}", headers=headers, timeout=30
            )
            response.raise_for_status()
            remaining = int(response.headers.get("X-RateLimit-Remaining", "100"))
            if remaining < 100:
                time.sleep(2.0)
            data = response.json()
            if self.cache_dir:
                cache_file = self.cache_dir / f"{cache_key}.json"
                with open(cache_file, "w") as cf:
                    json.dump(data, cf)
            return data
        except Exception as e:
            print(f"    GitHub API error: {e}")
            return None

    def _process_repo(self, f, pkg_uri_str, owner, repo_name):
        RDF_TYPE = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
        repo_data = self._api_get(f"/repos/{owner}/{repo_name}")
        if not repo_data:
            return

        repo_url = repo_data.get("html_url", f"https://github.com/{owner}/{repo_name}")
        r_uri = str(repo_uri(repo_url))

        f.write(f"<{r_uri}> <{RDF_TYPE}> <{VCS}Repository> .\n")
        f.write(f"<{r_uri}> <{VCS}repositoryURL> <{repo_url}> .\n")
        if repo_data.get("default_branch"):
            _write_lit(f, r_uri, f"{VCS}defaultBranch", repo_data["default_branch"])
        if repo_data.get("description"):
            _write_lit(
                f, r_uri, f"{VCS}repositoryDescription", repo_data["description"]
            )
        if repo_data.get("stargazers_count") is not None:
            _write_int(f, r_uri, f"{VCS}starCount", repo_data["stargazers_count"])
        if repo_data.get("forks_count") is not None:
            _write_int(f, r_uri, f"{VCS}forkCount", repo_data["forks_count"])

        # Fetch recent commits
        commits = self._api_get(f"/repos/{owner}/{repo_name}/commits?per_page=50")
        if commits:
            for entry in commits[:50]:
                sha = entry.get("sha", "")
                if not sha:
                    continue
                c_uri = f"{DATA}commit/{sha[:12]}"
                info = entry.get("commit", {})
                author = info.get("author", {})
                f.write(f"<{c_uri}> <{RDF_TYPE}> <{VCS}Commit> .\n")
                _write_lit(f, c_uri, f"{VCS}commitHash", sha)
                f.write(f"<{r_uri}> <{VCS}hasCommit> <{c_uri}> .\n")
                if author.get("date"):
                    _write_lit(f, c_uri, f"{VCS}commitDate", author["date"])
                if info.get("message"):
                    _write_lit(f, c_uri, f"{VCS}commitMessage", info["message"][:500])
                if author.get("name") and author.get("email"):
                    m_uri = str(maintainer_uri(author["email"]))
                    f.write(f"<{c_uri}> <{VCS}authoredBy> <{m_uri}> .\n")
                    f.write(f"<{m_uri}> <{RDF_TYPE}> <{PKG}Maintainer> .\n")
                    _write_lit(f, m_uri, f"{FOAF}name", author["name"])


def _escape_nt(s: str) -> str:
    return (
        s.replace("\\", "\\\\")
        .replace('"', '\\"')
        .replace("\n", "\\n")
        .replace("\r", "\\r")
        .replace("\t", "\\t")
    )


def _write_lit(f, subj, pred, val):
    f.write(f'<{subj}> <{pred}> "{_escape_nt(str(val))}" .\n')


def _write_int(f, subj, pred, val):
    f.write(
        f'<{subj}> <{pred}> "{val}"^^<http://www.w3.org/2001/XMLSchema#integer> .\n'
    )
