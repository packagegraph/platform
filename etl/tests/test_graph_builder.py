import pytest
from rdflib import Graph, URIRef, Literal
from rdflib.namespace import RDF, FOAF
from packagegraph.namespaces import PKG, DATA, DEB
from packagegraph.graph_builder import GraphBuilder


@pytest.mark.unit
def test_graphbuilder_initialization():
    """GraphBuilder should bind namespaces to the provided graph."""
    g = Graph()
    builder = GraphBuilder(g)

    assert builder.graph is g
    # Verify namespace bindings were added
    namespaces = dict(g.namespaces())
    assert "pkg" in namespaces
    assert "sec" in namespaces
    assert "vcs" in namespaces
    assert "data" in namespaces
    assert "foaf" in namespaces
    assert "prov" in namespaces


@pytest.mark.unit
def test_add_package():
    """add_package should create pkg:BinaryPackage with all required properties."""
    g = Graph()
    builder = GraphBuilder(g)

    package_uri = builder.add_package(
        distro="debian",
        release="bookworm",
        arch="amd64",
        name="curl",
        version="8.4.0-2",
        description="command line tool for transferring data with URL syntax",
        homepage="https://curl.se/",
        install_size=1234,
        package_size=567,
        checksum="abc123",
        suite="stable",
        component="main"
    )

    # Check the package URI format includes architecture
    expected_uri = DATA["package/debian/bookworm/amd64/curl/8.4.0-2"]
    assert package_uri == expected_uri

    # Check pkg:BinaryPackage type
    assert (package_uri, RDF.type, PKG.BinaryPackage) in g

    # Check pkg:packageName
    assert (package_uri, PKG.packageName, Literal("curl")) in g

    # Check pkg:hasVersion (should link to a pkg:Version resource)
    version_triples = list(g.triples((package_uri, PKG.hasVersion, None)))
    assert len(version_triples) == 1
    version_uri = version_triples[0][2]

    # Verify the Version resource exists
    assert (version_uri, RDF.type, PKG.Version) in g
    assert (version_uri, PKG.versionString, Literal("8.4.0-2")) in g

    # Check other properties
    assert (package_uri, PKG.description, Literal("command line tool for transferring data with URL syntax")) in g
    assert (package_uri, PKG.homepage, Literal("https://curl.se/")) in g
    assert (package_uri, PKG.installSize, Literal(1234)) in g
    assert (package_uri, PKG.packageSize, Literal(567)) in g
    assert (package_uri, PKG.checksum, Literal("abc123")) in g


@pytest.mark.unit
def test_add_package_dual_typing():
    """add_package should emit both pkg:BinaryPackage and deb:BinaryPackage types."""
    g = Graph()
    builder = GraphBuilder(g)

    package_uri = builder.add_package(
        distro="debian",
        release="bookworm",
        arch="amd64",
        name="curl",
        version="8.4.0-2",
        distro_type="deb"
    )

    # Check both types are asserted
    assert (package_uri, RDF.type, PKG.BinaryPackage) in g
    assert (package_uri, RDF.type, DEB.BinaryPackage) in g


@pytest.mark.unit
def test_add_dependency():
    """add_dependency should create pkg:directlyDependsOn link and reified Dependency."""
    g = Graph()
    builder = GraphBuilder(g)

    pkg_uri = DATA["package/debian/bookworm/amd64/curl/8.4.0-2"]
    dep_uri = DATA["package/debian/bookworm/amd64/libc6/2.36-9"]

    builder.add_dependency(
        package_uri=pkg_uri,
        target_uri=dep_uri,
        dep_type="runtime",
        distro_property=PKG.debDepends,
        constraint_op="≥",
        constraint_val="2.36"
    )

    # Check direct link
    assert (pkg_uri, PKG.directlyDependsOn, dep_uri) in g

    # Check distro-specific property
    assert (pkg_uri, PKG.debDepends, dep_uri) in g

    # Check reified Dependency
    dep_reified = list(g.triples((None, RDF.type, PKG.Dependency)))
    assert len(dep_reified) == 1
    dep_node = dep_reified[0][0]

    assert (dep_node, PKG.dependencyTarget, dep_uri) in g
    assert (dep_node, PKG.dependencyType, Literal("runtime")) in g

    # Check VersionConstraint
    constraint_triples = list(g.triples((dep_node, PKG.hasVersionConstraint, None)))
    assert len(constraint_triples) == 1
    constraint_node = constraint_triples[0][2]

    assert (constraint_node, RDF.type, PKG.VersionConstraint) in g
    assert (constraint_node, PKG.versionConstraintOperator, Literal("≥")) in g
    assert (constraint_node, PKG.versionConstraintValue, Literal("2.36")) in g


