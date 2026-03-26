"""Tests for MinioStore content-addressable upload."""

from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from packagegraph.minio import MinioStore


@pytest.fixture
def store():
    return MinioStore(
        endpoint="https://minio.example.com",
        bucket="test-bucket",
        access_key="AKTEST",
        secret_key="secret123",
    )


@pytest.fixture
def fake_tar(tmp_path):
    """Create a real temp tar file with known content for hashing."""
    tar_file = tmp_path / "tdb2.tar.gz"
    tar_file.write_bytes(b"fake tar content for hashing")
    return tar_file


class TestUploadSnapshot:
    def test_upload_snapshot_uses_content_hash_in_path(self, store, fake_tar):
        """upload_snapshot should compute SHA-256 and use it in the Minio object path."""
        mock_stat = MagicMock(returncode=1)  # object does not exist
        mock_cp = MagicMock(returncode=0)
        mock_pipe = MagicMock(returncode=0)

        def run_side_effect(cmd, **kwargs):
            cmd_str = " ".join(cmd) if isinstance(cmd, list) else cmd
            if "stat" in cmd_str:
                return mock_stat
            if "cp" in cmd_str:
                return mock_cp
            if "pipe" in cmd_str or "tee" in cmd_str:
                return mock_pipe
            return MagicMock(returncode=0)

        with patch(
            "packagegraph.minio.subprocess.run", side_effect=run_side_effect
        ) as mock_run:
            result = store.upload_snapshot(fake_tar)

        assert result.startswith("sha256-")
        # The hash should appear in an mc cp call
        cp_calls = [
            c
            for c in mock_run.call_args_list
            if any("cp" in str(arg) for arg in c.args[0])
        ]
        assert len(cp_calls) >= 1
        # The destination path should contain the hash
        cp_cmd = cp_calls[0].args[0]
        cp_cmd_str = " ".join(cp_cmd)
        assert result in cp_cmd_str

    def test_upload_snapshot_skips_upload_if_hash_exists(self, store, fake_tar):
        """upload_snapshot should skip mc cp if the hash already exists in Minio."""
        mock_stat = MagicMock(returncode=0)  # object already exists

        def run_side_effect(cmd, **kwargs):
            cmd_str = " ".join(cmd) if isinstance(cmd, list) else cmd
            if "stat" in cmd_str:
                return mock_stat
            return MagicMock(returncode=0)

        with patch(
            "packagegraph.minio.subprocess.run", side_effect=run_side_effect
        ) as mock_run:
            result = store.upload_snapshot(fake_tar)

        assert result.startswith("sha256-")
        # Should NOT have called mc cp since the object already exists
        cp_calls = [
            c
            for c in mock_run.call_args_list
            if any("cp" in str(arg) for arg in c.args[0])
        ]
        assert len(cp_calls) == 0

    def test_upload_snapshot_updates_latest_pointer(self, store, fake_tar):
        """upload_snapshot should update the tdb2/latest pointer file."""
        mock_stat = MagicMock(returncode=1)  # object does not exist

        def run_side_effect(cmd, **kwargs):
            cmd_str = " ".join(cmd) if isinstance(cmd, list) else cmd
            if "stat" in cmd_str:
                return mock_stat
            return MagicMock(returncode=0)

        with patch(
            "packagegraph.minio.subprocess.run", side_effect=run_side_effect
        ) as mock_run:
            store.upload_snapshot(fake_tar)

        # Should have a call that writes to tdb2/latest
        all_calls_str = str(mock_run.call_args_list)
        assert "latest" in all_calls_str

    def test_upload_snapshot_returns_hash_string(self, store, fake_tar):
        """upload_snapshot should return a string like 'sha256-<hex>'."""
        mock_stat = MagicMock(returncode=0)  # exists, skip upload

        with patch("packagegraph.minio.subprocess.run", return_value=mock_stat):
            result = store.upload_snapshot(fake_tar)

        assert isinstance(result, str)
        assert result.startswith("sha256-")
        # SHA-256 hex digest is 64 characters
        hex_part = result[len("sha256-") :]
        assert len(hex_part) == 64

    def test_upload_snapshot_raises_on_cp_failure(self, store, fake_tar):
        """upload_snapshot should raise RuntimeError when mc cp fails."""

        def run_side_effect(cmd, **kwargs):
            cmd_str = " ".join(cmd) if isinstance(cmd, list) else cmd
            if "stat" in cmd_str:
                return MagicMock(returncode=1)  # not found
            if "cp" in cmd_str:
                return MagicMock(returncode=1, stderr="upload error")
            return MagicMock(returncode=0)

        with patch("packagegraph.minio.subprocess.run", side_effect=run_side_effect):
            with pytest.raises(RuntimeError, match="Failed to upload snapshot"):
                store.upload_snapshot(fake_tar)

    def test_upload_snapshot_raises_on_update_latest_failure(self, store, fake_tar):
        """upload_snapshot should raise RuntimeError when _update_latest (mc pipe) fails."""

        def run_side_effect(cmd, **kwargs):
            cmd_str = " ".join(cmd) if isinstance(cmd, list) else cmd
            if "stat" in cmd_str:
                return MagicMock(returncode=1)  # not found
            if "cp" in cmd_str:
                return MagicMock(returncode=0)
            if "pipe" in cmd_str:
                return MagicMock(returncode=1, stderr="pipe error")
            return MagicMock(returncode=0)

        with patch("packagegraph.minio.subprocess.run", side_effect=run_side_effect):
            with pytest.raises(RuntimeError, match="Failed to update latest pointer"):
                store.upload_snapshot(fake_tar)


