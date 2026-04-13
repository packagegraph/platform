"""Debian repository collector using ontology-aligned GraphBuilder."""

import sys
import click
import requests
import gzip
import re
import tempfile
from rdflib import Graph, Literal
from rdflib.namespace import RDF
from ..collector import BaseCollector
from ..profiler import profiler
from ..graph_builder import GraphBuilder
from ..namespaces import PKG, package_uri


class DebianCollector(BaseCollector):
    """Collects data from a Debian repository using ontology-aligned triples."""

    def __init__(
        self,
        g,
        repo_url,
        distribution,
        component,
        arch,
        parallel=True,
        chunk_size=1000,
        workers=4,
    ):
        super().__init__(g, repo_url, parallel, chunk_size, workers)
        self.distribution = distribution
        self.component = component
        # arch can be a single string or a tuple of strings
        self.arches = arch if isinstance(arch, (list, tuple)) else [arch]

    def collect(self):
        with profiler.step("Get Release Info"):
            release_info = self._get_release_info()
            if not release_info:
                click.echo(
                    f"Could not determine release information for '{self.distribution}'. Aborting.",
                    err=True,
                )
                sys.exit(1)

        codename, suite, origin = (
            release_info["Codename"],
            release_info["Suite"],
            release_info["Origin"],
        )
        click.echo(
            f"Resolved '{self.distribution}' to Origin='{origin}', Suite='{suite}', Codename='{codename}'."
        )

        with profiler.step("Add Distribution Metadata"):
            # Use GraphBuilder to add distribution metadata
            builder = GraphBuilder(self.g)
            builder.add_distribution("debian")
            builder.add_release("debian", codename, suite, origin)

            # Add all architectures
            for arch in self.arches:
                arch_name = arch.split("-")[-1] if "-" in arch else arch
                builder.add_architecture(arch_name)

        total_processed = 0

        # Process each architecture
        for arch in self.arches:
            click.echo(f"\nProcessing architecture: {arch}")

            with profiler.step(f"Download Package Metadata ({arch})"):
                packages_data = self._get_packages_data(arch)
                click.echo(f"Found {len(packages_data)} package entries for {arch}.")

            with profiler.step(f"Process Packages ({arch})"):
                # Extract arch name for URI building
                arch_name = arch.split("-")[-1] if "-" in arch else arch

                if not self.parallel or len(packages_data) < self.chunk_size:
                    # Single-threaded processing
                    processed_count = self._process_packages_single_threaded(
                        packages_data, codename, suite, arch_name
                    )
                else:
                    # Parallel processing with chunks
                    processed_count = self.collect_parallel(
                        [(pkg_data, codename, suite, arch_name) for pkg_data in packages_data],
                        DebianCollector._process_package_chunk_wrapper,
                    )

                total_processed += processed_count

            with profiler.step(f"Process Contents File ({arch})"):
                # Build package map for contents processing
                pkg_map = self._build_package_map(packages_data, codename, arch_name)
                self._process_contents_parallel(pkg_map, arch_name, arch)

        return total_processed

    def _get_release_info(self):
        """Fetches the Release file and extracts Codename, Suite, and Origin."""
        release_url = f"{self.repo_url.rstrip('/')}/dists/{self.distribution}/Release"
        click.echo(f"Fetching Release info from {release_url}")
        release_info = {}
        try:
            response = requests.get(release_url)
            response.raise_for_status()
            for line in response.text.splitlines():
                if line.startswith("Codename:"):
                    release_info["Codename"] = line.split(":", 1)[1].strip()
                elif line.startswith("Suite:"):
                    release_info["Suite"] = line.split(":", 1)[1].strip()
                elif line.startswith("Origin:"):
                    release_info["Origin"] = line.split(":", 1)[1].strip()
            if (
                "Codename" in release_info
                and "Suite" in release_info
                and "Origin" in release_info
            ):
                return release_info
        except requests.exceptions.RequestException as e:
            click.echo(f"Error: Could not fetch or parse Release file: {e}", err=True)
            sys.exit(1)
        return None

    def _get_packages_data(self, arch):
        """Download and parse package data into structured format."""
        packages_url = f"{self.repo_url.rstrip('/')}/dists/{self.distribution}/{self.component}/{arch}/Packages.gz"

        with profiler.step("Download Packages.gz"):
            click.echo(f"Downloading {packages_url}")
            response = requests.get(packages_url)
            response.raise_for_status()
            profiler.log(f"Downloaded {len(response.content)} bytes")

        with profiler.step("Decompress Packages.gz"):
            content = gzip.decompress(response.content).decode("utf-8")
            profiler.log(f"Decompressed to {len(content)} characters")

        with profiler.step("Parse Package Entries"):
            packages = content.strip().split("\n\n")
            packages_data = []

            for pkg_info in packages:
                pkg_data = {}
                for line in pkg_info.strip().split("\n"):
                    if ":" in line:
                        # Handle multi-line fields
                        if line.startswith(" "):
                            # Continuation of previous field
                            if pkg_data:
                                last_key = list(pkg_data.keys())[-1]
                                pkg_data[last_key] += " " + line.strip()
                        else:
                            k, v = line.split(":", 1)
                            pkg_data[k.strip()] = v.strip()

                if "Package" in pkg_data and "Version" in pkg_data:
                    packages_data.append(pkg_data)

            profiler.log(f"Parsed {len(packages_data)} valid packages")

        return packages_data

    def _build_package_map(self, packages_data, codename, arch_name):
        """Build package name to URI mapping for contents processing."""
        pkg_map = {}
        for pkg_data in packages_data:
            pkg_name = pkg_data["Package"]
            pkg_version = pkg_data["Version"]
            pkg_uri = package_uri("debian", codename, arch_name, pkg_name, pkg_version)
            pkg_map[pkg_name] = pkg_uri
        return pkg_map

    def _process_packages_single_threaded(self, packages_data, codename, suite, arch_name):
        """Process packages directly into main graph (single-threaded)."""
        builder = GraphBuilder(self.g)

        for pkg_data in packages_data:
            self._process_single_package(builder, pkg_data, codename, suite, arch_name)

        return len(packages_data)

    def _process_single_package(self, builder, pkg_data, codename, suite, arch_name):
        """Process a single package using GraphBuilder."""
        pkg_name = pkg_data["Package"]
        pkg_version = pkg_data["Version"]

        # Parse maintainer
        maintainer_name, maintainer_email = self._parse_maintainer(pkg_data.get("Maintainer", ""))

        # Create package
        pkg_uri = builder.add_package(
            distro="debian",
            release=codename,
            arch=arch_name,
            name=pkg_name,
            version=pkg_version,
            description=pkg_data.get("Description"),
            homepage=pkg_data.get("Homepage"),
            install_size=int(pkg_data["Installed-Size"]) * 1024 if "Installed-Size" in pkg_data else None,  # KB to bytes
            package_size=int(pkg_data["Size"]) if "Size" in pkg_data else None,
            checksum=pkg_data.get("SHA256"),
            suite=suite,
            component=self.component,
            distro_type="deb"
        )

        # Add maintainer if parsed
        if maintainer_name and maintainer_email:
            builder.add_maintainer(pkg_uri, maintainer_name, maintainer_email)

        # Parse and link source package
        self._process_source_field(builder, pkg_uri, pkg_data, codename)

        # Process dependencies
        self._process_dependencies(builder, pkg_uri, pkg_data, codename, arch_name)

    def _parse_maintainer(self, maintainer_str):
        """Parse maintainer string 'Name <email>' into components."""
        if not maintainer_str:
            return None, None

        # Match "Name <email>"
        match = re.match(r'^(.+?)\s*<(.+?)>$', maintainer_str)
        if match:
            return match.group(1).strip(), match.group(2).strip()

        # If no match, return None
        return None, None

    def _process_source_field(self, builder, binary_pkg_uri, pkg_data, codename):
        """Parse Source field and create SourcePackage link."""
        source_str = pkg_data.get("Source")
        pkg_name = pkg_data["Package"]
        pkg_version = pkg_data["Version"]

        if source_str:
            # Format can be "sourcename" or "sourcename (version)"
            match = re.match(r'^([^\s]+)(?:\s+\(([^)]+)\))?$', source_str)
            if match:
                source_name = match.group(1)
                source_version = match.group(2) if match.group(2) else pkg_version
            else:
                source_name = source_str
                source_version = pkg_version
        else:
            # No Source field means source name = binary name
            source_name = pkg_name
            source_version = pkg_version

        builder.add_source_package(
            binary_package_uri=binary_pkg_uri,
            distro="debian",
            release=codename,
            source_name=source_name,
            source_version=source_version
        )

    def _process_dependencies(self, builder, pkg_uri, pkg_data, codename, arch_name):
        """Process all dependency fields."""
        dep_mappings = {
            "Depends": ("runtime", PKG.debDepends),
            "Pre-Depends": ("runtime", PKG.debDepends),
            "Recommends": ("recommends", PKG.debRecommends),
            "Suggests": ("suggests", PKG.debSuggests),
            "Conflicts": ("conflicts", PKG.debConflicts),
            "Breaks": ("breaks", PKG.debConflicts),
            "Enhances": ("enhances", PKG.debEnhances),
            "Replaces": ("replaces", None),
            "Provides": (None, PKG.debProvides),  # Provides is handled differently
        }

        for field, (dep_type, distro_prop) in dep_mappings.items():
            if field in pkg_data:
                dependencies = self._parse_dependency_string(pkg_data[field])

                for dep_name, version_constraint in dependencies:
                    dep_uri = package_uri("debian", codename, arch_name, dep_name, "unknown")

                    if field == "Provides":
                        builder.graph.add((dep_uri, RDF.type, PKG.BinaryPackage))
                        builder.graph.add((dep_uri, PKG.packageName, Literal(dep_name)))
                        builder.graph.add((pkg_uri, PKG.directlyProvides, dep_uri))
                        if distro_prop:
                            builder.graph.add((pkg_uri, distro_prop, dep_uri))
                    else:
                        constraint_op, constraint_val = self._parse_version_constraint(version_constraint)

                        builder.add_dependency(
                            package_uri=pkg_uri,
                            target_uri=dep_uri,
                            dep_type=dep_type,
                            target_name=dep_name,
                            distro_property=distro_prop,
                            constraint_op=constraint_op,
                            constraint_val=constraint_val
                        )

    def _parse_dependency_string(self, dep_string):
        """Parse a Debian dependency string into a list of (name, constraint) tuples."""
        dep_pattern = re.compile(r"([\w.-]+)(?:\s+\(([^)]+)\))?")
        dependencies = []
        for part in dep_string.split(","):
            # Handle alternatives by taking the first one
            alternatives = [d.strip() for d in part.split("|")]
            first_alternative = alternatives[0]
            match = dep_pattern.match(first_alternative)
            if match:
                dependencies.append((match.group(1), match.group(2)))
        return dependencies

    def _parse_version_constraint(self, constraint_str):
        """Parse version constraint like '>= 2.36' into operator and value."""
        if not constraint_str:
            return None, None

        # Match operator and version
        match = re.match(r'^\s*([<>=]+)\s*(.+)$', constraint_str)
        if match:
            op_str = match.group(1)
            value = match.group(2).strip()

            # Map Debian operators to symbols
            op_map = {
                "<<": "<",
                "<=": "≤",
                "=": "=",
                ">=": "≥",
                ">>": ">"
            }
            operator = op_map.get(op_str, op_str)

            return operator, value

        return None, None

    def _process_contents_parallel(self, pkg_map, arch_name, arch):
        """Download and process contents file."""
        # Use the arch parameter to construct the correct URL
        # Contents files use just the arch name (without "binary-" prefix)
        contents_url = f"{self.repo_url.rstrip('/')}/dists/{self.distribution}/Contents-{arch_name}.gz"

        try:
            with profiler.step("Download Contents File"):
                click.echo(f"Downloading {contents_url}")
                with requests.get(contents_url, stream=True) as response:
                    response.raise_for_status()
                    profiler.log(f"Downloaded {len(response.content)} bytes")

            with profiler.step("Decompress Contents File"):
                contents_content = gzip.decompress(response.content).decode(
                    "utf-8", errors="ignore"
                )
                profiler.log(f"Decompressed to {len(contents_content)} characters")

            with profiler.step("Parse Contents Lines"):
                click.echo("Processing file lists...")
                lines = contents_content.splitlines()
                profiler.log(f"Split into {len(lines)} lines")

            with profiler.step("Process Contents"):
                # Simple single-threaded for now
                builder = GraphBuilder(self.g)
                for line in lines:
                    parts = line.strip().split()
                    if len(parts) < 2:
                        continue

                    file_path, pkg_names_str = parts[0], parts[-1]
                    for pkg_name in pkg_names_str.split(","):
                        clean_pkg_name = pkg_name.split("/")[-1]
                        if clean_pkg_name in pkg_map:
                            builder.add_installed_file(pkg_map[clean_pkg_name], file_path)

        except requests.exceptions.HTTPError as e:
            click.echo(
                f"Warning: Could not download Contents file at {contents_url}: {e}",
                err=True,
            )

    @staticmethod
    def _process_package_chunk_wrapper(data_chunk, chunk_id):
        """Wrapper for parallel processing of package chunks."""
        packages_data = [item[0] for item in data_chunk]
        codename, suite, arch_name = data_chunk[0][1], data_chunk[0][2], data_chunk[0][3]
        return DebianCollector._process_package_chunk(
            packages_data, chunk_id, codename, suite, arch_name
        )

    @staticmethod
    def _process_package_chunk(packages_chunk, chunk_id, codename, suite, arch_name):
        """Process a chunk of packages in a separate process using GraphBuilder."""
        from ..graph_builder import GraphBuilder
        import re

        chunk_graph = Graph()
        builder = GraphBuilder(chunk_graph)

        def parse_maintainer(maintainer_str):
            if not maintainer_str:
                return None, None
            match = re.match(r'^(.+?)\s*<(.+?)>$', maintainer_str)
            if match:
                return match.group(1).strip(), match.group(2).strip()
            return None, None

        def parse_source_field(pkg_data, pkg_name, pkg_version):
            source_str = pkg_data.get("Source")
            if source_str:
                match = re.match(r'^([^\s]+)(?:\s+\(([^)]+)\))?$', source_str)
                if match:
                    return match.group(1), match.group(2) if match.group(2) else pkg_version
                return source_str, pkg_version
            return pkg_name, pkg_version

        for pkg_data in packages_chunk:
            pkg_name = pkg_data["Package"]
            pkg_version = pkg_data["Version"]

            maintainer_name, maintainer_email = parse_maintainer(pkg_data.get("Maintainer", ""))

            pkg_uri = builder.add_package(
                distro="debian",
                release=codename,
                arch=arch_name,
                name=pkg_name,
                version=pkg_version,
                description=pkg_data.get("Description"),
                homepage=pkg_data.get("Homepage"),
                install_size=int(pkg_data["Installed-Size"]) * 1024 if "Installed-Size" in pkg_data else None,
                package_size=int(pkg_data["Size"]) if "Size" in pkg_data else None,
                checksum=pkg_data.get("SHA256"),
                suite=suite,
                component=pkg_data.get("Section", "").split("/")[0] if "/" in pkg_data.get("Section", "") else "main",
                distro_type="deb"
            )

            if maintainer_name and maintainer_email:
                builder.add_maintainer(pkg_uri, maintainer_name, maintainer_email)

            source_name, source_version = parse_source_field(pkg_data, pkg_name, pkg_version)
            builder.add_source_package(pkg_uri, "debian", codename, source_name, source_version)

        # Save to temp file
        temp_file = tempfile.NamedTemporaryFile(
            mode="w", suffix=f"_chunk_{chunk_id}.ttl", delete=False
        )
        chunk_graph.serialize(destination=temp_file.name, format="turtle")
        temp_file.close()
        return temp_file.name
