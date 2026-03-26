"""Content-addressable Minio upload via mc CLI."""

import hashlib
import os
import subprocess
from pathlib import Path


class MinioStore:
    """Uploads and downloads TDB2 snapshots to Minio using content-addressable paths."""

    def __init__(
        self,
        endpoint: str,
        bucket: str,
        access_key: str,
        secret_key: str,
        alias: str = "pgraph",
    ):
        self.endpoint = endpoint
        self.bucket = bucket
        self.access_key = access_key
        self.secret_key = secret_key
        self.alias = alias
        # Build MC_HOST value with embedded credentials
        if endpoint.startswith("https://"):
            mc_host = endpoint.rstrip("/").replace(
                "https://", f"https://{access_key}:{secret_key}@", 1
            )
        elif endpoint.startswith("http://"):
            mc_host = endpoint.rstrip("/").replace(
                "http://", f"http://{access_key}:{secret_key}@", 1
            )
        else:
            # Bare hostname — assume https
            mc_host = f"https://{access_key}:{secret_key}@{endpoint.rstrip('/')}"

        self._env = {
            **os.environ,
            f"MC_HOST_{alias}": mc_host,
        }

    def _hash_file(self, path: Path) -> str:
        """Compute SHA-256 hash of a file."""
        sha256 = hashlib.sha256()
        with open(path, "rb") as f:
            for chunk in iter(lambda: f.read(8192), b""):
                sha256.update(chunk)
        return sha256.hexdigest()

    def _mc(
        self, *args: str, check: bool = False, **kwargs
    ) -> subprocess.CompletedProcess:
        """Run an mc command."""
        cmd = ["mc", *args]
        return subprocess.run(
            cmd, env=self._env, capture_output=True, text=True, check=check, **kwargs
        )

    def upload_snapshot(self, tar_path: Path) -> str:
        """Upload a TDB2 tar snapshot using content-addressable storage.

        Returns the content hash string (e.g., "sha256-abc123...").
        """
        tar_path = Path(tar_path)
        hex_digest = self._hash_file(tar_path)
        content_hash = f"sha256-{hex_digest}"
        object_path = f"{self.alias}/{self.bucket}/tdb2/{content_hash}/tdb2.tar.gz"

        # Check if already uploaded
        result = self._mc("stat", object_path)
        if result.returncode == 0:
            # Already exists, just update latest pointer
            self._update_latest(content_hash)
            return content_hash

        # Upload the tar file
        result = self._mc("cp", str(tar_path), object_path)
        if result.returncode != 0:
            raise RuntimeError(
                f"Failed to upload snapshot to {object_path}: {result.stderr}"
            )

        # Update the latest pointer
        self._update_latest(content_hash)

        return content_hash

    def _update_latest(self, content_hash: str) -> None:
        """Update the tdb2/latest pointer file in Minio."""
        latest_path = f"{self.alias}/{self.bucket}/tdb2/latest"
        result = self._mc("pipe", latest_path, input=content_hash)
        if result.returncode != 0:
            raise RuntimeError(
                f"Failed to update latest pointer at {latest_path}: {result.stderr}"
            )

    def download_latest(self, dest_dir: Path) -> Path:
        """Download the latest TDB2 snapshot.

        Returns the local path to the downloaded tar.
        """
        dest_dir = Path(dest_dir)
        latest_path = f"{self.alias}/{self.bucket}/tdb2/latest"

        # Read the latest pointer
        result = self._mc("cat", latest_path)
        if result.returncode != 0:
            raise RuntimeError(
                f"Failed to read latest pointer at {latest_path}: {result.stderr}"
            )
        content_hash = result.stdout.strip()

        # Download the snapshot
        object_path = f"{self.alias}/{self.bucket}/tdb2/{content_hash}/tdb2.tar.gz"
        local_path = dest_dir / "tdb2.tar.gz"
        result = self._mc("cp", object_path, str(local_path))
        if result.returncode != 0:
            raise RuntimeError(
                f"Failed to download snapshot from {object_path}: {result.stderr}"
            )

        return local_path
