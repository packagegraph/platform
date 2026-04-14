from unittest.mock import patch, MagicMock
import pytest
from packagegraph.enrichers.security import SecurityEnricher


@pytest.mark.unit
class TestSecurityEnricher:
    def test_enrich_writes_ntriples_for_vulnerable_package(self, tmp_path):
        mock_client = MagicMock()
        mock_client.query_package_names_and_versions.return_value = [
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
                mock_client, str(output), cache_dir=str(tmp_path / "cache")
            )
            enricher.enrich()

        content = output.read_text()
        assert "CVE-2022-0778" in content
        assert "Vulnerability" in content
        assert "affectsVersion" in content

    def test_enrich_skips_unrelated_cves(self, tmp_path):
        mock_client = MagicMock()
        mock_client.query_package_names_and_versions.return_value = [("bash", "5.2")]

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
                mock_client, str(output), cache_dir=str(tmp_path / "cache")
            )
            enricher.enrich()

        content = output.read_text()
        assert "affectsVersion" not in content
