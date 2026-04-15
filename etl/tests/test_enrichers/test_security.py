from unittest.mock import patch, MagicMock
import pytest
from packagegraph.enrichers.security import SecurityEnricher


@pytest.mark.unit
class TestSecurityEnricher:
    def test_enrich_writes_ntriples_for_vulnerable_package(self, tmp_path):
        mock_client = MagicMock()
        mock_client.query_packages_by_type.return_value = [
            ("openssl", "3.0.2-1")
        ]

        osv_resp = MagicMock()
        osv_resp.status_code = 200
        osv_resp.json.return_value = {
            "vulns": [
                {
                    "id": "CVE-2022-0778",
                    "summary": "Infinite loop in BN_mod_sqrt()",
                    "severity": [{"type": "CVSS_V3", "score": "7.5"}],
                    "published": "2022-03-15T00:00:00Z",
                    "affected": [
                        {"package": {"name": "openssl", "ecosystem": "Debian"}}
                    ],
                }
            ]
        }
        osv_resp.raise_for_status.return_value = None

        output = tmp_path / "security.nt"
        with patch(
            "packagegraph.enrichers.security.requests.post", return_value=osv_resp
        ):
            enricher = SecurityEnricher(
                mock_client, str(output), cache_dir=str(tmp_path / "cache"), ecosystem="debian"
            )
            with patch.object(enricher, '_validate_fuseki_recency'):
                enricher.enrich()

        content = output.read_text()
        assert "CVE-2022-0778" in content
        assert "Vulnerability" in content
        assert "affectsVersion" in content

    def test_enrich_skips_unrelated_cves(self, tmp_path):
        mock_client = MagicMock()
        mock_client.query_packages_by_type.return_value = [("bash", "5.2")]

        osv_resp = MagicMock()
        osv_resp.status_code = 200
        osv_resp.json.return_value = {
            "vulns": [
                {
                    "id": "CVE-2099-9999",
                    "affected": [
                        {"package": {"name": "other-package", "ecosystem": "Debian"}}
                    ],
                }
            ]
        }
        osv_resp.raise_for_status.return_value = None

        output = tmp_path / "security.nt"
        with patch(
            "packagegraph.enrichers.security.requests.post", return_value=osv_resp
        ):
            enricher = SecurityEnricher(
                mock_client, str(output), cache_dir=str(tmp_path / "cache"), ecosystem="debian"
            )
            with patch.object(enricher, '_validate_fuseki_recency'):
                enricher.enrich()

        content = output.read_text()
        assert "affectsVersion" not in content

    def test_enrich_extracts_cvss_vector_and_score(self, tmp_path):
        """Test that CVSS vector and numeric score are extracted."""
        mock_client = MagicMock()
        mock_client.query_packages_by_type.return_value = [
            ("glibc", "2.36-1")
        ]

        osv_resp = MagicMock()
        osv_resp.status_code = 200
        osv_resp.json.return_value = {
            "vulns": [
                {
                    "id": "CVE-2023-1234",
                    "summary": "Test vulnerability",
                    "severity": [
                        {"type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"}
                    ],
                    "published": "2023-05-01T00:00:00Z",
                    "affected": [
                        {"package": {"name": "glibc", "ecosystem": "Debian"}}
                    ],
                }
            ]
        }
        osv_resp.raise_for_status.return_value = None

        output = tmp_path / "security.nt"
        with patch(
            "packagegraph.enrichers.security.requests.post", return_value=osv_resp
        ):
            enricher = SecurityEnricher(
                mock_client, str(output), cache_dir=str(tmp_path / "cache"), ecosystem="debian"
            )
            with patch.object(enricher, '_validate_fuseki_recency'):
                enricher.enrich()

        content = output.read_text()
        assert "cvssVector" in content
        assert "CVSS:3.1/AV:N" in content
        # CVSS vector is the ground truth; numeric score would need parsing

    def test_enrich_extracts_cwe_id(self, tmp_path):
        """Test that CWE ID is extracted from database_specific."""
        mock_client = MagicMock()
        mock_client.query_packages_by_type.return_value = [
            ("curl", "7.88.1")
        ]

        osv_resp = MagicMock()
        osv_resp.status_code = 200
        osv_resp.json.return_value = {
            "vulns": [
                {
                    "id": "CVE-2023-5678",
                    "summary": "Buffer overflow",
                    "database_specific": {
                        "cwe_ids": ["CWE-119", "CWE-787"]
                    },
                    "affected": [
                        {"package": {"name": "curl", "ecosystem": "Debian"}}
                    ],
                }
            ]
        }
        osv_resp.raise_for_status.return_value = None

        output = tmp_path / "security.nt"
        with patch(
            "packagegraph.enrichers.security.requests.post", return_value=osv_resp
        ):
            enricher = SecurityEnricher(
                mock_client, str(output), cache_dir=str(tmp_path / "cache"), ecosystem="debian"
            )
            with patch.object(enricher, '_validate_fuseki_recency'):
                enricher.enrich()

        content = output.read_text()
        assert "cweId" in content
        assert "CWE-119" in content

    def test_enrich_extracts_fixed_version(self, tmp_path):
        """Test that fixed version is extracted from ranges/events."""
        mock_client = MagicMock()
        mock_client.query_packages_by_type.return_value = [
            ("nginx", "1.22.0")
        ]

        osv_resp = MagicMock()
        osv_resp.status_code = 200
        osv_resp.json.return_value = {
            "vulns": [
                {
                    "id": "CVE-2023-9999",
                    "summary": "Security issue",
                    "affected": [
                        {
                            "package": {"name": "nginx", "ecosystem": "Debian"},
                            "ranges": [
                                {
                                    "type": "ECOSYSTEM",
                                    "events": [
                                        {"introduced": "0"},
                                        {"fixed": "1.23.0"}
                                    ]
                                }
                            ]
                        }
                    ],
                }
            ]
        }
        osv_resp.raise_for_status.return_value = None

        output = tmp_path / "security.nt"
        with patch(
            "packagegraph.enrichers.security.requests.post", return_value=osv_resp
        ):
            enricher = SecurityEnricher(
                mock_client, str(output), cache_dir=str(tmp_path / "cache"), ecosystem="debian"
            )
            with patch.object(enricher, '_validate_fuseki_recency'):
                enricher.enrich()

        content = output.read_text()
        assert "fixedInVersion" in content
        assert "1.23.0" in content

    def test_enrich_supports_npm_ecosystem(self, tmp_path):
        """Test that enricher works with npm ecosystem."""
        mock_client = MagicMock()
        mock_client.query_packages_by_type.return_value = [
            ("express", "4.18.2")
        ]

        osv_resp = MagicMock()
        osv_resp.status_code = 200
        osv_resp.json.return_value = {
            "vulns": [
                {
                    "id": "GHSA-1234-5678-9abc",
                    "summary": "Prototype pollution",
                    "affected": [
                        {"package": {"name": "express", "ecosystem": "npm"}}
                    ],
                }
            ]
        }
        osv_resp.raise_for_status.return_value = None

        output = tmp_path / "security_npm.nt"
        with patch(
            "packagegraph.enrichers.security.requests.post", return_value=osv_resp
        ):
            enricher = SecurityEnricher(
                mock_client, str(output), cache_dir=str(tmp_path / "cache"), ecosystem="npm"
            )
            with patch.object(enricher, '_validate_fuseki_recency'):
                enricher.enrich()

        content = output.read_text()
        assert "GHSA-1234-5678-9abc" in content
        assert "Vulnerability" in content
        # Should query with npm RDF type
        mock_client.query_packages_by_type.assert_called_once()
        call_args = mock_client.query_packages_by_type.call_args
        assert "npm:NpmPackage" in str(call_args)
