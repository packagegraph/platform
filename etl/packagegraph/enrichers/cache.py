"""Content-addressed cache with Minio tiering for enrichers.

Three-tier cache model:
  - Hot (local disk): Active cache during ETL runs. Entries expire by TTL.
  - Nearline (Minio enricher-cache/): Archived entries, compressed.
    Retrievable by get() on cache miss. Retained for 90 days by default.
  - Cold (Minio lifecycle → deletion): After 90 days, Minio lifecycle
    policy deletes entries. The enrichment triples persist in Fuseki;
    only the raw API evidence is aged out.

Minio lifecycle policy requirements:
  - Bucket: packagegraph (or configured bucket)
  - Prefix: enricher-cache/
  - Rule: Delete objects older than 90 days (or transition to GLACIER/DEEP_ARCHIVE)
  - Configure via: mc ilm add myminio/packagegraph --prefix enricher-cache/ --expiry-days 90

The cache is shared across enrichers by API endpoint (GitHub API /repos response
is the same regardless of which enricher requests it). Provenance is per-enricher
via PROV-O activity triples, not cache-level.
"""

import json
import hashlib
import subprocess
from pathlib import Path
from datetime import datetime, timedelta
from typing import Any


class CacheManager:
    """Content-addressed cache with hot/nearline/cold tiering."""

    def __init__(
        self,
        cache_dir: str,
        enricher_name: str,
        minio_endpoint: str | None = None,
        minio_bucket: str = 'packagegraph',
        minio_access_key: str | None = None,
        minio_secret_key: str | None = None,
        minio_alias: str = 'pgraph',
    ):
        self.cache_dir: Path = Path(cache_dir)
        self.enricher_name: str = enricher_name
        self.minio_endpoint: str | None = minio_endpoint
        self.minio_bucket: str = minio_bucket
        self.minio_access_key: str | None = minio_access_key
        self.minio_secret_key: str | None = minio_secret_key
        self.minio_alias: str = minio_alias

        self.cache_dir.mkdir(parents=True, exist_ok=True)

        # Build MC_HOST env var if Minio is configured
        self._minio_env: dict[str, str] | None = None
        if self.minio_endpoint and self.minio_access_key and self.minio_secret_key:
            if self.minio_endpoint.startswith('https://'):
                mc_host = self.minio_endpoint.rstrip('/').replace(
                    'https://', f'https://{self.minio_access_key}:{self.minio_secret_key}@', 1
                )
            elif self.minio_endpoint.startswith('http://'):
                mc_host = self.minio_endpoint.rstrip('/').replace(
                    'http://', f'http://{self.minio_access_key}:{self.minio_secret_key}@', 1
                )
            else:
                mc_host = f'https://{self.minio_access_key}:{self.minio_secret_key}@{self.minio_endpoint.rstrip("/")}'

            import os
            self._minio_env = {
                **os.environ,
                f'MC_HOST_{self.minio_alias}': mc_host,
            }

    def _compute_key(self, url: str, params: dict[str, Any] | None = None) -> str:
        """Compute SHA-256 cache key from URL and optional params."""
        content = url
        if params:
            content = f"{url}?{json.dumps(params, sort_keys=True)}"
        return hashlib.sha256(content.encode()).hexdigest()

    def _cache_file_path(self, cache_key: str) -> Path:
        """Get local cache file path with hash prefix directory."""
        hash_prefix = cache_key[:2]
        prefix_dir = self.cache_dir / hash_prefix
        prefix_dir.mkdir(parents=True, exist_ok=True)
        return prefix_dir / f"{cache_key}.json"

    def get(self, url: str, params: dict | None = None) -> dict | None:
        """Get cached data for URL, checking local then Minio.

        Returns the data payload (from envelope['data']) or None if not found/expired.
        """
        cache_key = self._compute_key(url, params)
        cache_file = self._cache_file_path(cache_key)

        # Check local disk (hot cache)
        if cache_file.exists():
            with open(cache_file) as f:
                envelope = json.load(f)

            # Check TTL
            fetched_at = datetime.fromisoformat(envelope['fetched_at'])
            ttl = timedelta(hours=envelope.get('ttl_hours', 24))
            if datetime.now() - fetched_at < ttl:
                return envelope['data']
            # Expired, fall through to Minio

        # Check Minio (nearline cache)
        if self._minio_env:
            minio_data = self._get_from_minio(cache_key)
            if minio_data:
                # Cache locally for future access
                with open(cache_file, 'w') as f:
                    json.dump(minio_data, f)
                return minio_data['data']

        return None

    def put(
        self,
        url: str,
        data: dict,
        source_url: str,
        api_version: str,
        params: dict | None = None,
        ttl_hours: int = 24,
    ) -> None:
        """Store data in cache with metadata envelope."""
        cache_key = self._compute_key(url, params)
        cache_file = self._cache_file_path(cache_key)

        envelope = {
            'fetched_at': datetime.now().isoformat(),
            'source_url': source_url,
            'api_version': api_version,
            'ttl_hours': ttl_hours,
            'data': data,
        }

        with open(cache_file, 'w') as f:
            json.dump(envelope, f)

    def _get_from_minio(self, cache_key: str) -> dict | None:
        """Retrieve cache entry from Minio nearline storage."""
        if not self._minio_env:
            return None

        hash_prefix = cache_key[:2]
        minio_path = f"{self.minio_alias}/{self.minio_bucket}/enricher-cache/{self.enricher_name}/{hash_prefix}/{cache_key}.json"

        result = subprocess.run(
            ['mc', 'cat', minio_path],
            env=self._minio_env,
            capture_output=True,
            text=True,
        )

        if result.returncode == 0:
            try:
                return json.loads(result.stdout)
            except json.JSONDecodeError:
                return None
        return None

    def archive_to_minio(self, older_than_days: int) -> int:
        """Move cache entries older than threshold from local disk to Minio.

        Returns the number of entries archived.
        """
        if not self._minio_env:
            return 0

        archived_count = 0
        threshold = datetime.now() - timedelta(days=older_than_days)

        # Walk all cache files
        for cache_file in self.cache_dir.rglob('*.json'):
            if cache_file.name == 'manifest.json':
                continue

            # Read envelope to check age
            try:
                with open(cache_file) as f:
                    envelope = json.load(f)
                fetched_at = datetime.fromisoformat(envelope['fetched_at'])

                if fetched_at < threshold:
                    # Archive to Minio
                    cache_key = cache_file.stem
                    hash_prefix = cache_key[:2]
                    minio_path = f"{self.minio_alias}/{self.minio_bucket}/enricher-cache/{self.enricher_name}/{hash_prefix}/{cache_key}.json"

                    result = subprocess.run(
                        ['mc', 'cp', str(cache_file), minio_path],
                        env=self._minio_env,
                        capture_output=True,
                        text=True,
                    )

                    if result.returncode == 0:
                        # Remove from local disk
                        cache_file.unlink()
                        archived_count += 1
                    else:
                        print(f"Warning: Failed to archive {cache_file.name}: {result.stderr}")
            except (json.JSONDecodeError, KeyError, ValueError) as e:
                print(f"Warning: Skipping malformed cache file {cache_file}: {e}")

        return archived_count

    def list_entries(self) -> list[dict]:
        """List all cache entries with metadata."""
        entries = []
        for cache_file in self.cache_dir.rglob('*.json'):
            if cache_file.name == 'manifest.json':
                continue

            try:
                with open(cache_file) as f:
                    envelope = json.load(f)
                entries.append({
                    'cache_key': cache_file.stem,
                    'source_url': envelope['source_url'],
                    'fetched_at': envelope['fetched_at'],
                    'api_version': envelope.get('api_version', 'unknown'),
                    'size_bytes': cache_file.stat().st_size,
                })
            except (json.JSONDecodeError, KeyError):
                continue

        return sorted(entries, key=lambda e: e['fetched_at'], reverse=True)

    def generate_manifest(self) -> dict:
        """Generate cache manifest listing all entries."""
        return {
            'enricher_name': self.enricher_name,
            'generated_at': datetime.now().isoformat(),
            'entries': self.list_entries(),
        }

    def sync_to_minio(self) -> int:
        """Upload all local cache entries to Minio nearline storage.

        Returns the number of entries synced. Uses mc mirror for efficiency.
        """
        if not self._minio_env:
            return 0

        minio_path = f"{self.minio_alias}/{self.minio_bucket}/enricher-cache/{self.enricher_name}/"
        result = subprocess.run(
            ['mc', 'mirror', '--overwrite', str(self.cache_dir) + '/', minio_path],
            env=self._minio_env,
            capture_output=True,
            text=True,
        )

        if result.returncode != 0:
            print(f"Warning: cache sync to Minio failed: {result.stderr[:200]}")
            return 0

        count = sum(1 for _ in self.cache_dir.rglob('*.json') if _.name != 'manifest.json')
        return count

    def sync_from_minio(self) -> int:
        """Download cache entries from Minio to local disk.

        Returns the number of entries synced.
        """
        if not self._minio_env:
            return 0

        minio_path = f"{self.minio_alias}/{self.minio_bucket}/enricher-cache/{self.enricher_name}/"
        result = subprocess.run(
            ['mc', 'mirror', '--overwrite', minio_path, str(self.cache_dir) + '/'],
            env=self._minio_env,
            capture_output=True,
            text=True,
        )

        if result.returncode != 0:
            # Not an error — Minio path may not exist yet
            return 0

        count = sum(1 for _ in self.cache_dir.rglob('*.json') if _.name != 'manifest.json')
        return count

    def purge_expired(self) -> int:
        """Remove expired entries from local disk.

        Returns the number of entries purged.
        """
        purged_count = 0
        for cache_file in self.cache_dir.rglob('*.json'):
            if cache_file.name == 'manifest.json':
                continue

            try:
                with open(cache_file) as f:
                    envelope = json.load(f)
                fetched_at = datetime.fromisoformat(envelope['fetched_at'])
                ttl = timedelta(hours=envelope.get('ttl_hours', 24))

                if datetime.now() - fetched_at >= ttl:
                    cache_file.unlink()
                    purged_count += 1
            except (json.JSONDecodeError, KeyError, ValueError):
                continue

        return purged_count