@pytest.mark.unit
def test_add_maintainer():
    """add_maintainer should create pkg:Maintainer with foaf:name and foaf:mbox."""
    g = Graph()
    builder = GraphBuilder(g)

    pkg_uri = DATA["package/debian/bookworm/amd64/curl/8.4.0-2"]

    maintainer_uri = builder.add_maintainer(
        package_uri=pkg_uri,
        name="Jane Doe",
        email="jane.doe@debian.org"
    )

    expected_maintainer_uri = DATA["maintainer/jane.doe@debian.org"]
    assert maintainer_uri == expected_maintainer_uri

    # Check Maintainer resource
    assert (maintainer_uri, RDF.type, PKG.Maintainer) in g
    assert (maintainer_uri, FOAF.name, Literal("Jane Doe")) in g
    assert (maintainer_uri, FOAF.mbox, URIRef("mailto:jane.doe@debian.org")) in g

    # Check maintainedBy link
    assert (pkg_uri, PKG.maintainedBy, maintainer_uri) in g


@pytest.mark.unit
def test_add_source_package():
    """add_source_package should create pkg:SourcePackage and link via builtFromSource."""
    g = Graph()
    builder = GraphBuilder(g)

    bin_pkg_uri = DATA["package/debian/bookworm/amd64/curl/8.4.0-2"]

    src_pkg_uri = builder.add_source_package(
        binary_package_uri=bin_pkg_uri,
        distro="debian",
        release="bookworm",
        source_name="curl",
        source_version="8.4.0-2"
    )

    expected_src_uri = DATA["source/debian/bookworm/curl/8.4.0-2"]
    assert src_pkg_uri == expected_src_uri

    # Check SourcePackage resource
    assert (src_pkg_uri, RDF.type, PKG.SourcePackage) in g
    assert (src_pkg_uri, PKG.packageName, Literal("curl")) in g

    # Check builtFromSource link
    assert (bin_pkg_uri, PKG.builtFromSource, src_pkg_uri) in g


@pytest.mark.unit
def test_uri_encoding():
    """URI builder should properly encode special characters like +."""
    g = Graph()
    builder = GraphBuilder(g)

    package_uri = builder.add_package(
        distro="debian",
        release="bookworm",
        arch="amd64",
        name="libstdc++-dev",
        version="12.2.0-14"
    )

    # The + should be URL-encoded
    assert "libstdc%2B%2B-dev" in str(package_uri)


@pytest.mark.unit
def test_graphbuilder_stateless():
    """GraphBuilder should work with multiple independent graphs."""
    g1 = Graph()
    g2 = Graph()

    builder1 = GraphBuilder(g1)
    builder2 = GraphBuilder(g2)

    pkg1 = builder1.add_package(
        distro="debian",
        release="bookworm",
        arch="amd64",
        name="curl",
        version="8.4.0-2"
    )

    pkg2 = builder2.add_package(
        distro="fedora",
        release="41",
        arch="x86_64",
        name="curl",
        version="8.6.0-5.fc41"
    )

    # Each graph should have its own package
    assert (pkg1, RDF.type, PKG.BinaryPackage) in g1
    assert (pkg2, RDF.type, PKG.BinaryPackage) in g2

    # But not in the other graph
    assert (pkg1, RDF.type, PKG.BinaryPackage) not in g2
    assert (pkg2, RDF.type, PKG.BinaryPackage) not in g1