class TestInit:
    def test_https_endpoint_embeds_credentials(self):
        """MC_HOST should embed credentials in https URL."""
        store = MinioStore(
            endpoint="https://minio.example.com",
            bucket="b",
            access_key="AK",
            secret_key="SK",
        )
        assert store._env["MC_HOST_pgraph"] == "https://AK:SK@minio.example.com"

    def test_http_endpoint_embeds_credentials(self):
        """MC_HOST should embed credentials in http URL."""
        store = MinioStore(
            endpoint="http://localhost:9000",
            bucket="b",
            access_key="AK",
            secret_key="SK",
        )
        assert store._env["MC_HOST_pgraph"] == "http://AK:SK@localhost:9000"

    def test_bare_hostname_assumes_https(self):
        """Bare hostname should produce https MC_HOST with credentials."""
        store = MinioStore(
            endpoint="minio.internal:9000",
            bucket="b",
            access_key="AK",
            secret_key="SK",
        )
        assert store._env["MC_HOST_pgraph"] == "https://AK:SK@minio.internal:9000"


class TestDownloadLatest:
    def test_download_latest_reads_pointer_and_downloads(self, store, tmp_path):
        """download_latest should read the latest pointer, then download the snapshot."""
        expected_hash = "sha256-abc123def456"

        def run_side_effect(cmd, **kwargs):
            cmd_str = " ".join(cmd) if isinstance(cmd, list) else cmd
            if "cat" in cmd_str:
                return MagicMock(returncode=0, stdout=expected_hash)
            return MagicMock(returncode=0)

        with patch(
            "packagegraph.minio.subprocess.run", side_effect=run_side_effect
        ) as mock_run:
            result = store.download_latest(tmp_path)

        assert isinstance(result, Path)
        assert result == tmp_path / "tdb2.tar.gz"

        # Verify mc cat was called to read the latest pointer
        cat_calls = [
            c
            for c in mock_run.call_args_list
            if "cat" in " ".join(c.args[0])
        ]
        assert len(cat_calls) == 1
        assert "tdb2/latest" in " ".join(cat_calls[0].args[0])

        # Verify mc cp was called with the content hash in the source path
        cp_calls = [
            c
            for c in mock_run.call_args_list
            if "cp" in " ".join(c.args[0])
        ]
        assert len(cp_calls) == 1
        cp_cmd = " ".join(cp_calls[0].args[0])
        assert expected_hash in cp_cmd
        assert str(tmp_path / "tdb2.tar.gz") in cp_cmd

    def test_download_latest_raises_on_cat_failure(self, store, tmp_path):
        """download_latest should raise RuntimeError when mc cat fails."""
        mock_result = MagicMock(returncode=1, stderr="no such object")

        with patch("packagegraph.minio.subprocess.run", return_value=mock_result):
            with pytest.raises(RuntimeError, match="Failed to read latest pointer"):
                store.download_latest(tmp_path)

    def test_download_latest_raises_on_cp_failure(self, store, tmp_path):
        """download_latest should raise RuntimeError when mc cp fails."""
        expected_hash = "sha256-abc123def456"

        def run_side_effect(cmd, **kwargs):
            cmd_str = " ".join(cmd) if isinstance(cmd, list) else cmd
            if "cat" in cmd_str:
                return MagicMock(returncode=0, stdout=expected_hash)
            if "cp" in cmd_str:
                return MagicMock(returncode=1, stderr="download failed")
            return MagicMock(returncode=0)

        with patch("packagegraph.minio.subprocess.run", side_effect=run_side_effect):
            with pytest.raises(RuntimeError, match="Failed to download snapshot"):
                store.download_latest(tmp_path)
