"""Ontology and data namespace definitions for PackageGraph.

URI Design (v0.5.0):
  Ontology: https://purl.org/packagegraph/ontology/{module}#
  Data:     https://packagegraph.github.io/d/{type}/{path...}

Data URI paths:
  d/pkg/{distro}/{release}/{arch}/{name}              ← PackageIdentity (version-agnostic)
  d/pkg/{distro}/{release}/{arch}/{name}/{version}    ← BinaryPackage (versioned)
  d/src/{distro}/{release}/{name}/{version}            ← SourcePackage
  d/ver/{distro}/{release}/{name}/{version}            ← Version
  d/distro/{name}                                      ← Distribution
  d/release/{distro}/{codename}                        ← DistributionRelease
  d/arch/{name}                                        ← Architecture
  d/maintainer/{email}                                 ← Maintainer
  d/cve/{id}                                           ← Vulnerability
  d/repo/{host/path}                                   ← VCS Repository
  d/commit/{sha12}                                     ← Commit
"""

from urllib.parse import quote
from rdflib import Namespace


# Ontology namespaces (classes + properties)
PKG = Namespace("https://purl.org/packagegraph/ontology/core#")
SEC = Namespace("https://purl.org/packagegraph/ontology/security#")
VCS = Namespace("https://purl.org/packagegraph/ontology/vcs#")
SLSA = Namespace("https://purl.org/packagegraph/ontology/slsa#")
MET = Namespace("https://purl.org/packagegraph/ontology/metrics#")

# Distribution-specific ontology extensions
DEB = Namespace("https://purl.org/packagegraph/ontology/debian#")
RPM = Namespace("https://purl.org/packagegraph/ontology/rpm#")
GEMS = Namespace("https://purl.org/packagegraph/ontology/rubygems#")
MAVEN = Namespace("https://purl.org/packagegraph/ontology/maven#")
CPAN = Namespace("https://purl.org/packagegraph/ontology/cpan#")
CRAN = Namespace("https://purl.org/packagegraph/ontology/cran#")

# Data quality
DQ = Namespace("https://purl.org/packagegraph/ontology/dq#")

# External vocabularies
FOAF = Namespace("http://xmlns.com/foaf/0.1/")
PROV = Namespace("http://www.w3.org/ns/prov#")

# Data namespace (instances) — shortened from /data/ to /d/
DATA = Namespace("https://packagegraph.github.io/d/")


def _encode(component: str) -> str:
    """URL-encode a URI path component, encoding all special characters."""
    return quote(component, safe="")


def package_identity_uri(distro: str, release: str, arch: str, name: str) -> str:
    """Build a version-agnostic PackageIdentity URI.

    Used as the target of dependency links instead of versioned URIs.
    """
    return DATA[f"pkg/{_encode(distro)}/{_encode(release)}/{_encode(arch)}/{_encode(name)}"]


def package_uri(distro: str, release: str, arch: str, name: str, version: str) -> str:
    """Build a versioned BinaryPackage URI."""
    return DATA[f"pkg/{_encode(distro)}/{_encode(release)}/{_encode(arch)}/{_encode(name)}/{_encode(version)}"]


def source_uri(distro: str, release: str, name: str, version: str) -> str:
    """Build a SourcePackage URI."""
    return DATA[f"src/{_encode(distro)}/{_encode(release)}/{_encode(name)}/{_encode(version)}"]


def version_uri(distro: str, release: str, name: str, version: str) -> str:
    """Build a Version URI."""
    return DATA[f"ver/{_encode(distro)}/{_encode(release)}/{_encode(name)}/{_encode(version)}"]


def maintainer_uri(email: str) -> str:
    """Build a Maintainer URI from email address."""
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
    cleaned = url.replace("https://", "").replace("http://", "").rstrip("/")
    return DATA[f"repo/{_encode(cleaned)}"]


def advisory_uri(advisory_id: str) -> str:
    """Build a SecurityAdvisory URI."""
    return DATA[f"advisory/{_encode(advisory_id)}"]


def build_uri(distro: str, release: str, name: str, version: str) -> str:
    """Build a BuildActivity URI."""
    return DATA[f"build/{_encode(distro)}/{_encode(release)}/{_encode(name)}/{_encode(version)}"]


def attestation_uri(distro: str, release: str, name: str, version: str) -> str:
    """Build a SLSA ProvenanceAttestation URI."""
    return DATA[f"attestation/{_encode(distro)}/{_encode(release)}/{_encode(name)}/{_encode(version)}"]


def builder_uri(builder_id: str) -> str:
    """Build a SLSA Builder URI from builder ID."""
    cleaned = builder_id.replace("https://", "").replace("http://", "").rstrip("/")
    return DATA[f"builder/{_encode(cleaned)}"]


def build_env_uri(distro: str, release: str, name: str, version: str) -> str:
    """Build a SLSA BuildEnvironment URI."""
    return DATA[f"buildenv/{_encode(distro)}/{_encode(release)}/{_encode(name)}/{_encode(version)}"]


def claim_uri(enricher: str, subject_hash: str, timestamp: str) -> str:
    """Build a claim URI for attributed enricher data.

    Format: d/claim/{enricher}/{timestamp_hash}/{subject_hash}
    """
    import hashlib
    ts_hash = hashlib.sha256(timestamp.encode()).hexdigest()[:8]
    return str(DATA[f"claim/{_encode(enricher)}/{ts_hash}/{subject_hash}"])


def snapshot_uri(enricher: str, timestamp: str) -> str:
    """Build a DataSnapshot URI for an enrichment run.

    Format: d/snapshot/{enricher}/{iso_timestamp}
    """
    # Use timestamp as-is for readability (ISO format is URI-safe)
    return str(DATA[f"snapshot/{_encode(enricher)}/{timestamp}"])


def license_uri(spdx_id: str) -> str:
    """Build a License URI from SPDX identifier."""
    return str(DATA[f"license/{_encode(spdx_id)}"])


def language_uri(language_name: str) -> str:
    """Build a ProgrammingLanguage URI."""
    return str(DATA[f"language/{_encode(language_name)}"])
