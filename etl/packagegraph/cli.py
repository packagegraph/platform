import sys
from pathlib import Path
import click
from rdflib import Graph

from packagegraph.debian_collector import DebianCollector
from packagegraph.rpm_collector import RpmCollector
from packagegraph.profiler import profiler


@click.group()
def cli():
    """A CLI for managing and inspecting the package graph ontology."""
    pass


@cli.command()
@click.argument("repo_url", required=False)
@click.option(
    "--repo-type",
    type=click.Choice(["debian", "rpm"], case_sensitive=False),
    default="debian",
    help="Type of the repository.",
)
@click.option(
    "--distribution",
    "-d",
    default="stable",
    help="[Debian] The distribution to collect (e.g., stable, bookworm).",
)
@click.option(
    "--component",
    default="main",
    help="[Debian] The component to collect (e.g., main).",
)
@click.option(
    "--arch",
    multiple=True,
    default=["binary-amd64"],
    help="[Debian] The architecture(s) to collect. Can be specified multiple times.",
)
@click.option(
    "--distro-name",
    default="fedora",
    help="[RPM] Distribution name (e.g., fedora, centos).",
)
@click.option(
    "--release-name", default="", help="[RPM] Release name/version (e.g., 41, 42)."
)
@click.option(
    "--rpm-repo",
    multiple=True,
    help="[RPM Multi-Release] Repo in format 'name:release:url'. Can be specified multiple times.",
)
@click.option("--output-file", "-o", help="Path to save the graph to.")
@click.option("--input-file", "-i", help="Path to an existing graph to load.")
@click.option(
    "--parallel/--no-parallel", default=True, help="Enable parallel processing."
)
@click.option(
    "--chunk-size",
    default=1000,
    help="Number of packages per chunk for parallel processing.",
)
@click.option("--workers", default=4, help="Number of worker processes.")
@click.option(
    "--profile/--no-profile", default=False, help="Enable detailed timing profiling."
)
def collect(
    repo_url,
    repo_type,
    distribution,
    component,
    arch,
    distro_name,
    release_name,
    rpm_repo,
    output_file,
    input_file,
    parallel,
    chunk_size,
    workers,
    profile,
):
    """Downloads package information from a repository and creates a linked data graph."""

    # Enable profiling if requested
    profiler.enabled = profile

    # Validate arguments
    if repo_type == "rpm" and rpm_repo:
        # Multi-repo RPM mode - repo_url is ignored
        if repo_url:
            click.echo(
                "Warning: repo_url argument ignored when using --rpm-repo", err=True
            )
    elif not repo_url:
        click.echo("Error: repo_url is required unless using --rpm-repo", err=True)
        sys.exit(1)

    with profiler.step("Total Collection Time"):
        g = Graph()

        if input_file and Path(input_file).exists():
            with profiler.step("Load Existing Graph"):
                click.echo(f"Loading existing graph from {input_file}")
                g.parse(input_file)
                click.echo(f"Loaded {len(g)} triples.")

        try:
            total_parsed = 0

            if repo_type == "debian":
                with profiler.step("Initialize Debian Collector"):
                    collector = DebianCollector(
                        g,
                        repo_url,
                        distribution,
                        component,
                        arch,
                        parallel,
                        chunk_size,
                        workers,
                    )

                with profiler.step("Collect Debian Package Data"):
                    parsed_count = collector.collect()
                    total_parsed += parsed_count

            elif repo_type == "rpm":
                if rpm_repo:
                    # Multi-repo RPM collection
                    for repo_spec in rpm_repo:
                        parts = repo_spec.split(":", 2)
                        if len(parts) != 3:
                            click.echo(
                                f"Error: Invalid --rpm-repo format: {repo_spec}. "
                                f"Expected 'name:release:url'",
                                err=True,
                            )
                            sys.exit(1)

                        rpm_distro, rpm_release, rpm_url = parts

                        with profiler.step(f"Collect RPM {rpm_distro}/{rpm_release}"):
                            collector = RpmCollector(
                                g,
                                rpm_url,
                                distro_name=rpm_distro,
                                release_name=rpm_release,
                                parallel=parallel,
                                chunk_size=chunk_size,
                                workers=workers,
                            )
                            parsed_count = collector.collect()
                            total_parsed += parsed_count
                            click.echo(
                                f"Processed {parsed_count} packages from {rpm_distro}/{rpm_release}."
                            )
                else:
                    # Single-repo RPM collection
                    with profiler.step("Initialize RPM Collector"):
                        collector = RpmCollector(
                            g,
                            repo_url,
                            distro_name=distro_name,
                            release_name=release_name,
                            parallel=parallel,
                            chunk_size=chunk_size,
                            workers=workers,
                        )

                    with profiler.step("Collect RPM Package Data"):
                        parsed_count = collector.collect()
                        total_parsed += parsed_count

            click.echo(f"Successfully processed {total_parsed} packages.")
            click.echo(f"Graph now contains {len(g)} triples.")

            if output_file:
                with profiler.step("Serialize Graph"):
                    click.echo(f"Serializing graph to {output_file}")
                    g.serialize(destination=output_file, format="turtle")
                    click.echo("Graph saved.")

        except Exception as e:
            click.echo(f"An unexpected error occurred: {e}", err=True)
            sys.exit(1)

    # Print profiling summary
    profiler.print_summary()


