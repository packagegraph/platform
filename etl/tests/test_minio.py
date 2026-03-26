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


class TestDownloadLatest:
    def test_download_latest_reads_pointer_and_downloads(self, store, tmp_path):
        """download_latest should read the latest pointer, then download the snapshot."""
        expected_hash = "sha256-abc123def456"

        def run_side_effect(cmd, **kwargs):
            cmd_str = " ".join(cmd) if isinstance(cmd, list) else cmd
            if "cat" in cmd_str:
                return MagicMock(returncode=0, stdout=expected_hash)
            return MagicMock(returncode=0)

        with patch("packagegraph.minio.subprocess.run", side_effect=run_side_effect):
            result = store.download_latest(tmp_path)

        assert isinstance(result, Path)
