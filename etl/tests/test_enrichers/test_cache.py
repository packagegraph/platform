"""Tests for CacheManager with Minio tiering."""

import pytest
import json
import hashlib
from datetime import datetime, timedelta
from unittest.mock import MagicMock, patch
from packagegraph.enrichers.cache import CacheManager


def _compute_cache_key(url: str, params: dict | None = None) -> str:
    """Compute cache key (for test assertions)."""
    content = url
    if params:
        content = f"{url}?{json.dumps(params, sort_keys=True)}"
    return hashlib.sha256(content.encode()).hexdigest()


@pytest.mark.unit
class TestCacheManager:
    def test_get_returns_none_when_cache_miss(self, tmp_path):
        """Test cache miss returns None."""
        cache = CacheManager(cache_dir=str(tmp_path), enricher_name='test')
        result = cache.get('nonexistent_url')
        assert result is None

    def test_put_and_get_roundtrip(self, tmp_path):
        """Test putting data in cache and retrieving it."""
        cache = CacheManager(cache_dir=str(tmp_path), enricher_name='test')
        url = 'https://api.example.com/endpoint'
        data = {'foo': 'bar', 'baz': 42}

        cache.put(url, data, source_url=url, api_version='v1')
        result = cache.get(url)

        assert result is not None
        assert result['foo'] == 'bar'
        assert result['baz'] == 42

    def test_cache_key_is_content_addressed(self, tmp_path):
        """Test that cache keys are SHA-256 hashes of the request."""
        cache = CacheManager(cache_dir=str(tmp_path), enricher_name='test')
        url = 'https://api.example.com/endpoint?param=value'
        data = {'result': 'data'}

        cache.put(url, data, source_url=url, api_version='v1')

        # Verify cache file exists with hash-based name
        cache_key = _compute_cache_key(url)
        hash_prefix = cache_key[:2]
        cache_file = tmp_path / hash_prefix / f"{cache_key}.json"
        assert cache_file.exists()

    def test_metadata_envelope_structure(self, tmp_path):
        """Test cache entry includes metadata envelope."""
        cache = CacheManager(cache_dir=str(tmp_path), enricher_name='test')
        url = 'https://api.example.com/test'
        data = {'value': 123}

        cache.put(url, data, source_url=url, api_version='v2.0', ttl_hours=48)

        # Read cache file directly
        cache_key = _compute_cache_key(url)
        hash_prefix = cache_key[:2]
        cache_file = tmp_path / hash_prefix / f"{cache_key}.json"
        with open(cache_file) as f:
            envelope = json.load(f)

        assert 'fetched_at' in envelope
        assert 'source_url' in envelope
        assert envelope['source_url'] == url
        assert envelope['api_version'] == 'v2.0'
        assert envelope['ttl_hours'] == 48
        assert envelope['data'] == data

    def test_ttl_expiration(self, tmp_path):
        """Test that expired cache entries return None."""
        cache = CacheManager(cache_dir=str(tmp_path), enricher_name='test')
        url = 'https://api.example.com/expiring'
        data = {'value': 'old'}

        # Put with short TTL
        cache.put(url, data, source_url=url, api_version='v1', ttl_hours=1)

        # Manually backdating the file to make it expired
        cache_key = _compute_cache_key(url)
        hash_prefix = cache_key[:2]
        cache_file = tmp_path / hash_prefix / f"{cache_key}.json"

        # Modify the fetched_at timestamp to 2 hours ago
        with open(cache_file) as f:
            envelope = json.load(f)
        envelope['fetched_at'] = (datetime.now() - timedelta(hours=2)).isoformat()
        with open(cache_file, 'w') as f:
            json.dump(envelope, f)

        # Should return None (expired)
        result = cache.get(url)
        assert result is None

    def test_archive_to_minio_moves_old_entries(self, tmp_path):
        """Test archiving entries older than threshold to Minio."""
        cache = CacheManager(
            cache_dir=str(tmp_path),
            enricher_name='test_enricher',
            minio_endpoint='http://minio:9000',
            minio_bucket='packagegraph',
            minio_access_key='key',
            minio_secret_key='secret'
        )

        # Create a cache entry
        url = 'https://api.example.com/old'
        data = {'old': 'data'}
        cache.put(url, data, source_url=url, api_version='v1')

        # Backdate the file
        cache_key = _compute_cache_key(url)
        hash_prefix = cache_key[:2]
        cache_file = tmp_path / hash_prefix / f"{cache_key}.json"
        with open(cache_file) as f:
            envelope = json.load(f)
        envelope['fetched_at'] = (datetime.now() - timedelta(days=10)).isoformat()
        with open(cache_file, 'w') as f:
            json.dump(envelope, f)

        # Mock mc CLI calls
        with patch('subprocess.run') as mock_run:
            mock_run.return_value = MagicMock(returncode=0, stdout='', stderr='')
            cache.archive_to_minio(older_than_days=7)

            # Verify mc cp was called for the old entry
            assert mock_run.called
            args_list = [call.args[0] for call in mock_run.call_args_list]
            # Should have called mc cp with source and dest
            mc_cp_calls = [args for args in args_list if 'cp' in args]
            assert len(mc_cp_calls) > 0

    def test_get_retrieves_from_minio_on_local_miss(self, tmp_path):
        """Test that get() checks Minio when local cache misses."""
        cache = CacheManager(
            cache_dir=str(tmp_path),
            enricher_name='test',
            minio_endpoint='http://minio:9000',
            minio_bucket='packagegraph',
            minio_access_key='key',
            minio_secret_key='secret'
        )

        url = 'https://api.example.com/nearline'
        minio_data = {
            'fetched_at': datetime.now().isoformat(),
            'source_url': url,
            'api_version': 'v1',
            'ttl_hours': 24,
            'data': {'from': 'minio'}
        }

        # Mock mc cat to return the cache entry
        with patch('subprocess.run') as mock_run:
            mock_run.return_value = MagicMock(
                returncode=0,
                stdout=json.dumps(minio_data),
                stderr=''
            )

            result = cache.get(url)

            # Should have retrieved from Minio
            assert result is not None
            assert result['from'] == 'minio'
            # Verify mc cat was called
            assert any('cat' in str(call.args) for call in mock_run.call_args_list)

    def test_manifest_generation(self, tmp_path):
        """Test cache manifest lists all entries."""
        cache = CacheManager(cache_dir=str(tmp_path), enricher_name='test')

        # Add multiple entries
        for i in range(3):
            url = f'https://api.example.com/entry{i}'
            cache.put(url, {'index': i}, source_url=url, api_version='v1')

        manifest = cache.generate_manifest()

        assert len(manifest['entries']) == 3
        assert manifest['enricher_name'] == 'test'
        assert 'generated_at' in manifest
        for entry in manifest['entries']:
            assert 'cache_key' in entry
            assert 'fetched_at' in entry
            assert 'source_url' in entry
            assert 'size_bytes' in entry
