"""GraphBuilder - Shared ontology-aligned triple emission logic."""

from rdflib import Graph, URIRef, Literal, BNode
from rdflib.namespace import RDF, XSD
from .namespaces import (
    PKG, SEC, VCS, DATA, DEB, RPM, FOAF, PROV,
    package_uri, source_uri, version_uri, maintainer_uri,
    arch_uri, distro_uri, release_uri, upstream_uri,
    repo_uri, cve_uri, build_uri
)


class GraphBuilder:
    """Encapsulates ontology-aligned triple emission.

    Stateless per-graph: each instance operates on a single Graph,
    making it safe for parallel processing where each worker creates
    its own GraphBuilder(Graph()).
    """

    def __init__(self, graph: Graph):
        """Initialize with a graph and bind all namespace prefixes."""
        self.graph = graph

        # Bind namespace prefixes for readable Turtle serialization
        self.graph.bind("pkg", PKG)
        self.graph.bind("sec", SEC)
        self.graph.bind("vcs", VCS)
        self.graph.bind("data", DATA)
        self.graph.bind("deb", DEB)
        self.graph.bind("rpm", RPM)
        self.graph.bind("foaf", FOAF)
        self.graph.bind("prov", PROV)

    def add_distribution(self, name: str) -> URIRef:
        """Create a pkg:Distribution resource."""
        dist_uri = distro_uri(name)
        self.graph.add((dist_uri, RDF.type, PKG.Distribution))
        self.graph.add((dist_uri, PKG.distributionName, Literal(name)))
        return dist_uri

    def add_release(self, distro: str, codename: str, suite: str | None = None, origin: str | None = None) -> URIRef:
        """Create a pkg:DistributionRelease resource."""
        release_uri_ref = release_uri(distro, codename)
        self.graph.add((release_uri_ref, RDF.type, PKG.DistributionRelease))
        self.graph.add((release_uri_ref, PKG.releaseCodename, Literal(codename)))

        if suite:
            self.graph.add((release_uri_ref, PKG.releaseSuite, Literal(suite)))
        if origin:
            self.graph.add((release_uri_ref, PKG.releaseOrigin, Literal(origin)))

        # Link to distribution
        dist_uri = distro_uri(distro)
        self.graph.add((release_uri_ref, PKG.partOfDistribution, dist_uri))

        return release_uri_ref

    def add_architecture(self, name: str) -> URIRef:
        """Create a pkg:Architecture resource."""
        arch_uri_ref = arch_uri(name)
        self.graph.add((arch_uri_ref, RDF.type, PKG.Architecture))
        self.graph.add((arch_uri_ref, PKG.architectureName, Literal(name)))
        return arch_uri_ref

    def add_version(
        self,
        distro: str,
        release: str,
        name: str,
        version: str,
        epoch: str | None = None,
        release_num: str | None = None,
        revision: str | None = None
    ) -> URIRef:
        """Create a separate pkg:Version resource.

        Required because sec:affectsVersion and sec:fixedInVersion
        range on pkg:Version, not pkg:Package.
        """
        ver_uri = version_uri(distro, release, name, version)
        self.graph.add((ver_uri, RDF.type, PKG.Version))
        self.graph.add((ver_uri, PKG.versionString, Literal(version)))

        if epoch:
            self.graph.add((ver_uri, PKG.epoch, Literal(epoch)))
        if release_num:
            self.graph.add((ver_uri, PKG.release, Literal(release_num)))
        if revision:
            self.graph.add((ver_uri, PKG.revision, Literal(revision)))

        return ver_uri

    def add_package(
        self,
        distro: str,
        release: str,
        arch: str,
        name: str,
        version: str,
        description: str | None = None,
        homepage: str | None = None,
        install_size: int | None = None,
        package_size: int | None = None,
        checksum: str | None = None,
        suite: str | None = None,
        component: str | None = None,
        distro_type: str | None = None,
        epoch: str | None = None,
        release_num: str | None = None
    ) -> URIRef:
        """Create a pkg:BinaryPackage with all properties.

        Args:
            distro_type: "deb" or "rpm" for dual typing
        """
        pkg_uri = package_uri(distro, release, arch, name, version)

        # Dual typing: always emit pkg:BinaryPackage
        self.graph.add((pkg_uri, RDF.type, PKG.BinaryPackage))

        # Add distro-specific type if specified
        if distro_type == "deb":
            self.graph.add((pkg_uri, RDF.type, DEB.BinaryPackage))
        elif distro_type == "rpm":
            self.graph.add((pkg_uri, RDF.type, RPM.BinaryRPM))

        # Core properties
        self.graph.add((pkg_uri, PKG.packageName, Literal(name)))

        # Create and link Version resource
        ver_uri = self.add_version(distro, release, name, version, epoch, release_num)
        self.graph.add((pkg_uri, PKG.hasVersion, ver_uri))

        # Architecture
        arch_uri_ref = arch_uri(arch)
        self.graph.add((pkg_uri, PKG.targetArchitecture, arch_uri_ref))

        # Distribution and release
        dist_uri = distro_uri(distro)
        rel_uri = release_uri(distro, release)
        self.graph.add((pkg_uri, PKG.partOfDistribution, dist_uri))
        self.graph.add((pkg_uri, PKG.partOfRelease, rel_uri))

        # Optional properties
        if description:
            self.graph.add((pkg_uri, PKG.description, Literal(description)))
        if homepage:
            self.graph.add((pkg_uri, PKG.homepage, Literal(homepage)))
        if install_size is not None:
            self.graph.add((pkg_uri, PKG.installSize, Literal(install_size, datatype=XSD.integer)))
        if package_size is not None:
            self.graph.add((pkg_uri, PKG.packageSize, Literal(package_size, datatype=XSD.integer)))
        if checksum:
            self.graph.add((pkg_uri, PKG.checksum, Literal(checksum)))

        # Debian-specific properties
        if suite:
            self.graph.add((pkg_uri, DEB.inSuite, Literal(suite)))
        if component:
            self.graph.add((pkg_uri, DEB.inComponent, Literal(component)))

        return pkg_uri

    def add_dependency(
        self,
        package_uri: URIRef,
        target_uri: URIRef,
        dep_type: str,
        target_name: str | None = None,
        distro_property: URIRef | None = None,
        constraint_op: str | None = None,
        constraint_val: str | None = None
    ):
        """Create a dependency link with optional version constraint.

        Args:
            package_uri: The package that has the dependency
            target_uri: The package being depended on
            dep_type: "runtime", "recommends", "suggests", "conflicts", etc.
            target_name: Package name for the dependency target stub
            distro_property: Additional distro-specific property (e.g., PKG.debDepends)
            constraint_op: Version constraint operator (e.g., "≥", "=", "<")
            constraint_val: Version constraint value (e.g., "2.36")
        """
        # Ensure dependency target stub has basic properties for graph traversal.
        # Targets are version-agnostic stubs (version=unknown) — without at least
        # pkg:packageName and rdf:type, they are invisible to typed queries and
        # name-based joins, breaking transitive dependency traversal.
        if target_name:
            self.graph.add((target_uri, RDF.type, PKG.BinaryPackage))
            self.graph.add((target_uri, PKG.packageName, Literal(target_name)))

        # Emit generic property based on dep_type
        if dep_type in ["conflicts", "breaks"]:
            self.graph.add((package_uri, PKG.directlyConflictsWith, target_uri))
        else:
            self.graph.add((package_uri, PKG.directlyDependsOn, target_uri))

        # Emit distro-specific property if provided
        if distro_property:
            self.graph.add((package_uri, distro_property, target_uri))

        # Create reified Dependency
        dep_node = BNode()
        self.graph.add((dep_node, RDF.type, PKG.Dependency))
        self.graph.add((dep_node, PKG.dependencyTarget, target_uri))
        self.graph.add((dep_node, PKG.dependencyType, Literal(dep_type)))
        self.graph.add((package_uri, PKG.hasDependency, dep_node))

        # Add VersionConstraint if specified
        if constraint_op and constraint_val:
            constraint_node = BNode()
            self.graph.add((constraint_node, RDF.type, PKG.VersionConstraint))
            self.graph.add((constraint_node, PKG.versionConstraintOperator, Literal(constraint_op)))
            self.graph.add((constraint_node, PKG.versionConstraintValue, Literal(constraint_val)))
            self.graph.add((dep_node, PKG.hasVersionConstraint, constraint_node))

    def add_maintainer(
        self,
        package_uri: URIRef,
        name: str,
        email: str
    ) -> URIRef:
        """Create a pkg:Maintainer and link via maintainedBy."""
        maint_uri = maintainer_uri(email)

        self.graph.add((maint_uri, RDF.type, PKG.Maintainer))
        self.graph.add((maint_uri, FOAF.name, Literal(name)))
        self.graph.add((maint_uri, FOAF.mbox, URIRef(f"mailto:{email}")))

        self.graph.add((package_uri, PKG.maintainedBy, maint_uri))

        return maint_uri

    def add_source_package(
        self,
        binary_package_uri: URIRef,
        distro: str,
        release: str,
        source_name: str,
        source_version: str
    ) -> URIRef:
        """Create a pkg:SourcePackage and link via builtFromSource."""
        src_uri = source_uri(distro, release, source_name, source_version)

        self.graph.add((src_uri, RDF.type, PKG.SourcePackage))
        self.graph.add((src_uri, PKG.packageName, Literal(source_name)))

        # Create and link Version resource for source package
        ver_uri = self.add_version(distro, release, source_name, source_version)
        self.graph.add((src_uri, PKG.hasVersion, ver_uri))

        # Link binary to source
        self.graph.add((binary_package_uri, PKG.builtFromSource, src_uri))

        return src_uri

    def add_installed_file(
        self,
        package_uri: URIRef,
        file_path: str
    ):
        """Create a pkg:InstalledFile and link via installsFile."""
        file_node = BNode()
        self.graph.add((file_node, RDF.type, PKG.InstalledFile))
        self.graph.add((file_node, PKG.installedFilePath, Literal(file_path)))
        self.graph.add((package_uri, PKG.installsFile, file_node))

    # --- VCS methods (Task 7) ---

    def add_repository(
        self,
        url: str,
        default_branch: str | None = None,
        description: str | None = None,
        stars: int | None = None,
        forks: int | None = None
    ) -> URIRef:
        """Create a vcs:Repository resource."""
        r_uri = repo_uri(url)
        self.graph.add((r_uri, RDF.type, VCS.Repository))
        self.graph.add((r_uri, VCS.repositoryURL, URIRef(url)))

        if default_branch:
            self.graph.add((r_uri, VCS.defaultBranch, Literal(default_branch)))
        if description:
            self.graph.add((r_uri, VCS.repositoryDescription, Literal(description)))
        if stars is not None:
            self.graph.add((r_uri, VCS.starCount, Literal(stars, datatype=XSD.integer)))
        if forks is not None:
            self.graph.add((r_uri, VCS.forkCount, Literal(forks, datatype=XSD.integer)))

        return r_uri

    def add_commit(
        self,
        repo_uri_ref: URIRef,
        sha: str,
        author_name: str | None = None,
        author_email: str | None = None,
        timestamp: str | None = None,
        message: str | None = None
    ) -> URIRef:
        """Create a vcs:Commit resource and link to repository."""
        commit_uri = DATA[f"commit/{sha[:12]}"]
        self.graph.add((commit_uri, RDF.type, VCS.Commit))
        self.graph.add((commit_uri, VCS.commitHash, Literal(sha)))
        self.graph.add((repo_uri_ref, VCS.hasCommit, commit_uri))

        if timestamp:
            self.graph.add((commit_uri, VCS.commitDate, Literal(timestamp)))
        if message:
            # Truncate long commit messages
            self.graph.add((commit_uri, VCS.commitMessage, Literal(message[:500])))

        if author_name and author_email:
            maint_uri = maintainer_uri(author_email)
            self.graph.add((commit_uri, VCS.authoredBy, maint_uri))
            # Ensure maintainer resource exists
            self.graph.add((maint_uri, RDF.type, PKG.Maintainer))
            self.graph.add((maint_uri, FOAF.name, Literal(author_name)))
            self.graph.add((maint_uri, FOAF.mbox, URIRef(f"mailto:{author_email}")))

        return commit_uri

    def link_upstream(
        self,
        source_package_uri: URIRef,
        project_name: str,
        repository_uri: URIRef
    ):
        """Link SourcePackage → UpstreamProject → Repository.

        The domain of pkg:hasUpstreamProject is pkg:SourcePackage,
        so this must be called with a SourcePackage URI, not BinaryPackage.
        """
        up_uri = upstream_uri(project_name)
        self.graph.add((up_uri, RDF.type, PKG.UpstreamProject))
        self.graph.add((up_uri, PKG.projectName, Literal(project_name)))
        self.graph.add((source_package_uri, PKG.hasUpstreamProject, up_uri))
        self.graph.add((up_uri, VCS.hasUpstreamRepository, repository_uri))

    def add_cross_distro_mapping(self, pkg1_uri: URIRef, pkg2_uri: URIRef):
        """Add symmetric pkg:equivalentInDistribution links."""
        self.graph.add((pkg1_uri, PKG.equivalentInDistribution, pkg2_uri))
        self.graph.add((pkg2_uri, PKG.equivalentInDistribution, pkg1_uri))

    # --- Security methods (Task 8) ---

    def add_vulnerability(
        self,
        cve_id: str,
        description: str | None = None,
        severity: str | None = None,
        published: str | None = None,
        modified: str | None = None
    ) -> URIRef:
        """Create a sec:Vulnerability resource."""
        vuln_uri = cve_uri(cve_id)
        self.graph.add((vuln_uri, RDF.type, SEC.Vulnerability))
        self.graph.add((vuln_uri, SEC.cveId, Literal(cve_id)))

        if description:
            self.graph.add((vuln_uri, SEC.vulnerabilityDescription, Literal(description[:1000])))
        if severity:
            self.graph.add((vuln_uri, SEC.severity, Literal(severity)))
        if published:
            self.graph.add((vuln_uri, SEC.publishedDate, Literal(published)))
        if modified:
            self.graph.add((vuln_uri, SEC.modifiedDate, Literal(modified)))

        return vuln_uri

    def link_vulnerability_to_version(
        self,
        vuln_uri: URIRef,
        version_uri_ref: URIRef,
        relation: str = "affects"
    ):
        """Link vulnerability to version (affects or fixed).

        Args:
            relation: "affects" for sec:affectsVersion, "fixed" for sec:fixedInVersion
        """
        if relation == "fixed":
            self.graph.add((vuln_uri, SEC.fixedInVersion, version_uri_ref))
        else:
            self.graph.add((vuln_uri, SEC.affectsVersion, version_uri_ref))

    # --- Build methods (Task 9) ---

    def add_build_activity(
        self,
        distro: str,
        release: str,
        name: str,
        version: str,
        owner: str | None = None,
        start_time: str | None = None,
        end_time: str | None = None,
        build_system: str | None = None
    ) -> URIRef:
        """Create a pkg:BuildActivity (subclass of prov:Activity)."""
        b_uri = build_uri(distro, release, name, version)
        self.graph.add((b_uri, RDF.type, PKG.BuildActivity))
        self.graph.add((b_uri, RDF.type, PROV.Activity))

        if owner:
            self.graph.add((b_uri, PKG.wasBuiltBy, Literal(owner)))
        if start_time:
            self.graph.add((b_uri, PROV.startedAtTime, Literal(start_time)))
        if end_time:
            self.graph.add((b_uri, PROV.endedAtTime, Literal(end_time)))
        if build_system:
            self.graph.add((b_uri, PKG.buildSystem, Literal(build_system)))

        return b_uri

    def link_build_to_package(self, build_uri_ref: URIRef, package_uri_ref: URIRef):
        """Link a BuildActivity to the package it produced."""
        self.graph.add((package_uri_ref, PKG.wasProducedBy, build_uri_ref))

    def link_build_dependency(self, build_uri_ref: URIRef, dep_package_uri: URIRef):
        """Record that a build used a specific dependency package."""
        self.graph.add((build_uri_ref, PKG.usedDependency, dep_package_uri))
