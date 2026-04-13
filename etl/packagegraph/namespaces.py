"""Ontology and data namespace definitions for PackageGraph."""

from urllib.parse import quote
from rdflib import Namespace


# Ontology namespaces (classes + properties)
PKG = Namespace("https://packagegraph.github.io/ontology/core#")
SEC = Namespace("https://packagegraph.github.io/ontology/security#")
VCS = Namespace("https://packagegraph.github.io/ontology/vcs#")

# Distribution-specific ontology extensions
DEB = Namespace("https://packagegraph.github.io/ontology/debian#")
RPM = Namespace("https://packagegraph.github.io/ontology/rpm#")

# External vocabularies
FOAF = Namespace("http://xmlns.com/foaf/0.1/")
PROV = Namespace("http://www.w3.org/ns/prov#")

# Data namespace (instances)
DATA = Namespace("https://packagegraph.github.io/data/")


def _encode(component: str) -> str:
    """URL-encode a URI path component, encoding all special characters."""
    return quote(component, safe="")


def package_uri(distro: str, release: str, arch: str, name: str, version: str) -> str:
    """Build a BinaryPackage URI. Architecture is required."""
    return DATA[f"package/{_encode(distro)}/{_encode(release)}/{_encode(arch)}/{_encode(name)}/{_encode(version)}"]


def source_uri(distro: str, release: str, name: str, version: str) -> str:
    """Build a SourcePackage URI."""
    return DATA[f"source/{_encode(distro)}/{_encode(release)}/{_encode(name)}/{_encode(version)}"]


def version_uri(distro: str, release: str, name: str, version: str) -> str:
    """Build a Version URI."""
    return DATA[f"version/{_encode(distro)}/{_encode(release)}/{_encode(name)}/{_encode(version)}"]


def maintainer_uri(email: str) -> str:
    """Build a Maintainer URI from email address.

    Email addresses are used as-is since @ and . are valid in URIs.
    """
    return DATA[f"maintainer/{email}"]


def arch_uri(name: str) -> str:
    """Build an Architecture URI."""
    return DATA[f"arch/{_encode(name)}"]


def distro_uri(name: str) -> str:
    """Build a Distribution URI."""
    return DATA[f"distro/{_encode(name)}"]


def release_uri(distro: str, codename: str) -> str:
    """Build a DistributionRelease URI."""
    return DATA[f"release/{_encode(distro)}/{_encode(codename)}"]


def upstream_uri(name: str) -> str:
    """Build an UpstreamProject URI."""
    return DATA[f"upstream/{_encode(name)}"]


def cve_uri(cve_id: str) -> str:
    """Build a Vulnerability URI from CVE ID."""
    return DATA[f"cve/{_encode(cve_id)}"]


def repo_uri(url: str) -> str:
    """Build a VCS Repository URI from repository URL."""
    # Strip protocol and trailing slashes for URI
    cleaned = url.replace("https://", "").replace("http://", "").rstrip("/")
    return DATA[f"repo/{_encode(cleaned)}"]


def advisory_uri(advisory_id: str) -> str:
    """Build a SecurityAdvisory URI."""
    return DATA[f"advisory/{_encode(advisory_id)}"]


def build_uri(distro: str, release: str, name: str, version: str) -> str:
    """Build a BuildActivity URI."""
    return DATA[f"build/{_encode(distro)}/{_encode(release)}/{_encode(name)}/{_encode(version)}"]