@cli.command()
@click.option(
    "--input-dir",
    required=True,
    envvar="INPUT_DIR",
    type=click.Path(exists=True),
    help="Directory containing .nt/.ttl data files.",
)
@click.option(
    "--ontology-dir",
    required=True,
    envvar="ONTOLOGY_DIR",
    type=click.Path(exists=True),
    help="Directory containing ontology .ttl files.",
)
@click.option(
    "--output-dir",
    required=True,
    envvar="OUTPUT_DIR",
    type=click.Path(),
    help="Directory for TDB2 output and tar archive.",
)
@click.option(
    "--jena-home",
    default="/opt/jena",
    envvar="JENA_HOME",
    help="Path to Apache Jena installation.",
)
@click.option("--minio-endpoint", envvar="MINIO_ENDPOINT", help="Minio endpoint URL.")
@click.option(
    "--minio-bucket",
    default="packagegraph",
    envvar="MINIO_BUCKET",
    help="Minio bucket name.",
)
@click.option("--minio-access-key", envvar="MINIO_ACCESS_KEY", help="Minio access key.")
@click.option("--minio-secret-key", envvar="MINIO_SECRET_KEY", help="Minio secret key.")
def build(
    input_dir,
    ontology_dir,
    output_dir,
    jena_home,
    minio_endpoint,
    minio_bucket,
    minio_access_key,
    minio_secret_key,
):
    """Build a TDB2 index from RDF files and optionally upload to Minio."""
    from packagegraph.minio import MinioStore
    from packagegraph.tdb import TDB2Builder

    input_dir = Path(input_dir)
    ontology_dir = Path(ontology_dir)
    output_dir = Path(output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)

    # Gather input files (data and ontology separately)
    data_files = sorted(input_dir.glob("*.nt")) + sorted(input_dir.glob("*.ttl"))
    ontology_files = sorted(ontology_dir.glob("*.nt")) + sorted(
        ontology_dir.glob("*.ttl")
    )

    if not data_files and not ontology_files:
        click.echo("No .nt or .ttl files found in input-dir or ontology-dir.", err=True)
        sys.exit(1)

    click.echo(
        f"Found {len(data_files)} data files, {len(ontology_files)} ontology files."
    )

    # Build TDB2 index (ontology in named graph, data in default graph)
    tdb_dir = output_dir / "tdb2"
    tdb_dir.mkdir(parents=True, exist_ok=True)

    builder = TDB2Builder(jena_home=jena_home)
    try:
        click.echo("Building TDB2 index...")
        builder.build(data_files, tdb_dir, ontology_files=ontology_files)
        click.echo("TDB2 index built successfully.")
    except RuntimeError as e:
        click.echo(f"TDB2 build failed: {e}", err=True)
        sys.exit(1)

    # Package TDB2
    tar_path = output_dir / "tdb2.tar.gz"
    click.echo(f"Packaging TDB2 index to {tar_path}...")
    builder.package(tdb_dir, tar_path)
    click.echo("Packaging complete.")

    # Upload to Minio if endpoint is configured
    if minio_endpoint:
        click.echo("Uploading to Minio...")
        store = MinioStore(
            endpoint=minio_endpoint,
            bucket=minio_bucket,
            access_key=minio_access_key or "",
            secret_key=minio_secret_key or "",
        )
        content_hash = store.upload_snapshot(tar_path)
        click.echo(f"Uploaded with content hash: {content_hash}")
    else:
        click.echo("No Minio endpoint configured, skipping upload.")


@cli.command()
@click.option(
    "--input-file",
    "-i",
    required=True,
    type=click.Path(exists=True),
    help="Path to existing package graph (.ttl file).",
)
@click.option(
    "--output-file",
    "-o",
    required=True,
    type=click.Path(),
    help="Path to save enriched graph.",
)
@click.option(
    "--cache-dir",
    default=None,
    help="Directory for caching repology API responses (default: ~/.cache/packagegraph/repology).",
)
def enrich_repology(input_file, output_file, cache_dir):
    """Enrich package graph with cross-distribution equivalences from repology.org."""
    from packagegraph.collectors.repology import RepologyEnricher
    from pathlib import Path

    if cache_dir is None:
        cache_dir = Path.home() / ".cache" / "packagegraph" / "repology"

    click.echo(f"Loading graph from {input_file}...")
    g = Graph()
    g.parse(input_file)
    click.echo(f"Loaded {len(g)} triples.")

    enricher = RepologyEnricher(g, cache_dir=str(cache_dir))
    enricher.enrich()

    click.echo(f"Serializing enriched graph to {output_file}...")
    g.serialize(destination=output_file, format="turtle")
    click.echo(f"Enriched graph saved. Total triples: {len(g)}")


@cli.command()
@click.option(
    "--input-file",
    "-i",
    required=True,
    type=click.Path(exists=True),
    help="Path to existing package graph (.ttl file).",
)
@click.option(
    "--output-file",
    "-o",
    required=True,
    type=click.Path(),
    help="Path to save enriched graph.",
)
@click.option(
    "--github-token",
    envvar="GITHUB_TOKEN",
    default=None,
    help="GitHub API token (or set GITHUB_TOKEN env var).",
)
@click.option(
    "--cache-dir", default=None, help="Directory for caching GitHub API responses."
)
def enrich_github(input_file, output_file, github_token, cache_dir):
    """Enrich package graph with GitHub VCS metadata."""
    from packagegraph.collectors.github import GitHubEnricher

    if not github_token:
        click.echo(
            "Warning: No GITHUB_TOKEN set. API rate limit will be 60 req/hr.", err=True
        )

    if cache_dir is None:
        cache_dir = Path.home() / ".cache" / "packagegraph" / "github"

    g = Graph()
    click.echo(f"Loading graph from {input_file}...")
    g.parse(input_file)
    click.echo(f"Loaded {len(g)} triples.")

    enricher = GitHubEnricher(g, github_token=github_token, cache_dir=str(cache_dir))
    enricher.enrich()

    g.serialize(destination=output_file, format="turtle")
    click.echo(f"Enriched graph saved to {output_file}. Total triples: {len(g)}")


