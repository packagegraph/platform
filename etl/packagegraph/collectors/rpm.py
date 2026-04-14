"""RPM repository collector using ontology-aligned GraphBuilder."""

import click
import requests
import gzip
import xml.etree.ElementTree as ET
from rdflib import Literal

try:
    import zstandard as zstd
except ImportError:
    zstd = None

from ..collector import BaseCollector
from ..profiler import profiler
from ..graph_builder import GraphBuilder
from ..namespaces import RPM


class RpmCollector(BaseCollector):
    """Collects data from an RPM repository using ontology-aligned triples."""

    def __init__(
        self,
        g,
        repo_url,
        distro_name="fedora",
        release_name="",
        parallel=True,
        chunk_size=1000,
        workers=4,
    ):
        super().__init__(g, repo_url, parallel, chunk_size, workers)
        self.distro_name = distro_name
        self.release_name = release_name

    def collect(self):
        with profiler.step("Download Primary Metadata"):
            packages_data = self._get_primary_packages_data()
            click.echo(f"Found {len(packages_data)} package entries.")

        with profiler.step("Process Packages"):
            builder = GraphBuilder(self.g)
            # Add distribution and release metadata
            builder.add_distribution(self.distro_name)
            if self.release_name:
                builder.add_release(self.distro_name, self.release_name)

            processed_count = 0
            for pkg_data in packages_data:
                self._process_single_package(builder, pkg_data)
                processed_count += 1

        return processed_count

    def _get_metadata_url(self, metadata_type):
        """Finds the metadata URL for a given type from repomd.xml."""
        repomd_url = f"{self.repo_url.rstrip('/')}/repodata/repomd.xml"
        click.echo(f"Fetching repomd.xml from {repomd_url}")
        response = requests.get(repomd_url)
        response.raise_for_status()

        root = ET.fromstring(response.content)
        ns = {"repo": "http://linux.duke.edu/metadata/repo"}

        location = root.find(f"repo:data[@type='{metadata_type}']/repo:location", ns)
        if location is not None:
            return f"{self.repo_url.rstrip('/')}/{location.get('href')}"
        return None

    def _download_and_decompress(self, url):
        """Download and decompress metadata file."""
        click.echo(f"Downloading metadata from {url}")
        response = requests.get(url, stream=True)
        response.raise_for_status()
        if url.endswith(".gz"):
            return gzip.decompress(response.content)
        elif url.endswith(".zst"):
            if not zstd:
                raise RuntimeError("zstandard library required for .zst")
            dctx = zstd.ZstdDecompressor()
            return dctx.decompress(response.content)
        return response.content

    def _get_primary_packages_data(self):
        """Download and parse primary metadata into structured format."""
        primary_url = self._get_metadata_url("primary")
        if not primary_url:
            raise RuntimeError("Could not find primary metadata")

        content = self._download_and_decompress(primary_url)

        root = ET.fromstring(content)
        ns = {
            "common": "http://linux.duke.edu/metadata/common",
            "rpm": "http://linux.duke.edu/metadata/rpm",
        }

        packages_data = []
        for pkg in root.findall("common:package", ns):
            pkg_data = {
                "name": pkg.find("common:name", ns).text,
                "arch": pkg.find("common:arch", ns).text,
                "checksum": pkg.find("common:checksum", ns).text
                if pkg.find("common:checksum", ns) is not None
                else "",
            }

            version_info = pkg.find("common:version", ns)
            if version_info is not None:
                pkg_data["epoch"] = version_info.get("epoch", "0")
                pkg_data["ver"] = version_info.get("ver", "")
                pkg_data["rel"] = version_info.get("rel", "")

            summary = pkg.find("common:summary", ns)
            if summary is not None:
                pkg_data["summary"] = summary.text

            description = pkg.find("common:description", ns)
            if description is not None:
                pkg_data["description"] = description.text

            # RPM-specific properties from format element
            format_elem = pkg.find("common:format", ns)
            if format_elem is not None:
                license_elem = format_elem.find("rpm:license", ns)
                if license_elem is not None:
                    pkg_data["license"] = license_elem.text

                sourcerpm_elem = format_elem.find("rpm:sourcerpm", ns)
                if sourcerpm_elem is not None:
                    pkg_data["sourcerpm"] = sourcerpm_elem.text

                group_elem = format_elem.find("rpm:group", ns)
                if group_elem is not None:
                    pkg_data["group"] = group_elem.text

            packages_data.append(pkg_data)

        return packages_data

    def _process_single_package(self, builder, pkg_data):
        """Process a single RPM package using GraphBuilder."""
        name = pkg_data["name"]
        arch = pkg_data["arch"]
        ver = pkg_data["ver"]
        rel = pkg_data["rel"]
        epoch = pkg_data.get("epoch", "0")

        # Build version string (epoch:version-release.arch for RPM)
        version_str = f"{ver}-{rel}.{arch}"

        # Create package using GraphBuilder
        pkg_uri = builder.add_package(
            distro=self.distro_name,
            release=self.release_name or "unknown",
            arch=arch,
            name=name,
            version=version_str,
            description=pkg_data.get("description") or pkg_data.get("summary"),
            distro_type="rpm",
            epoch=epoch,
            release_num=rel,
        )

        # Add RPM-specific properties
        if "sourcerpm" in pkg_data:
            builder.graph.add((pkg_uri, RPM.sourceRPM, Literal(pkg_data["sourcerpm"])))

        if "group" in pkg_data:
            builder.graph.add((pkg_uri, RPM.RPMGroup, Literal(pkg_data["group"])))

        # Store epoch as RPM-specific property too
        builder.graph.add((pkg_uri, RPM.epoch, Literal(int(epoch))))
