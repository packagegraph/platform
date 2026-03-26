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
@click.argument("repo_url")
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
    "--arch", default="binary-amd64", help="[Debian] The architecture to collect."
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

    with profiler.step("Total Collection Time"):
        g = Graph()

        if input_file and Path(input_file).exists():
            with profiler.step("Load Existing Graph"):
                click.echo(f"Loading existing graph from {input_file}")
                g.parse(input_file)
                click.echo(f"Loaded {len(g)} triples.")

        try:
            with profiler.step("Initialize Collector"):
                if repo_type == "debian":
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
                elif repo_type == "rpm":
                    collector = RpmCollector(g, repo_url, parallel, chunk_size, workers)

            with profiler.step("Collect Package Data"):
                parsed_count = collector.collect()

            click.echo(f"Successfully processed {parsed_count} packages.")
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

    # Gather input files
    input_files = sorted(input_dir.glob("*.nt")) + sorted(input_dir.glob("*.ttl"))
    input_files += sorted(ontology_dir.glob("*.nt")) + sorted(
        ontology_dir.glob("*.ttl")
    )

    if not input_files:
        click.echo("No .nt or .ttl files found in input-dir or ontology-dir.", err=True)
        sys.exit(1)

    click.echo(f"Found {len(input_files)} input files.")

    # Build TDB2 index
    tdb_dir = output_dir / "tdb2"
    tdb_dir.mkdir(parents=True, exist_ok=True)

    builder = TDB2Builder(jena_home=jena_home)
    try:
        click.echo("Building TDB2 index...")
        builder.build(input_files, tdb_dir)
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


if __name__ == "__main__":
    cli()
