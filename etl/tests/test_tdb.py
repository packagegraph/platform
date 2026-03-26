"""Tests for TDB2Builder index builder."""

from unittest.mock import MagicMock, patch

import pytest

from packagegraph.tdb import TDB2Builder


@pytest.fixture
def builder():
    return TDB2Builder(jena_home="/opt/jena")


@pytest.fixture
def input_files(tmp_path):
    """Create fake input files for TDB2 loading."""
    f1 = tmp_path / "data.nt"
    f1.write_text("<s> <p> <o> .")
    f2 = tmp_path / "ontology.ttl"
    f2.write_text("@prefix ex: <http://example.org/> .")
    return [f1, f2]


class TestBuild:
    def test_build_calls_tdbloader_with_correct_args(
        self, builder, input_files, tmp_path
    ):
        """build should invoke tdb2.tdbloader with --loc and the input files."""
        output_dir = tmp_path / "tdb_output"
        output_dir.mkdir()

        mock_result = MagicMock(returncode=0)
        with patch(
            "packagegraph.tdb.subprocess.run", return_value=mock_result
        ) as mock_run:
            builder.build(input_files, output_dir)

        mock_run.assert_called_once()
        cmd = mock_run.call_args.args[0]
        assert cmd[0].endswith("tdb2.tdbloader") or "tdb2.tdbloader" in cmd[0]
        assert f"--loc={output_dir}" in cmd
        for f in input_files:
            assert str(f) in cmd

    def test_build_raises_on_failure(self, builder, input_files, tmp_path):
        """build should raise RuntimeError when tdb2.tdbloader exits non-zero."""
        output_dir = tmp_path / "tdb_output"
        output_dir.mkdir()

        mock_result = MagicMock(returncode=1, stderr="Error loading data")
        with patch("packagegraph.tdb.subprocess.run", return_value=mock_result):
            with pytest.raises(RuntimeError, match="tdb2.tdbloader failed"):
                builder.build(input_files, output_dir)

    def test_build_uses_jena_home_for_binary_path(self, input_files, tmp_path):
        """build should use jena_home to locate the tdb2.tdbloader binary."""
        custom_builder = TDB2Builder(jena_home="/custom/jena")
        output_dir = tmp_path / "tdb_output"
        output_dir.mkdir()

        mock_result = MagicMock(returncode=0)
        with patch(
            "packagegraph.tdb.subprocess.run", return_value=mock_result
        ) as mock_run:
            custom_builder.build(input_files, output_dir)

        cmd = mock_run.call_args.args[0]
        assert "/custom/jena" in cmd[0]


class TestPackage:
    def test_package_creates_tar_gz(self, builder, tmp_path):
        """package should create a tar.gz archive of the TDB2 directory."""
        tdb_dir = tmp_path / "tdb_data"
        tdb_dir.mkdir()
        (tdb_dir / "data.dat").write_bytes(b"fake tdb data")

        output_path = tmp_path / "output.tar.gz"
        builder.package(tdb_dir, output_path)

        assert output_path.exists()
        assert output_path.stat().st_size > 0

    def test_package_contains_tdb_files(self, builder, tmp_path):
        """package should include the contents of the TDB2 directory in the archive."""
        import tarfile

        tdb_dir = tmp_path / "tdb_data"
        tdb_dir.mkdir()
        (tdb_dir / "data.dat").write_bytes(b"fake tdb data")
        (tdb_dir / "index.dat").write_bytes(b"fake index")

        output_path = tmp_path / "output.tar.gz"
        builder.package(tdb_dir, output_path)

        with tarfile.open(output_path, "r:gz") as tar:
            names = tar.getnames()
            assert any("data.dat" in n for n in names)
            assert any("index.dat" in n for n in names)