@cli.command()
@click.option(
    "--fuseki-endpoint",
    required=True,
    envvar="FUSEKI_ENDPOINT",
    help="Fuseki SPARQL endpoint URL (e.g., http://fuseki:3030/packagegraph).",
)
@click.option(
    "--output-dir",
    required=True,
    type=click.Path(),
    help="Directory for enrichment output (.nt files).",
)
@click.option(
    "--cache-dir",
    default=None,
    help="Directory for caching OSV API responses.",
)
@click.option(
    "--ecosystem",
    required=True,
    type=click.Choice(["debian", "alpine", "npm", "pypi", "cargo", "gomod"], case_sensitive=False),
    help="Ecosystem to enrich (debian, alpine, npm, pypi, cargo, gomod).",
)
def enrich_security(fuseki_endpoint, output_dir, cache_dir, ecosystem):
    """Enrich package graph with vulnerability data from OSV.dev (Fuseki-aware)."""
    from packagegraph.sparql_client import SparqlQueryClient
    from packagegraph.enrichers.security import SecurityEnricher

    if cache_dir is None:
        cache_dir = Path.home() / ".cache" / "packagegraph" / "security" / ecosystem

    output_dir = Path(output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    output_file = output_dir / f"security_{ecosystem}.nt"

    client = SparqlQueryClient(fuseki_endpoint)

    enricher = SecurityEnricher(
        sparql_client=client,
        output_path=str(output_file),
        cache_dir=str(cache_dir),
        ecosystem=ecosystem,
    )
    enricher.enrich()
    click.echo(f"Security enrichment ({ecosystem}) complete. Output: {output_file}")


@cli.command()
@click.option(
    "--input-file",
    "-i",
    required=True,
    type=click.Path(exists=True),
    help="Path to existing package graph (.ttl file).",
)
@click.option(
    "--output-file",
    "-o",
    required=True,
    type=click.Path(),
    help="Path to save enriched graph.",
)
@click.option(
    "--koji-hub",
    default="https://koji.fedoraproject.org/kojihub",
    help="Koji hub XML-RPC endpoint.",
)
@click.option("--distro-name", default="fedora", help="Distribution name.")
@click.option("--release-name", default="", help="Release name/version.")
@click.option(
    "--cache-dir", default=None, help="Directory for caching koji API responses."
)
def enrich_koji(
    input_file, output_file, koji_hub, distro_name, release_name, cache_dir
):
    """Enrich RPM package graph with build metadata from Koji."""
    from packagegraph.collectors.koji import KojiEnricher

    if cache_dir is None:
        cache_dir = Path.home() / ".cache" / "packagegraph" / "koji"

    g = Graph()
    click.echo(f"Loading graph from {input_file}...")
    g.parse(input_file)
    click.echo(f"Loaded {len(g)} triples.")

    enricher = KojiEnricher(
        g,
        koji_hub=koji_hub,
        distro_name=distro_name,
        release_name=release_name,
        cache_dir=str(cache_dir),
    )
    enricher.enrich()

    g.serialize(destination=output_file, format="turtle")
    click.echo(f"Enriched graph saved to {output_file}. Total triples: {len(g)}")


@cli.command()
@click.option(
    "--fuseki-endpoint",
    required=True,
    envvar="FUSEKI_ENDPOINT",
    help="Fuseki SPARQL endpoint URL.",
)
@click.option(
    "--output-dir",
    required=True,
    type=click.Path(),
    help="Directory for enrichment output (.nt files).",
)
@click.option(
    "--cache-dir",
    default=None,
    help="Directory for caching GitHub API responses.",
)
@click.option("--github-token", envvar="GITHUB_TOKEN", help="GitHub API token.")
@click.option("--minio-endpoint", envvar="MINIO_ENDPOINT", help="Minio endpoint URL.")
@click.option("--minio-bucket", default="packagegraph", envvar="MINIO_BUCKET", help="Minio bucket.")
@click.option("--minio-access-key", envvar="MINIO_ACCESS_KEY", help="Minio access key.")
@click.option("--minio-secret-key", envvar="MINIO_SECRET_KEY", help="Minio secret key.")
def enrich_github_vcs(
    fuseki_endpoint,
    output_dir,
    cache_dir,
    github_token,
    minio_endpoint,
    minio_bucket,
    minio_access_key,
    minio_secret_key,
):
    """Enrich package graph with GitHub VCS metadata (Fuseki-aware).

    Queries Fuseki for packages with GitHub homepages, fetches repo metadata
    from the GitHub API, and writes VCS triples. Also populates the shared
    GitHub cache used by enrich-license, enrich-metrics, and enrich-vcs-activity.
    """
    from packagegraph.sparql_client import SparqlQueryClient
    from packagegraph.enrichers.github import GitHubEnricher

    if cache_dir is None:
        cache_dir = Path.home() / ".cache" / "packagegraph" / "github"

    output_dir = Path(output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    output_file = output_dir / "github_vcs.nt"

    client = SparqlQueryClient(fuseki_endpoint)

    enricher = GitHubEnricher(
        sparql_client=client,
        output_path=str(output_file),
        github_token=github_token,
        cache_dir=str(cache_dir),
        minio_endpoint=minio_endpoint,
        minio_bucket=minio_bucket,
        minio_access_key=minio_access_key,
        minio_secret_key=minio_secret_key,
    )
    enricher.enrich()
    click.echo(f"GitHub VCS enrichment complete. Output: {output_file}")


@cli.command()
@click.option(
    "--fuseki-endpoint",
    required=True,
    envvar="FUSEKI_ENDPOINT",
    help="Fuseki SPARQL endpoint URL (e.g., http://fuseki:3030/packagegraph).",
)
@click.option(
    "--output-dir",
    required=True,
    type=click.Path(),
    help="Directory for enrichment output (.nt files).",
)
@click.option(
    "--cache-dir",
    default=None,
    help="Directory for caching API responses.",
)
@click.option("--github-token", envvar="GITHUB_TOKEN", help="GitHub API token.")
@click.option("--minio-endpoint", envvar="MINIO_ENDPOINT", help="Minio endpoint URL.")
@click.option("--minio-bucket", default="packagegraph", envvar="MINIO_BUCKET", help="Minio bucket.")
@click.option("--minio-access-key", envvar="MINIO_ACCESS_KEY", help="Minio access key.")
@click.option("--minio-secret-key", envvar="MINIO_SECRET_KEY", help="Minio secret key.")
def enrich_license(
    fuseki_endpoint,
    output_dir,
    cache_dir,
    github_token,
    minio_endpoint,
    minio_bucket,
    minio_access_key,
    minio_secret_key,
):
    """Enrich package graph with license claims from GitHub API."""
    from packagegraph.sparql_client import SparqlQueryClient
    from packagegraph.enrichers.cache import CacheManager
    from packagegraph.enrichers.license import LicenseEnricher

    if cache_dir is None:
        cache_dir = Path.home() / ".cache" / "packagegraph" / "license"

    output_dir = Path(output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    output_file = output_dir / "licenses.nt"

    client = SparqlQueryClient(fuseki_endpoint)
    cache_mgr = CacheManager(
        cache_dir=str(cache_dir),
        enricher_name='github',  # Shared GitHub cache
        minio_endpoint=minio_endpoint,
        minio_bucket=minio_bucket,
        minio_access_key=minio_access_key,
        minio_secret_key=minio_secret_key,
    )

    enricher = LicenseEnricher(
        sparql_client=client,
        output_path=str(output_file),
        cache_manager=cache_mgr,
        enricher_version='1.0.0',
        github_token=github_token,
    )
    enricher.enrich()
    click.echo(f"License enrichment complete. Output: {output_file}")


@cli.command()
@click.option(
    "--fuseki-endpoint",
    required=True,
    envvar="FUSEKI_ENDPOINT",
    help="Fuseki SPARQL endpoint URL.",
)
@click.option(
    "--output-dir",
    required=True,
    type=click.Path(),
    help="Directory for enrichment output (.nt files).",
)
@click.option("--cache-dir", default=None, help="Directory for caching API responses.")
@click.option("--github-token", envvar="GITHUB_TOKEN", help="GitHub API token.")
@click.option("--minio-endpoint", envvar="MINIO_ENDPOINT", help="Minio endpoint URL.")
@click.option("--minio-bucket", default="packagegraph", help="Minio bucket.")
@click.option("--minio-access-key", envvar="MINIO_ACCESS_KEY", help="Minio access key.")
@click.option("--minio-secret-key", envvar="MINIO_SECRET_KEY", help="Minio secret key.")
def enrich_metrics(
    fuseki_endpoint,
    output_dir,
    cache_dir,
    github_token,
    minio_endpoint,
    minio_bucket,
    minio_access_key,
    minio_secret_key,
):
    """Enrich package graph with language metrics claims from GitHub API."""
    from packagegraph.sparql_client import SparqlQueryClient
    from packagegraph.enrichers.cache import CacheManager
    from packagegraph.enrichers.metrics import MetricsEnricher

    if cache_dir is None:
        cache_dir = Path.home() / ".cache" / "packagegraph" / "metrics"

    output_dir = Path(output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    output_file = output_dir / "metrics.nt"

    client = SparqlQueryClient(fuseki_endpoint)
    cache_mgr = CacheManager(
        cache_dir=str(cache_dir),
        enricher_name='github',  # Shared GitHub cache
        minio_endpoint=minio_endpoint,
        minio_bucket=minio_bucket,
        minio_access_key=minio_access_key,
        minio_secret_key=minio_secret_key,
    )

    enricher = MetricsEnricher(
        sparql_client=client,
        output_path=str(output_file),
        cache_manager=cache_mgr,
        enricher_version='1.0.0',
        github_token=github_token,
    )
    enricher.enrich()
    click.echo(f"Metrics enrichment complete. Output: {output_file}")


@cli.command()
@click.option(
    "--fuseki-endpoint",
    required=True,
    envvar="FUSEKI_ENDPOINT",
    help="Fuseki SPARQL endpoint URL.",
)
@click.option(
    "--output-dir",
    required=True,
    type=click.Path(),
    help="Directory for enrichment output (.nt files).",
)
@click.option("--cache-dir", default=None, help="Directory for caching API responses.")
@click.option("--github-token", envvar="GITHUB_TOKEN", help="GitHub API token.")
@click.option("--minio-endpoint", envvar="MINIO_ENDPOINT", help="Minio endpoint URL.")
@click.option("--minio-bucket", default="packagegraph", help="Minio bucket.")
@click.option("--minio-access-key", envvar="MINIO_ACCESS_KEY", help="Minio access key.")
@click.option("--minio-secret-key", envvar="MINIO_SECRET_KEY", help="Minio secret key.")
def enrich_vcs_activity(
    fuseki_endpoint,
    output_dir,
    cache_dir,
    github_token,
    minio_endpoint,
    minio_bucket,
    minio_access_key,
    minio_secret_key,
):
    """Enrich package graph with VCS activity claims from GitHub API."""
    from packagegraph.sparql_client import SparqlQueryClient
    from packagegraph.enrichers.cache import CacheManager
    from packagegraph.enrichers.vcs_activity import VCSActivityEnricher

    if cache_dir is None:
        cache_dir = Path.home() / ".cache" / "packagegraph" / "vcs_activity"

    output_dir = Path(output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    output_file = output_dir / "vcs_activity.nt"

    client = SparqlQueryClient(fuseki_endpoint)
    cache_mgr = CacheManager(
        cache_dir=str(cache_dir),
        enricher_name='github',  # Shared GitHub cache
        minio_endpoint=minio_endpoint,
        minio_bucket=minio_bucket,
        minio_access_key=minio_access_key,
        minio_secret_key=minio_secret_key,
    )

    enricher = VCSActivityEnricher(
        sparql_client=client,
        output_path=str(output_file),
        cache_manager=cache_mgr,
        enricher_version='1.0.0',
        github_token=github_token,
    )
    enricher.enrich()
    click.echo(f"VCS activity enrichment complete. Output: {output_file}")


@cli.command()
@click.option(
    "--fuseki-endpoint",
    required=True,
    envvar="FUSEKI_ENDPOINT",
    help="Fuseki SPARQL endpoint URL (e.g., http://fuseki:3030/packagegraph).",
)
@click.option(
    "--output-dir",
    required=True,
    type=click.Path(),
    help="Directory for enrichment output (.nt files).",
)
@click.option("--cache-dir", default=None, help="Directory for caching API responses.")
@click.option(
    "--type",
    "advisory_type",
    required=True,
    type=click.Choice(["rhsa", "dsa"], case_sensitive=False),
    help="Advisory type: rhsa (Red Hat) or dsa (Debian).",
)
@click.option("--days-back", default=365, type=int, help="For RHSA: days back to fetch advisories.")
def enrich_advisory(fuseki_endpoint, output_dir, cache_dir, advisory_type, days_back):
    """Enrich package graph with vendor security advisories (RHSA, DSA)."""
    from packagegraph.sparql_client import SparqlQueryClient
    from packagegraph.enrichers.advisory import RHSAEnricher, DSAEnricher

    output_dir = Path(output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    output_file = output_dir / f"advisory_{advisory_type}.nt"

    if cache_dir is None:
        cache_dir = Path.home() / ".cache" / "packagegraph" / f"advisory_{advisory_type}"

    client = SparqlQueryClient(fuseki_endpoint)

    if advisory_type == "rhsa":
        enricher = RHSAEnricher(
            sparql_client=client,
            output_path=str(output_file),
            cache_dir=str(cache_dir),
            days_back=days_back,
        )
    else:  # dsa
        enricher = DSAEnricher(
            sparql_client=client,
            output_path=str(output_file),
            cache_dir=str(cache_dir),
        )

    enricher.enrich()
    click.echo(f"Advisory enrichment ({advisory_type.upper()}) complete. Output: {output_file}")


@cli.command()
@click.option(
    "--fuseki-endpoint",
    required=True,
    envvar="FUSEKI_ENDPOINT",
    help="Fuseki SPARQL endpoint URL (e.g., http://fuseki:3030/packagegraph).",
)
@click.option(
    "--output-dir",
    required=True,
    type=click.Path(),
    help="Directory for enrichment output (.nt files).",
)
def enrich_npm_provenance(fuseki_endpoint, output_dir):
    """Enrich npm packages with SLSA provenance attestations from registry."""
    from packagegraph.sparql_client import SparqlQueryClient
    from packagegraph.enrichers.npm_provenance import NpmProvenanceEnricher

    output_dir = Path(output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    output_file = output_dir / "npm_provenance.nt"

    client = SparqlQueryClient(fuseki_endpoint)

    enricher = NpmProvenanceEnricher(
        sparql_client=client,
        output_path=str(output_file),
    )
    enricher.enrich()
    click.echo(f"npm provenance enrichment complete. Output: {output_file}")


# ─── Canned Query Commands ────────────────────────────────────────────────────

def _query_to_json(client, sparql: str) -> str:
    """Execute SPARQL query and return results as JSON string."""
    import json
    bindings = client.query(sparql)
    rows = [{k: v["value"] for k, v in b.items()} for b in bindings]
    return json.dumps(rows, indent=2)


@cli.command()
@click.option(
    "--fuseki-endpoint",
    required=True,
    envvar="FUSEKI_ENDPOINT",
    help="Fuseki SPARQL endpoint URL.",
)
def query_stats(fuseki_endpoint):
    """Distribution statistics — package and version counts per distro."""
    from packagegraph.sparql_client import SparqlQueryClient

    client = SparqlQueryClient(fuseki_endpoint)
    sparql = """
    PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
    PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
    SELECT ?distro (COUNT(DISTINCT ?p) AS ?packages) (COUNT(DISTINCT ?v) AS ?versions) WHERE {
      ?p a pkg:BinaryPackage ;
         pkg:partOfDistribution ?d ;
         pkg:hasVersion ?v .
      ?d rdfs:label ?distro .
    }
    GROUP BY ?distro
    ORDER BY DESC(?packages)
    """
    click.echo(_query_to_json(client, sparql))


@cli.command()
@click.argument("name")
@click.option(
    "--fuseki-endpoint",
    required=True,
    envvar="FUSEKI_ENDPOINT",
    help="Fuseki SPARQL endpoint URL.",
)
@click.option("--limit", default=50, help="Max results.")
def query_search(name, fuseki_endpoint, limit):
    """Search packages by name across all distributions."""
    from packagegraph.sparql_client import SparqlQueryClient

    client = SparqlQueryClient(fuseki_endpoint)
    # Escape the search term for SPARQL
    safe_name = name.replace('"', '\\"')
    sparql = f"""
    PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
    PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
    SELECT ?name ?version ?distro WHERE {{
      ?p a pkg:BinaryPackage ;
         pkg:packageName ?name ;
         pkg:hasVersion ?v ;
         pkg:partOfDistribution ?d .
      ?v pkg:versionString ?version .
      ?d rdfs:label ?distro .
      FILTER(CONTAINS(LCASE(?name), LCASE("{safe_name}")))
    }}
    ORDER BY ?name ?distro
    LIMIT {limit}
    """
    click.echo(_query_to_json(client, sparql))


@cli.command()
@click.argument("name")
@click.option(
    "--fuseki-endpoint",
    required=True,
    envvar="FUSEKI_ENDPOINT",
    help="Fuseki SPARQL endpoint URL.",
)
@click.option("--reverse", is_flag=True, help="Show reverse dependencies (who depends on this).")
def query_deps(name, fuseki_endpoint, reverse):
    """Query dependencies of a package (or reverse with --reverse)."""
    from packagegraph.sparql_client import SparqlQueryClient

    client = SparqlQueryClient(fuseki_endpoint)
    safe_name = name.replace('"', '\\"')

    if reverse:
        sparql = f"""
        PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
        SELECT ?pkg_name ?dep_type WHERE {{
          ?target pkg:packageName "{safe_name}" .
          ?dep pkg:dependencyTarget ?target ;
               pkg:dependencyType ?dep_type .
          ?p pkg:hasDependency ?dep ;
             pkg:packageName ?pkg_name .
        }}
        ORDER BY ?dep_type ?pkg_name
        LIMIT 100
        """
    else:
        sparql = f"""
        PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
        SELECT ?dep_name ?dep_type WHERE {{
          ?p a pkg:BinaryPackage ;
             pkg:packageName "{safe_name}" ;
             pkg:hasDependency ?dep .
          ?dep pkg:dependencyTarget ?target ;
               pkg:dependencyType ?dep_type .
          ?target pkg:packageName ?dep_name .
        }}
        ORDER BY ?dep_type ?dep_name
        LIMIT 100
        """
    click.echo(_query_to_json(client, sparql))


@cli.command()
@click.option(
    "--fuseki-endpoint",
    required=True,
    envvar="FUSEKI_ENDPOINT",
    help="Fuseki SPARQL endpoint URL.",
)
@click.option("--package", default=None, help="Filter to a specific package name.")
@click.option("--limit", default=50, help="Max results.")
def query_vulns(fuseki_endpoint, package, limit):
    """Query packages with known vulnerabilities."""
    from packagegraph.sparql_client import SparqlQueryClient

    client = SparqlQueryClient(fuseki_endpoint)
    pkg_filter = ""
    if package:
        safe_name = package.replace('"', '\\"')
        pkg_filter = f'FILTER(?pkg_name = "{safe_name}")'

    sparql = f"""
    PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
    PREFIX sec: <https://purl.org/packagegraph/ontology/security#>
    SELECT ?pkg_name ?cve_id ?severity ?published WHERE {{
      ?vuln a sec:Vulnerability ;
            sec:cveId ?cve_id ;
            sec:affectsVersion ?ver .
      ?pkg pkg:hasVersion ?ver ;
           pkg:packageName ?pkg_name .
      OPTIONAL {{ ?vuln sec:severity ?severity }}
      OPTIONAL {{ ?vuln sec:publishedDate ?published }}
      {pkg_filter}
    }}
    ORDER BY DESC(?published)
    LIMIT {limit}
    """
    click.echo(_query_to_json(client, sparql))


@cli.command()
@click.option(
    "--fuseki-endpoint",
    required=True,
    envvar="FUSEKI_ENDPOINT",
    help="Fuseki SPARQL endpoint URL.",
)
def query_graphs(fuseki_endpoint):
    """List named graphs and their triple counts."""
    from packagegraph.sparql_client import SparqlQueryClient

    client = SparqlQueryClient(fuseki_endpoint)
    sparql = """
    SELECT ?graph (COUNT(*) AS ?triples) WHERE {
      GRAPH ?graph { ?s ?p ?o }
    }
    GROUP BY ?graph
    ORDER BY DESC(?triples)
    """
    click.echo(_query_to_json(client, sparql))


@cli.command()
@click.argument("sparql_query")
@click.option(
    "--fuseki-endpoint",
    required=True,
    envvar="FUSEKI_ENDPOINT",
    help="Fuseki SPARQL endpoint URL.",
)
def query_raw(sparql_query, fuseki_endpoint):
    """Execute a raw SPARQL query. Pass the query string as an argument."""
    from packagegraph.sparql_client import SparqlQueryClient

    client = SparqlQueryClient(fuseki_endpoint)
    click.echo(_query_to_json(client, sparql_query))


# ─── Seed Commands ────────────────────────────────────────────────────────────

def _seed_from_homepage(client, pattern: str, extract_fn) -> list[str]:
    """Query Fuseki for package names matching a homepage pattern, then extract language names."""
    sparql = f"""
    PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
    SELECT DISTINCT ?name ?homepage WHERE {{
      ?p a pkg:BinaryPackage ;
         pkg:packageName ?name ;
         pkg:homepage ?homepage .
      FILTER(CONTAINS(LCASE(STR(?homepage)), "{pattern}"))
    }}
    """
    bindings = client.query(sparql)
    names = set()
    for b in bindings:
        extracted = extract_fn(b["name"]["value"], b["homepage"]["value"])
        if extracted:
            names.add(extracted)
    return sorted(names)


def _seed_from_upstream(client, ecosystem: str) -> list[str]:
    """Query Fuseki for upstream package names via pkg:upstreamPackageName.

    This is the preferred method — uses Provides data from RPM metadata
    which has the exact upstream name (e.g., 'tokio' not 'rust-tokio-devel').
    """
    sparql = f"""
    PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
    SELECT DISTINCT ?upstream WHERE {{
      ?p a pkg:BinaryPackage ;
         pkg:upstreamEcosystem "{ecosystem}" ;
         pkg:upstreamPackageName ?upstream .
    }}
    """
    try:
        bindings = client.query(sparql)
        return [b["upstream"]["value"] for b in bindings]
    except Exception:
        return []


def _seed_from_names(client, prefix: str) -> list[str]:
    """Fallback: extract upstream names from distro package names by stripping prefix/suffix.

    Use _seed_from_upstream() when possible — this method loses information
    (scoped packages, case sensitivity, etc.).
    """
    import re
    sparql = f"""
    PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
    SELECT DISTINCT ?name WHERE {{
      ?p a pkg:BinaryPackage ;
         pkg:packageName ?name .
      FILTER(STRSTARTS(?name, "{prefix}"))
    }}
    """
    rpm_suffixes = re.compile(
        r'(\+\w+)?-(devel|doc|static|libs|tools|data|common|debuginfo|debugsource|tests|help)$'
    )
    bindings = client.query(sparql)
    names = set()
    for b in bindings:
        full_name = b["name"]["value"]
        lang_name = full_name[len(prefix):]
        if lang_name:
            lang_name = rpm_suffixes.sub('', lang_name)
            if lang_name:
                names.add(lang_name)
    return sorted(names)


@cli.command()
@click.option("--fuseki-endpoint", required=True, envvar="FUSEKI_ENDPOINT")
@click.option("-o", "--output", required=True, type=click.Path())
@click.option("--from-names", is_flag=True, help="Also extract from node-* package names")
def seed_npm(fuseki_endpoint, output, from_names):
    """Generate NPM seed file from binary package graph."""
    from packagegraph.sparql_client import SparqlQueryClient

    client = SparqlQueryClient(fuseki_endpoint)
    names = set()

    # From homepage URLs
    def extract_npm(pkg_name, homepage):
        import re
        m = re.search(r'npmjs\.com/package/(.+?)(?:/|$)', homepage)
        return m.group(1) if m else None

    for n in _seed_from_homepage(client, "npmjs.com", extract_npm):
        names.add(n)

    # From binary package names
    if from_names:
        # Prefer Provides-derived names (exact upstream names)
        upstream = _seed_from_upstream(client, "npm")
        if upstream:
            for n in upstream:
                names.add(n)
        else:
            # Fallback: strip node- prefix (lossy — misses scoped packages)
            for n in _seed_from_names(client, "node-"):
                names.add(n)

    Path(output).write_text("\n".join(sorted(names)) + "\n")
    click.echo(f"Wrote {len(names)} NPM package names to {output}")


@cli.command()
@click.option("--fuseki-endpoint", required=True, envvar="FUSEKI_ENDPOINT")
@click.option("-o", "--output", required=True, type=click.Path())
@click.option("--from-names", is_flag=True, help="Also extract from python3-* package names")
def seed_pypi(fuseki_endpoint, output, from_names):
    """Generate PyPI seed file from binary package graph."""
    from packagegraph.sparql_client import SparqlQueryClient

    client = SparqlQueryClient(fuseki_endpoint)
    names = set()

    def extract_pypi(pkg_name, homepage):
        import re
        m = re.search(r'pypi\.org/project/(.+?)(?:/|$)', homepage)
        return m.group(1) if m else None

    for n in _seed_from_homepage(client, "pypi.org", extract_pypi):
        names.add(n)

    if from_names:
        upstream = _seed_from_upstream(client, "pypi")
        if upstream:
            for n in upstream:
                names.add(n)
        else:
            for n in _seed_from_names(client, "python3-"):
                names.add(n)
            for n in _seed_from_names(client, "python-"):
                names.add(n)

    Path(output).write_text("\n".join(sorted(names)) + "\n")
    click.echo(f"Wrote {len(names)} PyPI package names to {output}")


@cli.command()
@click.option("--fuseki-endpoint", required=True, envvar="FUSEKI_ENDPOINT")
@click.option("-o", "--output", required=True, type=click.Path())
@click.option("--from-names", is_flag=True, help="Also extract from rust-* package names")
def seed_cargo(fuseki_endpoint, output, from_names):
    """Generate Cargo seed file from binary package graph."""
    from packagegraph.sparql_client import SparqlQueryClient

    client = SparqlQueryClient(fuseki_endpoint)
    names = set()

    def extract_cargo(pkg_name, homepage):
        import re
        m = re.search(r'crates\.io/crates/(.+?)(?:/|$)', homepage)
        return m.group(1) if m else None

    for n in _seed_from_homepage(client, "crates.io", extract_cargo):
        names.add(n)

    if from_names:
        upstream = _seed_from_upstream(client, "cargo")
        if upstream:
            for n in upstream:
                names.add(n)
        else:
            for n in _seed_from_names(client, "rust-"):
                names.add(n)

    Path(output).write_text("\n".join(sorted(names)) + "\n")
    click.echo(f"Wrote {len(names)} Cargo crate names to {output}")


@cli.command()
@click.option("--fuseki-endpoint", required=True, envvar="FUSEKI_ENDPOINT")
@click.option("-o", "--output", required=True, type=click.Path())
@click.option("--from-names", is_flag=True, help="Also extract from golang-* package names")
def seed_gomod(fuseki_endpoint, output, from_names):
    """Generate Go modules seed file from binary package graph."""
    from packagegraph.sparql_client import SparqlQueryClient

    client = SparqlQueryClient(fuseki_endpoint)
    names = set()

    def extract_gomod(pkg_name, homepage):
        import re
        m = re.search(r'pkg\.go\.dev/(.+?)(?:\?|$)', homepage)
        if m:
            return m.group(1)
        m = re.search(r'github\.com/([^/]+/[^/]+)', homepage)
        return m.group(0) if m else None

    for n in _seed_from_homepage(client, "pkg.go.dev", extract_gomod):
        names.add(n)

    if from_names:
        upstream = _seed_from_upstream(client, "gomod")
        if upstream:
            for n in upstream:
                names.add(n)
        else:
            for n in _seed_from_names(client, "golang-"):
                parts = n.split("-", 2)
                if len(parts) >= 3 and parts[0] in ("github", "gitlab", "golang"):
                    host = parts[0] + ".com" if parts[0] != "golang" else "golang.org"
                    names.add(f"{host}/{'/'.join(parts[1:])}")

    Path(output).write_text("\n".join(sorted(names)) + "\n")
    click.echo(f"Wrote {len(names)} Go module paths to {output}")


@cli.command()
@click.option("--fuseki-endpoint", required=True, envvar="FUSEKI_ENDPOINT")
@click.option("-o", "--output", required=True, type=click.Path())
@click.option("--from-names", is_flag=True, help="Also extract from rubygem-* package names")
def seed_rubygems(fuseki_endpoint, output, from_names):
    """Generate RubyGems seed file from binary package graph."""
    from packagegraph.sparql_client import SparqlQueryClient

    client = SparqlQueryClient(fuseki_endpoint)
    names = set()

    def extract_rubygem(pkg_name, homepage):
        import re
        m = re.search(r'rubygems\.org/gems/(.+?)(?:/|$)', homepage)
        return m.group(1) if m else None

    for n in _seed_from_homepage(client, "rubygems.org", extract_rubygem):
        names.add(n)

    if from_names:
        upstream = _seed_from_upstream(client, "rubygems")
        if upstream:
            for n in upstream:
                names.add(n)
        else:
            for n in _seed_from_names(client, "rubygem-"):
                names.add(n)

    Path(output).write_text("\n".join(sorted(names)) + "\n")
    click.echo(f"Wrote {len(names)} RubyGems names to {output}")


@cli.command()
@click.option("--fuseki-endpoint", required=True, envvar="FUSEKI_ENDPOINT")
@click.option("-o", "--output", required=True, type=click.Path())
@click.option("--from-names", is_flag=True, help="Also extract from maven-* and java-* package names")
def seed_maven(fuseki_endpoint, output, from_names):
    """Generate Maven seed file (groupId:artifactId) from binary package graph."""
    from packagegraph.sparql_client import SparqlQueryClient
    import re

    client = SparqlQueryClient(fuseki_endpoint)
    coords = set()

    # Extract from homepages matching maven.apache.org or search.maven.org
    def extract_maven(pkg_name, homepage):
        # Match: mvnrepository.com/artifact/groupId/artifactId
        m = re.search(r'mvnrepository\.com/artifact/([^/]+)/([^/]+)', homepage)
        if m:
            return f"{m.group(1)}:{m.group(2)}"
        # Match: search.maven.org/artifact/groupId/artifactId
        m = re.search(r'search\.maven\.org/artifact/([^/]+)/([^/]+)', homepage)
        if m:
            return f"{m.group(1)}:{m.group(2)}"
        return None

    for coord in _seed_from_homepage(client, "maven", extract_maven):
        if coord:
            coords.add(coord)

    if from_names:
        # Get from upstreamPackageName (ecosystem=maven)
        upstream = _seed_from_upstream(client, "maven")
        if upstream:
            for coord in upstream:
                coords.add(coord)
        else:
            # Fallback: extract from maven-* package names
            for n in _seed_from_names(client, "maven-"):
                # Try to derive groupId:artifactId — this is lossy
                # Many maven-* packages don't encode coordinates in the name
                # Example: maven-compiler-plugin → org.apache.maven.plugins:maven-compiler-plugin
                # For now, just use the name as artifactId with a placeholder groupId
                coords.add(f"org.apache.maven.plugins:{n}")

    Path(output).write_text("\n".join(sorted(coords)) + "\n")
    click.echo(f"Wrote {len(coords)} Maven coordinates to {output}")


@cli.command()
@click.option("--fuseki-endpoint", required=True, envvar="FUSEKI_ENDPOINT")
@click.option("-o", "--output", required=True, type=click.Path())
@click.option("--from-names", is_flag=True, help="Also extract from perl-* package names")
def seed_cpan(fuseki_endpoint, output, from_names):
    """Generate CPAN seed file from binary package graph."""
    from packagegraph.sparql_client import SparqlQueryClient
    import re

    client = SparqlQueryClient(fuseki_endpoint)
    names = set()

    def extract_cpan(pkg_name, homepage):
        m = re.search(r'metacpan\.org/(?:pod|release)/(.+?)(?:/|$)', homepage)
        if m:
            return m.group(1)
        m = re.search(r'cpan\.org/(?:dist|module)/(.+?)(?:/|$)', homepage)
        if m:
            return m.group(1)
        return None

    for n in _seed_from_homepage(client, "metacpan.org", extract_cpan):
        if n:
            names.add(n)

    if from_names:
        upstream = _seed_from_upstream(client, "cpan")
        if upstream:
            for n in upstream:
                names.add(n)
        else:
            for n in _seed_from_names(client, "perl-"):
                # Convert perl-Module-Name to Module::Name
                parts = n.split("-")
                dist_name = "::".join(parts)
                names.add(dist_name)

    Path(output).write_text("\n".join(sorted(names)) + "\n")
    click.echo(f"Wrote {len(names)} CPAN distribution names to {output}")


@cli.command()
@click.option("--fuseki-endpoint", required=True, envvar="FUSEKI_ENDPOINT")
@click.option("-o", "--output", required=True, type=click.Path())
@click.option("--from-names", is_flag=True, help="Also extract from ghc-* package names")
def seed_hackage(fuseki_endpoint, output, from_names):
    """Generate Hackage seed file from binary package graph."""
    from packagegraph.sparql_client import SparqlQueryClient
    import re

    client = SparqlQueryClient(fuseki_endpoint)
    names = set()

    def extract_hackage(pkg_name, homepage):
        m = re.search(r'hackage\.haskell\.org/package/(.+?)(?:/|$)', homepage)
        return m.group(1) if m else None

    for n in _seed_from_homepage(client, "hackage.haskell.org", extract_hackage):
        if n:
            names.add(n)

    if from_names:
        upstream = _seed_from_upstream(client, "hackage")
        if upstream:
            for n in upstream:
                names.add(n)
        else:
            for n in _seed_from_names(client, "ghc-"):
                names.add(n)

    Path(output).write_text("\n".join(sorted(names)) + "\n")
    click.echo(f"Wrote {len(names)} Hackage package names to {output}")


@cli.command()
@click.option("--fuseki-endpoint", required=True, envvar="FUSEKI_ENDPOINT")
@click.option("-o", "--output", required=True, type=click.Path())
@click.option("--from-names", is_flag=True, help="Also extract from dotnet-*/aspnetcore-* package names")
def seed_nuget(fuseki_endpoint, output, from_names):
    """Generate NuGet seed file from binary package graph."""
    from packagegraph.sparql_client import SparqlQueryClient
    import re

    client = SparqlQueryClient(fuseki_endpoint)
    names = set()

    def extract_nuget(pkg_name, homepage):
        m = re.search(r'nuget\.org/packages/(.+?)(?:/|$)', homepage)
        return m.group(1) if m else None

    for n in _seed_from_homepage(client, "nuget.org", extract_nuget):
        if n:
            names.add(n)

    if from_names:
        upstream = _seed_from_upstream(client, "nuget")
        if upstream:
            for n in upstream:
                names.add(n)
        else:
            for n in _seed_from_names(client, "dotnet-"):
                names.add(n)
            for n in _seed_from_names(client, "aspnetcore-"):
                names.add(n)

    Path(output).write_text("\n".join(sorted(names)) + "\n")
    click.echo(f"Wrote {len(names)} NuGet package names to {output}")


@cli.command()
@click.option("--fuseki-endpoint", required=True, envvar="FUSEKI_ENDPOINT")
@click.option("-o", "--output", required=True, type=click.Path())
@click.option("--from-names", is_flag=True, help="Also extract from erlang-* package names")
def seed_hex(fuseki_endpoint, output, from_names):
    """Generate Hex seed file from binary package graph."""
    from packagegraph.sparql_client import SparqlQueryClient
    import re

    client = SparqlQueryClient(fuseki_endpoint)
    names = set()

    def extract_hex(pkg_name, homepage):
        m = re.search(r'hex\.pm/packages/(.+?)(?:/|$)', homepage)
        return m.group(1) if m else None

    for n in _seed_from_homepage(client, "hex.pm", extract_hex):
        if n:
            names.add(n)

    if from_names:
        upstream = _seed_from_upstream(client, "hex")
        if upstream:
            for n in upstream:
                names.add(n)
        else:
            for n in _seed_from_names(client, "erlang-"):
                names.add(n)

    Path(output).write_text("\n".join(sorted(names)) + "\n")
    click.echo(f"Wrote {len(names)} Hex package names to {output}")


if __name__ == "__main__":
    cli()
