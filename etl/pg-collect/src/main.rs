use clap::{Parser, Subcommand};
use pg_collect::alpine::AlpineCollector;
use pg_collect::arch::ArchCollector;
use pg_collect::buildroot::BuildrootCollector;
use pg_collect::cache::MinioConfig;
use pg_collect::chocolatey::ChocolateyCollector;
use pg_collect::collect_bodhi::BodhiCollector;
use pg_collect::collect_glsa::GlsaCollector;
use pg_collect::collect_salsa::SalsaCollector;
use pg_collect::collect_sources::SourcesCollector;
use pg_collect::collect_spec::SpecCollector;
use pg_collect::cpan::CpanCollector;
use pg_collect::cran::CranCollector;
use pg_collect::debian::{normalize_arch, DebianCollector};
use pg_collect::derive_releases::ReleaseDeriver;
use pg_collect::enrich_advisory::{AdvisoryEnricher, AdvisoryType};
use pg_collect::enrich_blast_radius::BlastRadiusEnricher;
use pg_collect::enrich_epss::EpssEnricher;
use pg_collect::enrich_forge_version::ForgeVersionEnricher;
use pg_collect::enrich_github::GitHubEnricher;
use pg_collect::enrich_koji::KojiEnricher;
use pg_collect::enrich_npm_provenance::NpmProvenanceEnricher;
use pg_collect::enrich_nvd::NvdEnricher;
use pg_collect::enrich_repology::RepologyEnricher;
use pg_collect::enrich_revdeps::RevdepsEnricher;
use pg_collect::enrich_security::SecurityEnricher;
use pg_collect::enrich_taxonomy::TaxonomyEnricher;
use pg_collect::freebsd::FreebsdCollector;
use pg_collect::hackage::HackageCollector;
use pg_collect::hex_collect::HexCollector;
use pg_collect::homebrew::HomebrewCollector;
use pg_collect::maven::MavenCollector;
use pg_collect::nix::NixCollector;
use pg_collect::ntriples::NTriplesWriter;
use pg_collect::nuget::NugetCollector;
use pg_collect::openwrt::OpenWrtCollector;
use pg_collect::rpm::RpmCollector;
use pg_collect::rubygems::RubyGemsCollector;
use pg_collect::seed;
use pg_collect::sparql::{SparqlAuth, SparqlBackend};
use pg_collect::yocto::YoctoCollector;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "pg-collect")]
#[command(about = "PackageGraph bulk collector - streams N-Triples from package repositories")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// SPARQL endpoint username for Basic Auth
    #[arg(long, global = true, env = "FUSEKI_USERNAME")]
    sparql_username: Option<String>,

    /// SPARQL endpoint password for Basic Auth
    #[arg(long, global = true, env = "FUSEKI_PASSWORD")]
    sparql_password: Option<String>,

    /// SPARQL backend type (fuseki or qlever)
    #[arg(long, global = true, default_value = "fuseki", env = "SPARQL_BACKEND")]
    sparql_backend: String,

    /// QLever access token (required when --sparql-backend=qlever)
    #[arg(long, global = true, env = "QLEVER_ACCESS_TOKEN")]
    qlever_access_token: Option<String>,

    /// Write backend for load/drop (fuseki or minio)
    #[arg(long, global = true, default_value = "fuseki", env = "WRITE_BACKEND")]
    write_backend: String,

    /// Minio endpoint URL (required when --write-backend=minio)
    #[arg(long, global = true, env = "MINIO_ENDPOINT")]
    minio_endpoint: Option<String>,

    /// Minio bucket name (required when --write-backend=minio)
    #[arg(
        long,
        global = true,
        default_value = "packagegraph",
        env = "MINIO_BUCKET"
    )]
    minio_bucket: String,

    /// Graph URI for N-Quads output (appends graph term to each triple).
    /// Pass explicitly — not bound to GRAPH_URI env var to avoid conflict
    /// with scheduled collector jobs that set GRAPH_URI for the load command.
    #[arg(long, global = true)]
    graph: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Collect from Debian repository
    Debian {
        /// Repository URL
        #[arg(long, required = true)]
        repo: String,

        /// Distro identifier (debian, ubuntu, mint, etc.)
        #[arg(long, default_value = "debian")]
        distro: String,

        /// Distribution codename (stable, trixie, noble, etc.)
        #[arg(long, default_value = "stable")]
        dist: String,

        /// Component name
        #[arg(long, default_value = "main")]
        component: String,

        /// Architecture(s) to collect (e.g., binary-amd64)
        #[arg(long, default_value = "amd64")]
        arch: Vec<String>,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,

        /// Number of worker threads (unused - kept for CLI compatibility)
        #[arg(long, default_value = "4")]
        workers: usize,

        /// Source artifact cache directory (enables conditional GET caching)
        #[arg(long)]
        cache_dir: Option<String>,
    },

    /// Collect from RPM repository
    Rpm {
        /// Repository URL
        #[arg(long)]
        repo: Option<String>,

        /// RPM repository spec (name:release:url), can be specified multiple times
        #[arg(long = "rpm-repo")]
        rpm_repos: Vec<String>,

        /// Distribution name
        #[arg(long, default_value = "fedora")]
        distro_name: String,

        /// Release name
        #[arg(long, default_value = "")]
        release_name: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,

        /// TLS client certificate PEM file (for RHEL CDN access)
        #[arg(long)]
        sslclientcert: Option<String>,

        /// TLS client key PEM file (for RHEL CDN access)
        #[arg(long)]
        sslclientkey: Option<String>,

        /// TLS CA certificate PEM file (for RHEL CDN access)
        #[arg(long)]
        sslcacert: Option<String>,

        /// Repository type (optional, inferred from URL if not provided)
        #[arg(long)]
        repo_type: Option<String>,

        /// Source artifact cache directory (enables conditional GET caching)
        #[arg(long)]
        cache_dir: Option<String>,
    },

    /// Collect from Alpine APK repository
    Alpine {
        /// Mirror URL (e.g., https://dl-cdn.alpinelinux.org/alpine)
        #[arg(long, default_value = "https://dl-cdn.alpinelinux.org/alpine")]
        mirror: String,

        /// Distribution name
        #[arg(long, default_value = "alpine")]
        distro: String,

        /// Alpine branch (e.g., v3.20, edge)
        #[arg(long, default_value = "v3.20")]
        branch: String,

        /// Repository names (main, community, testing)
        #[arg(long = "repo", default_values = ["main", "community"])]
        repos: Vec<String>,

        /// Architecture
        #[arg(long, default_value = "x86_64")]
        arch: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect from Homebrew formulae.brew.sh API
    Homebrew {
        /// API base URL
        #[arg(long, default_value = "https://formulae.brew.sh/api")]
        api_base: String,

        /// Distribution name
        #[arg(long, default_value = "homebrew")]
        distro: String,

        /// Release name
        #[arg(long, default_value = "homebrew")]
        release: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect from Arch Linux repositories
    Arch {
        /// Mirror URL (e.g., https://archive.archlinux.org/repos/last)
        #[arg(long, default_value = "https://archive.archlinux.org/repos/last")]
        mirror: String,

        /// Distribution name
        #[arg(long, default_value = "arch")]
        distro: String,

        /// Release name
        #[arg(long, default_value = "rolling")]
        release: String,

        /// Architecture
        #[arg(long, default_value = "x86_64")]
        arch: String,

        /// Repository names (core, extra, multilib)
        #[arg(long = "repo", default_values = ["core", "extra"])]
        repos: Vec<String>,

        /// Include AUR packages
        #[arg(long)]
        include_aur: bool,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect NPM packages from registry.npmjs.org
    Npm {
        /// Seed file with NPM package names (one per line). Omit with --endpoint to auto-discover.
        #[arg(long)]
        packages_file: Option<String>,

        /// Fuseki SPARQL endpoint for auto-discovery
        #[arg(long)]
        endpoint: Option<String>,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect Python packages from pypi.org
    Pypi {
        /// Seed file with PyPI package names (one per line). Omit with --endpoint to auto-discover.
        #[arg(long)]
        packages_file: Option<String>,

        /// Fuseki SPARQL endpoint for auto-discovery
        #[arg(long)]
        endpoint: Option<String>,

        /// Maximum dependency depth to spider (0 = seed only, no transitive deps)
        #[arg(long, default_value = "2")]
        max_depth: u32,

        /// Maximum total packages to collect (stops spider when reached)
        #[arg(long, default_value = "5000")]
        max_packages: usize,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,

        /// Cache directory for HTTP responses
        #[arg(long)]
        cache_dir: Option<String>,

        /// TTL for cached successful responses (hours)
        #[arg(long, default_value = "24")]
        cache_ttl_hours: u64,
    },

    /// Collect Rust crates from crates.io
    Cargo {
        /// Seed file with crate names (one per line). Omit with --endpoint to auto-discover.
        #[arg(long)]
        packages_file: Option<String>,

        /// Fuseki SPARQL endpoint for auto-discovery
        #[arg(long)]
        endpoint: Option<String>,

        /// Maximum dependency depth to spider (0 = seed only, no transitive deps)
        #[arg(long, default_value = "2")]
        max_depth: u32,

        /// Maximum total crates to collect (stops spider when reached)
        #[arg(long, default_value = "5000")]
        max_packages: usize,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect Go modules from proxy.golang.org
    Gomod {
        /// Seed file with Go module paths (one per line). Omit with --endpoint to auto-discover.
        #[arg(long)]
        packages_file: Option<String>,

        /// Fuseki SPARQL endpoint for auto-discovery
        #[arg(long)]
        endpoint: Option<String>,

        /// Go module proxy URL
        #[arg(long, default_value = "https://proxy.golang.org")]
        proxy: String,

        /// Maximum dependency depth to spider (0 = seed only, no transitive deps)
        #[arg(long, default_value = "2")]
        max_depth: u32,

        /// Maximum total modules to collect (stops spider when reached)
        #[arg(long, default_value = "5000")]
        max_packages: usize,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect Conda packages from conda-forge
    Conda {
        /// Seed file with package names (one per line), or omit for full collection
        #[arg(long)]
        packages_file: Option<String>,

        /// Distribution name
        #[arg(long, default_value = "conda")]
        distro: String,

        /// Release name
        #[arg(long, default_value = "conda-forge")]
        release: String,

        /// Conda channel URL
        #[arg(long, default_value = "https://conda.anaconda.org/conda-forge")]
        channel_url: String,

        /// Subdirectory (e.g., linux-64, osx-64, win-64)
        #[arg(long, default_value = "linux-64")]
        subdir: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect Flatpak apps from Flathub
    Flatpak {
        /// Seed file with Flatpak app IDs (one per line). Omit to auto-discover from Flathub.
        #[arg(long)]
        packages_file: Option<String>,

        /// Distribution name
        #[arg(long, default_value = "flatpak")]
        distro: String,

        /// Release name
        #[arg(long, default_value = "flathub")]
        release: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect Snap packages from Snap Store
    Snap {
        /// Seed file with Snap names (one per line). Omit to auto-discover from Snap Store.
        #[arg(long)]
        packages_file: Option<String>,

        /// Distribution name
        #[arg(long, default_value = "snap")]
        distro: String,

        /// Release name
        #[arg(long, default_value = "store")]
        release: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect Gentoo packages from local gentoo.git clone
    Gentoo {
        /// Path to local gentoo.git repository clone
        #[arg(long, required = true)]
        repo_path: String,

        /// Distribution name
        #[arg(long, default_value = "gentoo")]
        distro: String,

        /// Release name
        #[arg(long, default_value = "gentoo")]
        release: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect Void Linux packages from local void-packages clone
    Void {
        /// Path to local void-packages repository clone
        #[arg(long, required = true)]
        repo_path: String,

        /// Distribution name
        #[arg(long, default_value = "void")]
        distro: String,

        /// Release name
        #[arg(long, default_value = "void")]
        release: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect Yocto/OpenEmbedded recipes from local layer clones
    Yocto {
        /// Path(s) to OE layer directories (can be specified multiple times)
        #[arg(long, required = true)]
        layer: Vec<String>,

        /// Distribution name
        #[arg(long, default_value = "yocto")]
        distro: String,

        /// Release name
        #[arg(long, default_value = "yocto")]
        release: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect Buildroot packages from local buildroot.git clone
    Buildroot {
        /// Path to local Buildroot repository clone
        #[arg(long, required = true)]
        repo_path: String,

        /// Distribution name
        #[arg(long, default_value = "buildroot")]
        distro: String,

        /// Release name
        #[arg(long, default_value = "buildroot")]
        release: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect OpenWRT packages from local feed clone
    Openwrt {
        /// Path to local OpenWRT feed repository clone
        #[arg(long, required = true)]
        feed_path: String,

        /// Distribution name
        #[arg(long, default_value = "openwrt")]
        distro: String,

        /// Release name
        #[arg(long, default_value = "openwrt")]
        release: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect OpenWRT packages with full enrichment (multi-feed, binary index, upstream, attestation)
    OpenwrtFull {
        /// Feed repository paths (repeatable, basename must match feed name)
        #[arg(long = "feed", required = true)]
        feeds: Vec<String>,

        /// Distribution name
        #[arg(long, default_value = "openwrt")]
        distro: String,

        /// Release name (required - no default for full collector)
        #[arg(long, required = true)]
        release: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,

        /// Release download URL (enables Stage 2 binary index)
        #[arg(long)]
        release_url: Option<String>,

        /// Architecture (required with --release-url)
        #[arg(long)]
        arch: Option<String>,

        /// Enable upstream source enrichment
        #[arg(long)]
        with_upstream: bool,

        /// Enable SLSA attestation enrichment (requires --release-url)
        #[arg(long)]
        with_attestation: bool,

        /// GitHub token for attestation API
        #[arg(long, env = "GITHUB_TOKEN")]
        github_token: Option<String>,

        /// Cache directory
        #[arg(long)]
        cache_dir: Option<String>,

        /// Minio endpoint for cache sync
        #[arg(long, env = "MINIO_ENDPOINT")]
        minio_endpoint: Option<String>,

        /// Minio bucket
        #[arg(long, env = "MINIO_BUCKET", default_value = "packagegraph")]
        minio_bucket: String,

        /// Minio access key
        #[arg(long, env = "MINIO_ACCESS_KEY")]
        minio_access_key: Option<String>,

        /// Minio secret key
        #[arg(long, env = "MINIO_SECRET_KEY")]
        minio_secret_key: Option<String>,

        /// Maximum packages to process per stage
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Collect OSV security vulnerability data for a specific ecosystem
    Osv {
        /// Ecosystem name (e.g., npm, PyPI, crates.io, Go, Maven, etc.)
        #[arg(long, required = true)]
        ecosystem: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect Bodhi security advisories for Fedora
    CollectBodhi {
        /// Fuseki SPARQL endpoint URL for NVR→binary resolution
        #[arg(long, required = true)]
        endpoint: String,

        /// Fedora release tag (e.g., F43)
        #[arg(long, required = true)]
        release: String,

        /// Output N-Triples file path
        #[arg(short, long, required = true)]
        output: String,

        /// Only process advisories after this date (ISO format: YYYY-MM-DD)
        #[arg(long)]
        since: Option<String>,

        /// Cache directory for RSS feeds
        #[arg(long)]
        cache_dir: Option<String>,
    },

    /// Collect GLSA security advisories for Gentoo
    CollectGlsa {
        /// Fuseki SPARQL endpoint URL for atom→package resolution
        #[arg(long, required = true)]
        endpoint: String,

        /// Output N-Triples file path
        #[arg(short, long, required = true)]
        output: String,

        /// Only process GLSAs announced after this date (ISO format: YYYY-MM-DD)
        #[arg(long)]
        since: Option<String>,

        /// Cache directory for GLSA XML files
        #[arg(long)]
        cache_dir: Option<String>,
    },

    /// Collect RubyGems packages from rubygems.org
    Rubygems {
        /// Seed file with gem names (one per line). Omit with --endpoint to auto-discover.
        #[arg(long)]
        packages_file: Option<String>,

        /// Fuseki SPARQL endpoint for auto-discovery (queries rubygem() provides from RPM repos)
        #[arg(long)]
        endpoint: Option<String>,

        /// API base URL
        #[arg(long, default_value = "https://rubygems.org")]
        api_base: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect Maven artifacts from Maven Central.
    ///
    /// Emits raw POM declarations, NOT effective Maven resolution.
    /// When --max-depth > 0, performs recursive BFS traversal of dependency graphs.
    Maven {
        /// Seed file with groupId:artifactId coordinates (one per line). Omit with --endpoint to auto-discover.
        #[arg(long)]
        packages_file: Option<String>,

        /// Fuseki SPARQL endpoint for auto-discovery
        #[arg(long)]
        endpoint: Option<String>,

        /// Maven search API base URL
        #[arg(long, default_value = "https://search.maven.org")]
        search_base: String,

        /// Maven repository base URL
        #[arg(long, default_value = "https://repo1.maven.org/maven2")]
        repo_base: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,

        /// Cache directory for HTTP responses
        #[arg(long)]
        cache_dir: Option<String>,

        /// Force cache refresh (bypass fresh entries, re-fetch from network)
        #[arg(long)]
        cache_refresh: bool,

        /// Maximum traversal depth (0 = seed-only, no recursion)
        #[arg(long, default_value = "3")]
        max_depth: u32,

        /// Maximum number of seed roots to process
        #[arg(long, default_value = "10000")]
        max_roots: usize,

        /// Maximum total packages to schedule for fetching
        #[arg(long, default_value = "5000")]
        max_packages: usize,

        /// Courtesy delay between network requests in milliseconds
        #[arg(long, default_value = "500")]
        delay_ms: u64,
    },

    /// Collect CPAN distributions from MetaCPAN
    Cpan {
        /// Seed file with distribution names (one per line). Omit with --endpoint to auto-discover.
        #[arg(long)]
        packages_file: Option<String>,

        /// Fuseki SPARQL endpoint for auto-discovery
        #[arg(long)]
        endpoint: Option<String>,

        /// MetaCPAN API base URL
        #[arg(long, default_value = "https://fastapi.metacpan.org")]
        api_base: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect CRAN packages from CRAN mirror
    Cran {
        /// CRAN mirror URL
        #[arg(long, default_value = "https://cran.r-project.org")]
        mirror: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect Hackage packages from hackage.haskell.org
    Hackage {
        /// Seed file with package names (one per line). Omit with --endpoint to auto-discover.
        #[arg(long)]
        packages_file: Option<String>,

        /// Fuseki SPARQL endpoint for auto-discovery
        #[arg(long)]
        endpoint: Option<String>,

        /// Hackage base URL
        #[arg(long, default_value = "https://hackage.haskell.org")]
        base_url: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect NuGet packages from nuget.org
    Nuget {
        /// Seed file with package IDs (one per line). Omit with --endpoint to auto-discover.
        #[arg(long)]
        packages_file: Option<String>,

        /// Fuseki SPARQL endpoint for auto-discovery
        #[arg(long)]
        endpoint: Option<String>,

        /// NuGet service index URL
        #[arg(long, default_value = "https://api.nuget.org/v3/index.json")]
        service_index: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect Hex packages from hex.pm
    Hex {
        /// Seed file with package names (one per line). Omit with --endpoint to auto-discover.
        #[arg(long)]
        packages_file: Option<String>,

        /// Fuseki SPARQL endpoint for auto-discovery
        #[arg(long)]
        endpoint: Option<String>,

        /// Hex API base URL
        #[arg(long, default_value = "https://hex.pm")]
        api_base: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect FreeBSD packages from pkg.freebsd.org
    Freebsd {
        /// Mirror URL
        #[arg(long, default_value = "https://pkg.freebsd.org")]
        mirror: String,

        /// Distribution name
        #[arg(long, default_value = "freebsd")]
        distro: String,

        /// FreeBSD release (e.g., "14", "13")
        #[arg(long, default_value = "14")]
        release: String,

        /// Architecture (e.g., "amd64", "arm64")
        #[arg(long, default_value = "amd64")]
        arch: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect Nix packages from NixOS channel
    Nix {
        /// Channel URL
        #[arg(long, default_value = "https://channels.nixos.org/nixos-24.05")]
        channel_url: String,

        /// Distribution name
        #[arg(long, default_value = "nix")]
        distro: String,

        /// Release name
        #[arg(long, default_value = "nixpkgs")]
        release: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect Chocolatey packages from community repository
    Chocolatey {
        /// API URL
        #[arg(long, default_value = "https://community.chocolatey.org/api/v2")]
        api_url: String,

        /// Distribution name
        #[arg(long, default_value = "chocolatey")]
        distro: String,

        /// Release name
        #[arg(long, default_value = "community")]
        release: String,

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Load N-Triples file into a Fuseki named graph via SPARQL Update
    Load {
        /// N-Triples file to load
        #[arg(required = true)]
        file: String,

        /// Named graph URI (e.g., https://packagegraph.github.io/graph/debian/trixie)
        #[arg(long, required = true)]
        graph: String,

        /// Fuseki SPARQL endpoint base URL (e.g., http://fuseki:3030/packagegraph)
        #[arg(long, required = true)]
        endpoint: String,

        /// Number of triples per INSERT DATA batch
        #[arg(long, default_value = "10000")]
        batch_size: usize,
    },

    /// Drop a named graph from Fuseki
    Drop {
        /// Named graph URI to drop
        #[arg(long, required = true)]
        graph: String,

        /// Fuseki SPARQL endpoint base URL
        #[arg(long, required = true)]
        endpoint: String,
    },

    /// Extract a test corpus subset from a fully loaded Fuseki instance
    ExtractTestCorpus {
        /// Fuseki SPARQL endpoint base URL
        #[arg(long, required = true)]
        endpoint: String,

        /// Path to TOML seed configuration file
        #[arg(long, default_value = "test-corpus.toml")]
        config: String,

        /// Path to ontology directory (scanned recursively for *.ttl)
        #[arg(long, default_value = "../../../ontology")]
        ontology_dir: String,

        /// Output directory for .nt files, manifest, and coverage report
        #[arg(long, default_value = "../../test-data")]
        output_dir: String,

        /// Maximum total triples (overrides config file)
        #[arg(long)]
        max_triples: Option<usize>,

        /// BFS depth (overrides config file)
        #[arg(long)]
        depth: Option<usize>,

        /// Fan-out cap per (seed, predicate) pair (overrides config file)
        #[arg(long)]
        fan_out: Option<usize>,
    },

    // ─── Enricher Commands ─────────────────────────────────────────────
    /// Enrich package graph with GitHub VCS metadata, language metrics, and license
    EnrichGithub {
        /// Fuseki SPARQL endpoint URL
        #[arg(long, required = true)]
        endpoint: String,

        /// Output N-Triples file
        #[arg(short, long, required = true)]
        output: String,

        /// GitHub API token (or set GITHUB_TOKEN env var)
        #[arg(long, env = "GITHUB_TOKEN")]
        github_token: Option<String>,

        /// Cache directory for GitHub API responses
        #[arg(long)]
        cache_dir: Option<String>,

        /// Minio endpoint for cache sync
        #[arg(long, env = "MINIO_ENDPOINT")]
        minio_endpoint: Option<String>,

        /// Minio bucket
        #[arg(long, env = "MINIO_BUCKET", default_value = "packagegraph")]
        minio_bucket: String,

        /// Minio access key
        #[arg(long, env = "MINIO_ACCESS_KEY")]
        minio_access_key: Option<String>,

        /// Minio secret key
        #[arg(long, env = "MINIO_SECRET_KEY")]
        minio_secret_key: Option<String>,

        /// Maximum number of repos to process (incremental mode)
        #[arg(long)]
        max_repos: Option<usize>,

        /// Graph URI to load triples into (enables incremental mode with internal GSP loading)
        #[arg(long)]
        load_graph: Option<String>,
    },

    /// Enrich package graph with vendor security advisories (RHSA or DSA)
    EnrichAdvisory {
        /// Advisory type
        #[arg(long, required = true, value_parser = ["rhsa", "dsa"])]
        advisory_type: String,

        /// Output N-Triples file
        #[arg(short, long, required = true)]
        output: String,

        /// Days of advisories to fetch (RHSA only)
        #[arg(long, default_value = "365")]
        days_back: u32,

        /// Cache directory
        #[arg(long)]
        cache_dir: Option<String>,
    },

    /// Enrich npm packages with SLSA provenance attestations
    EnrichNpmProvenance {
        /// Fuseki SPARQL endpoint URL
        #[arg(long, required = true)]
        endpoint: String,

        /// Output N-Triples file
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Enrich RPM packages with Koji build metadata
    EnrichKoji {
        /// Fuseki SPARQL endpoint URL (not required when --srpm-list is provided)
        #[arg(long, default_value = "")]
        endpoint: String,

        /// Output N-Triples file
        #[arg(short, long, required = true)]
        output: String,

        /// Koji hub XML-RPC endpoint
        #[arg(long, default_value = "https://koji.fedoraproject.org/kojihub")]
        koji_hub: String,

        /// Distribution name
        #[arg(long, default_value = "fedora")]
        distro: String,

        /// Release name
        #[arg(long, default_value = "")]
        release: String,

        /// Cache directory
        #[arg(long)]
        cache_dir: Option<String>,

        /// Maximum number of packages to process (for testing)
        #[arg(long)]
        limit: Option<usize>,

        /// Path to text file with one SRPM NVR per line (bypasses Fuseki query)
        #[arg(long)]
        srpm_list: Option<String>,
    },

    /// Enrich package graph with cross-distribution equivalences from Repology
    EnrichRepology {
        /// Fuseki SPARQL endpoint URL
        #[arg(long, required = true)]
        endpoint: String,

        /// Output N-Triples file
        #[arg(short, long, required = true)]
        output: String,

        /// Cache directory
        #[arg(long)]
        cache_dir: Option<String>,
    },

    /// Enrich packages with OSV vulnerability data via per-package API queries
    EnrichSecurity {
        /// Fuseki SPARQL endpoint URL
        #[arg(long, required = true)]
        endpoint: String,

        /// Ecosystem to enrich
        #[arg(long, required = true, value_parser = ["deb", "apk", "rpm", "npm", "pypi", "cargo", "gomod", "maven", "debian", "alpine", "fedora"])]
        ecosystem: String,

        /// Output N-Triples file
        #[arg(short, long, required = true)]
        output: String,

        /// Cache directory
        #[arg(long)]
        cache_dir: Option<String>,
    },

    /// Enrich CVE entities with NVD canonical metadata (publishedDate, CVSS, CWE)
    EnrichNvd {
        /// Fuseki SPARQL endpoint URL for advisory-linked CVE discovery
        #[arg(long, required = true)]
        endpoint: String,

        /// Output N-Triples file (required for feed mode, unused for api mode)
        #[arg(short, long)]
        output: Option<String>,

        /// Enrichment mode: 'feed' (bulk download) or 'api' (per-CVE incremental)
        #[arg(long, default_value = "feed")]
        mode: String,

        /// Target graph URI for API mode (INSERT DATA destination)
        #[arg(long, default_value = "https://packagegraph.github.io/graph/cve/nvd")]
        graph: String,

        /// NVD API key for higher rate limits (optional, also via NVD_API_KEY env var)
        #[arg(long, env = "NVD_API_KEY")]
        nvd_api_key: Option<String>,

        /// Cache directory for NVD feed files (feed mode only, enables META-based conditional download)
        #[arg(long)]
        cache_dir: Option<String>,
    },

    /// Enrich forge instances with software version observations
    EnrichForgeVersion {
        /// Fuseki SPARQL endpoint URL
        #[arg(long, required = true)]
        endpoint: String,

        /// Output N-Triples file
        #[arg(short, long, required = true)]
        output: String,

        /// Cache directory
        #[arg(long)]
        cache_dir: Option<String>,

        /// GitLab API token for self-hosted instances requiring auth (also via GITLAB_TOKEN env var)
        #[arg(long, env = "GITLAB_TOKEN")]
        gitlab_token: Option<String>,
    },

    /// Enrich Maven repositories with release-to-release diffs
    EnrichDiff {
        /// Fuseki SPARQL endpoint URL
        #[arg(long, required = true)]
        endpoint: String,

        /// Output N-Triples file
        #[arg(short, long, required = true)]
        output: String,

        /// GitHub API token (or set GITHUB_TOKEN env var)
        #[arg(long, env = "GITHUB_TOKEN")]
        github_token: Option<String>,

        /// Cache directory
        #[arg(long)]
        cache_dir: Option<String>,
    },

    /// Enrich CVE entities with EPSS exploit prediction scores from FIRST.org
    EnrichEpss {
        /// Fuseki SPARQL endpoint URL
        #[arg(long, required = true)]
        endpoint: String,

        /// Output N-Triples file
        #[arg(short, long, required = true)]
        output: String,

        /// Minimum EPSS score threshold (skip near-zero CVEs)
        #[arg(long, default_value = "0.0")]
        min_score: f64,
    },

    /// Classify package identities using OSS Taxonomy (technology + role facets)
    EnrichTaxonomy {
        /// Fuseki SPARQL endpoint URL
        #[arg(long, required = true)]
        endpoint: String,

        /// Output N-Triples file
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Materialize reverse dependency counts on PackageIdentity entities
    EnrichRevdeps {
        /// Fuseki SPARQL endpoint URL
        #[arg(long, required = true)]
        endpoint: String,

        /// Output N-Triples file
        #[arg(short, long, required = true)]
        output: String,

        /// Scope to a specific named graph
        #[arg(long)]
        graph: Option<String>,
    },

    /// Compute blast radius scores for vulnerabilities (log10(revdeps) * CVSS)
    EnrichBlastRadius {
        /// Fuseki SPARQL endpoint URL
        #[arg(long, required = true)]
        endpoint: String,

        /// Output N-Triples file
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Derive pkg:lastReleaseDate from ecosystem build timestamps
    DerivePackageHistory {
        /// Fuseki SPARQL endpoint URL
        #[arg(long, required = true)]
        endpoint: String,

        /// Output N-Triples file
        #[arg(short, long, required = true)]
        output: String,

        /// Specific graph URIs to process (repeatable). If omitted, processes package-source graphs matching allowlist patterns.
        #[arg(long)]
        graph: Vec<String>,

        /// Load output into graph/derived/package-history after derivation
        #[arg(long)]
        load: bool,
    },

    /// Fetch upstream artifacts to source cache
    Fetch {
        /// Collector type (rpm, debian, alpine, arch)
        #[arg(long, required = true)]
        collector: String,

        /// Source artifact cache directory
        #[arg(long, required = true)]
        cache_dir: String,

        /// Repository URL or mirror
        #[arg(long, required = true)]
        url: String,

        /// Distribution name
        #[arg(long, required = true)]
        distro: String,

        /// Release identifier
        #[arg(long, required = true)]
        release: String,

        /// Architecture (for Debian/Alpine)
        #[arg(long)]
        arch: Option<String>,

        /// Repository/component name (for Debian/Alpine)
        #[arg(long)]
        repo: Option<String>,
    },

    /// Normalize cached artifacts to IR
    Normalize {
        /// Collector type (rpm, debian, alpine, arch)
        #[arg(long, required = true)]
        collector: String,

        /// Source artifact cache directory
        #[arg(long, required = true)]
        from_cache: String,

        /// IR output directory
        #[arg(long, required = true)]
        output_ir: String,
    },

    /// Emit N-Triples from a cached IR directory
    #[command(name = "emit")]
    EmitFromIr {
        /// IR directory to read from
        #[arg(long, required = true)]
        from_ir: String,

        /// Output N-Triples file
        #[arg(short, long, required = true)]
        output: String,

        /// Filter by collector (rpm, debian, etc.)
        #[arg(long)]
        collector: Option<String>,

        /// Filter by distro
        #[arg(long)]
        distro: Option<String>,

        /// Filter by release
        #[arg(long)]
        release: Option<String>,
    },

    /// Generate seed file of package names from a Fuseki graph
    Seed {
        /// Fuseki SPARQL endpoint URL
        #[arg(long, required = true)]
        endpoint: String,

        /// Named graph URI to extract package names from
        #[arg(long, required = true)]
        graph: String,

        /// Output file path (one package name per line)
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Consolidated multi-arch RPM collection with optional Koji + spec enrichment
    RpmFull {
        /// Repository URLs (one per arch, repeatable)
        #[arg(long = "url", required = true)]
        urls: Vec<String>,

        /// Distribution name
        #[arg(long, required = true)]
        distro: String,

        /// Release name
        #[arg(long, required = true)]
        release: String,

        /// Output N-Triples file
        #[arg(short, long, required = true)]
        output: String,

        /// Enable Koji build provenance enrichment
        #[arg(long)]
        with_koji: bool,

        /// Koji hub XML-RPC endpoint
        #[arg(long, default_value = "https://koji.fedoraproject.org/kojihub")]
        koji_hub: String,

        /// Enable spec file collection (Source0, commit, ecosystem)
        #[arg(long)]
        with_spec: bool,

        /// Enable BuildRequires dependency emission (requires --with-spec)
        #[arg(long)]
        with_buildrequires: bool,

        /// Enable maintainer extraction from changelog + Pagure API (requires --with-spec)
        #[arg(long)]
        with_maintainers: bool,

        /// Cache directory
        #[arg(long)]
        cache_dir: Option<String>,

        /// Minio endpoint for durable cache sync (survives pod restarts)
        #[arg(long, env = "MINIO_ENDPOINT")]
        minio_endpoint: Option<String>,

        /// Minio bucket for cache sync
        #[arg(long, env = "MINIO_BUCKET", default_value = "packagegraph")]
        minio_bucket: String,

        /// Minio access key
        #[arg(long, env = "MINIO_ACCESS_KEY")]
        minio_access_key: Option<String>,

        /// Minio secret key
        #[arg(long, env = "MINIO_SECRET_KEY")]
        minio_secret_key: Option<String>,

        /// Maximum packages to process per stage (for testing)
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Consolidated multi-arch Debian collection with inline enrichment
    #[command(name = "deb-full")]
    DebFull {
        /// Repository URL
        #[arg(long, required = true)]
        repo: String,

        /// Distro identifier (debian, ubuntu, mint)
        #[arg(long, default_value = "debian")]
        distro: String,

        /// Distribution codename
        #[arg(long, required = true)]
        dist: String,

        /// Component name
        #[arg(long, default_value = "main")]
        component: String,

        /// Architecture(s) to collect (e.g., binary-amd64, or bare amd64 with auto-normalization)
        #[arg(long, required = true)]
        arch: Vec<String>,

        /// Output N-Triples file
        #[arg(short, long, required = true)]
        output: String,

        /// Parse Sources.gz for Build-Depends + Uploaders
        #[arg(long)]
        with_sources: bool,

        /// Emit buildDependsOn triples (requires --with-sources)
        #[arg(long)]
        with_builddeps: bool,

        /// Emit co-maintainer triples from Uploaders
        #[arg(long)]
        with_maintainers: bool,

        /// Fetch debian/ files from salsa.debian.org
        #[arg(long)]
        with_salsa: bool,

        /// Cache directory
        #[arg(long)]
        cache_dir: Option<String>,

        /// Maximum packages to process per stage (for testing)
        #[arg(long)]
        limit: Option<usize>,
    },
}

fn main() {
    let cli = Cli::parse();

    let auth: SparqlAuth = match (&cli.sparql_username, &cli.sparql_password) {
        (Some(u), Some(p)) => Some((u.clone(), p.clone())),
        _ => None,
    };

    let backend_type = cli.sparql_backend.clone();
    let backend_token = cli.qlever_access_token.clone();
    let write_backend = cli.write_backend.clone();
    let minio_endpoint = cli.minio_endpoint.clone();
    let minio_bucket = cli.minio_bucket.clone();
    let graph_uri: Option<String> = cli.graph.clone();

    let make_backend = || -> SparqlBackend {
        match backend_type.as_str() {
            "fuseki" => SparqlBackend::Fuseki,
            "qlever" => {
                let token = backend_token.clone().unwrap_or_else(|| {
                    eprintln!("ERROR: --qlever-access-token required when --sparql-backend=qlever");
                    std::process::exit(1);
                });
                SparqlBackend::QLever {
                    access_token: token,
                }
            }
            other => {
                eprintln!(
                    "ERROR: unknown --sparql-backend '{}' (expected 'fuseki' or 'qlever')",
                    other
                );
                std::process::exit(1);
            }
        }
    };

    if !["fuseki", "minio"].contains(&write_backend.as_str()) {
        eprintln!(
            "ERROR: unknown --write-backend '{}' (expected 'fuseki' or 'minio')",
            write_backend
        );
        std::process::exit(1);
    }

    let start = Instant::now();

    let result = match cli.command {
        Commands::Debian {
            repo,
            distro,
            dist,
            component,
            arch,
            output,
            workers: _,
            cache_dir,
        } => {
            eprintln!("=== PackageGraph Debian Collector ===");
            eprintln!("Distro: {}", distro);
            eprintln!("Repository: {}", repo);
            eprintln!("Distribution: {}", dist);
            eprintln!("Component: {}", component);
            eprintln!("Architectures: {:?}", arch);
            if let Some(ref cd) = cache_dir {
                eprintln!("Cache: {}", cd);
            }
            eprintln!("Output: {}", output);
            eprintln!();

            let collector =
                DebianCollector::new(repo, distro, dist, component).with_graph(graph_uri.clone());
            let collector = match cache_dir {
                Some(ref cd) => collector.with_cache(cd).expect("Failed to create cache"),
                None => collector,
            };
            collector.collect(&arch, &output)
        }

        Commands::Rpm {
            repo,
            rpm_repos,
            distro_name,
            release_name,
            output,
            sslclientcert,
            sslclientkey,
            sslcacert,
            repo_type,
            cache_dir,
        } => {
            eprintln!("=== PackageGraph RPM Collector ===");

            let use_tls = sslclientcert.is_some();
            if use_tls {
                eprintln!(
                    "TLS client cert: {}",
                    sslclientcert.as_deref().unwrap_or("")
                );
            }

            // Helper to create collector with or without TLS
            let make_collector = |url: String, distro: String, release: String| -> RpmCollector {
                let c = if let (Some(cert), Some(key), Some(ca)) =
                    (&sslclientcert, &sslclientkey, &sslcacert)
                {
                    RpmCollector::new_with_tls_and_repo_type(
                        url,
                        distro,
                        release,
                        cert,
                        key,
                        ca,
                        repo_type.clone(),
                    )
                } else {
                    RpmCollector::new_with_repo_type(url, distro, release, repo_type.clone())
                };
                c.with_graph(graph_uri.clone())
            };

            if let Some(url) = repo {
                // Single --repo mode
                eprintln!("Repository: {}", url);
                eprintln!("Distribution: {}", distro_name);
                eprintln!("Release: {}", release_name);
                if let Some(ref cd) = cache_dir {
                    eprintln!("Cache: {}", cd);
                }
                eprintln!("Output: {}", output);
                eprintln!();
                let collector = make_collector(url, distro_name, release_name);
                let collector = match cache_dir {
                    Some(ref cd) => collector.with_cache(cd).expect("Failed to create cache"),
                    None => collector,
                };
                collector.collect(&output)
            } else if !rpm_repos.is_empty() {
                // Multi --rpm-repo mode: iterate ALL specs
                let mut total_packages = 0;
                let mut total_triples = 0;

                for (idx, repo_spec) in rpm_repos.iter().enumerate() {
                    let parts: Vec<&str> = repo_spec.splitn(3, ':').collect();
                    if parts.len() < 3 {
                        eprintln!(
                            "Error: --rpm-repo format is name:release:url, got: {}",
                            repo_spec
                        );
                        std::process::exit(1);
                    }
                    let rpm_distro = parts[0];
                    let rpm_release = parts[1];
                    let rpm_url = parts[2];

                    eprintln!(
                        "\n--- [{}/{}] {}/{} ---",
                        idx + 1,
                        rpm_repos.len(),
                        rpm_distro,
                        rpm_release
                    );
                    eprintln!("Repository: {}", rpm_url);

                    // Each repo gets its own output file
                    let repo_output = if rpm_repos.len() == 1 {
                        output.clone()
                    } else {
                        let base = output.trim_end_matches(".nt");
                        format!("{}-{}-{}.nt", base, rpm_distro, rpm_release)
                    };

                    let collector = make_collector(
                        rpm_url.to_string(),
                        rpm_distro.to_string(),
                        rpm_release.to_string(),
                    );
                    let collector = match cache_dir {
                        Some(ref cd) => collector.with_cache(cd).expect("Failed to create cache"),
                        None => collector,
                    };
                    match collector.collect(&repo_output) {
                        Ok((pkgs, triples)) => {
                            total_packages += pkgs;
                            total_triples += triples;
                        }
                        Err(e) => {
                            eprintln!("Error collecting {}/{}: {}", rpm_distro, rpm_release, e);
                            // Continue with other repos
                        }
                    }
                }
                Ok((total_packages, total_triples))
            } else {
                eprintln!("Error: Either --repo or --rpm-repo must be specified");
                std::process::exit(1);
            }
        }

        Commands::Alpine {
            mirror,
            distro,
            branch,
            repos,
            arch,
            output,
        } => {
            eprintln!("=== PackageGraph Alpine Collector ===");
            eprintln!("Distro: {}", distro);
            eprintln!("Mirror: {}", mirror);
            eprintln!("Branch: {}", branch);
            eprintln!("Repos: {:?}", repos);
            eprintln!("Arch: {}", arch);
            eprintln!("Output: {}", output);
            eprintln!();

            let collector = AlpineCollector::new(mirror, distro, branch, repos, arch)
                .with_graph(graph_uri.clone());
            collector.collect(&output)
        }

        Commands::Homebrew {
            api_base,
            distro,
            release,
            output,
        } => {
            eprintln!("=== PackageGraph Homebrew Collector ===");
            eprintln!("Distro: {} / Release: {}", distro, release);
            eprintln!("API: {}", api_base);
            eprintln!("Output: {}", output);
            eprintln!();

            let collector =
                HomebrewCollector::new(api_base, distro, release).with_graph(graph_uri.clone());
            collector.collect(&output)
        }

        Commands::Arch {
            mirror,
            distro,
            release,
            arch,
            repos,
            include_aur,
            output,
        } => {
            eprintln!("=== PackageGraph Arch Linux Collector ===");
            eprintln!("Distro: {} / Release: {} / Arch: {}", distro, release, arch);
            eprintln!("Mirror: {}", mirror);
            eprintln!("Repos: {:?}", repos);
            eprintln!("Include AUR: {}", include_aur);
            eprintln!("Output: {}", output);
            eprintln!();

            let collector = ArchCollector::new(mirror, distro, release, arch, repos, include_aur)
                .with_graph(graph_uri.clone());
            collector.collect(&output)
        }

        Commands::Npm {
            packages_file,
            endpoint,
            output,
        } => {
            eprintln!("=== PackageGraph NPM Collector ===");
            eprintln!("Output: {}", output);
            eprintln!();

            let collector = pg_collect::npm::NpmCollector::new("https://registry.npmjs.org".into())
                .with_graph(graph_uri.clone());
            if let Some(ref seed) = packages_file {
                eprintln!("Seed: {}", seed);
                collector.collect(seed, &output)
            } else if let Some(ref ep) = endpoint {
                eprintln!("Mode: auto-discover from Fuseki at {}", ep);
                collector.collect_discover(ep, &auth, make_backend(), &output)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Either --packages-file or --endpoint required",
                ))
            }
        }

        Commands::Pypi {
            packages_file,
            endpoint,
            max_depth,
            max_packages,
            output,
            cache_dir,
            cache_ttl_hours,
        } => {
            eprintln!("=== PackageGraph PyPI Collector ===");
            eprintln!(
                "Spider: max_depth={}, max_packages={}",
                max_depth, max_packages
            );
            eprintln!("Output: {}", output);
            if let Some(ref cd) = cache_dir {
                eprintln!("Cache: {} (TTL={}h)", cd, cache_ttl_hours);
            }
            eprintln!();

            let collector = pg_collect::pypi::PypiCollector::new()
                .with_cache_ttl_hours(cache_ttl_hours)
                .with_graph(graph_uri.clone());
            let collector = match cache_dir {
                Some(ref cd) => match collector.with_cache(cd) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!(
                            "WARNING: cache init failed for {}: {}, proceeding without cache",
                            cd, e
                        );
                        pg_collect::pypi::PypiCollector::new()
                            .with_cache_ttl_hours(cache_ttl_hours)
                            .with_graph(graph_uri.clone())
                    }
                },
                None => collector,
            };
            if let Some(ref seed) = packages_file {
                eprintln!("Seed: {}", seed);
                collector.collect(seed, max_depth, max_packages, &output)
            } else if let Some(ref ep) = endpoint {
                eprintln!("Mode: auto-discover from Fuseki at {}", ep);
                collector.collect_discover(
                    ep,
                    &auth,
                    make_backend(),
                    max_depth,
                    max_packages,
                    &output,
                )
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Either --packages-file or --endpoint required",
                ))
            }
        }

        Commands::Cargo {
            packages_file,
            endpoint,
            max_depth,
            max_packages,
            output,
        } => {
            eprintln!("=== PackageGraph Cargo Collector ===");
            eprintln!(
                "Spider: max_depth={}, max_packages={}",
                max_depth, max_packages
            );
            eprintln!("Output: {}", output);
            eprintln!();

            let collector =
                pg_collect::cargo_collect::CargoCollector::new().with_graph(graph_uri.clone());
            if let Some(ref seed) = packages_file {
                eprintln!("Seed: {}", seed);
                collector.collect(seed, max_depth, max_packages, &output)
            } else if let Some(ref ep) = endpoint {
                eprintln!("Mode: auto-discover from Fuseki at {}", ep);
                collector.collect_discover(
                    ep,
                    &auth,
                    make_backend(),
                    max_depth,
                    max_packages,
                    &output,
                )
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Either --packages-file or --endpoint required",
                ))
            }
        }

        Commands::Gomod {
            packages_file,
            endpoint,
            proxy,
            max_depth,
            max_packages,
            output,
        } => {
            eprintln!("=== PackageGraph Go Modules Collector ===");
            eprintln!("Proxy: {}", proxy);
            eprintln!(
                "Spider: max_depth={}, max_packages={}",
                max_depth, max_packages
            );
            eprintln!("Output: {}", output);
            eprintln!();

            let collector =
                pg_collect::gomod::GoModCollector::new(proxy).with_graph(graph_uri.clone());
            if let Some(ref seed) = packages_file {
                eprintln!("Seed: {}", seed);
                collector.collect(seed, max_depth, max_packages, &output)
            } else if let Some(ref ep) = endpoint {
                eprintln!("Mode: auto-discover from Fuseki at {}", ep);
                collector.collect_discover(
                    ep,
                    &auth,
                    make_backend(),
                    max_depth,
                    max_packages,
                    &output,
                )
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Either --packages-file or --endpoint required",
                ))
            }
        }

        Commands::Conda {
            packages_file,
            distro,
            release,
            channel_url,
            subdir,
            output,
        } => {
            eprintln!("=== PackageGraph Conda Collector ===");
            eprintln!("Channel: {}", channel_url);
            eprintln!("Subdir: {}", subdir);
            if let Some(ref seed) = packages_file {
                eprintln!("Seed: {}", seed);
            } else {
                eprintln!("Mode: full collection");
            }
            eprintln!("Output: {}", output);
            eprintln!();

            let collector =
                pg_collect::conda::CondaCollector::new(distro, release, channel_url, subdir)
                    .with_graph(graph_uri.clone());
            if let Some(seed) = packages_file {
                collector.collect_seeded(&seed, &output)
            } else {
                collector.collect_full(&output)
            }
        }

        Commands::Flatpak {
            packages_file,
            distro,
            release,
            output,
        } => {
            eprintln!("=== PackageGraph Flatpak Collector ===");
            eprintln!("Output: {}", output);
            eprintln!();

            let collector = pg_collect::flatpak::FlatpakCollector::new(distro, release)
                .with_graph(graph_uri.clone());
            if let Some(ref seed) = packages_file {
                eprintln!("Seed: {}", seed);
                collector.collect(seed, &output)
            } else {
                eprintln!("Mode: auto-discover from Flathub");
                collector.collect_discover(&output)
            }
        }

        Commands::Snap {
            packages_file,
            distro,
            release,
            output,
        } => {
            eprintln!("=== PackageGraph Snap Collector ===");
            eprintln!("Output: {}", output);
            eprintln!();

            let collector =
                pg_collect::snap::SnapCollector::new(distro, release).with_graph(graph_uri.clone());
            if let Some(ref seed) = packages_file {
                eprintln!("Seed: {}", seed);
                collector.collect(seed, &output)
            } else {
                eprintln!("Mode: auto-discover from Snap Store");
                collector.collect_discover(&output)
            }
        }

        Commands::Gentoo {
            repo_path,
            distro,
            release,
            output,
        } => {
            eprintln!("=== PackageGraph Gentoo Collector ===");
            eprintln!("Repo: {}", repo_path);
            eprintln!("Output: {}", output);
            eprintln!();

            let collector = pg_collect::gentoo::GentooCollector::new(distro, release, repo_path)
                .with_graph(graph_uri.clone());
            collector.collect(&output)
        }

        Commands::Void {
            repo_path,
            distro,
            release,
            output,
        } => {
            eprintln!("=== PackageGraph Void Collector ===");
            eprintln!("Repo: {}", repo_path);
            eprintln!("Output: {}", output);
            eprintln!();

            let collector =
                pg_collect::void_collect::VoidCollector::new(distro, release, repo_path)
                    .with_graph(graph_uri.clone());
            collector.collect(&output)
        }

        Commands::Yocto {
            layer,
            distro,
            release,
            output,
        } => {
            eprintln!("=== PackageGraph Yocto Collector ===");
            eprintln!("Layers: {}", layer.join(", "));
            eprintln!("Output: {}", output);
            eprintln!();

            let collector =
                YoctoCollector::new(distro, release, layer).with_graph(graph_uri.clone());
            collector.collect(&output)
        }

        Commands::Buildroot {
            repo_path,
            distro,
            release,
            output,
        } => {
            eprintln!("=== PackageGraph Buildroot Collector ===");
            eprintln!("Repo: {}", repo_path);
            eprintln!("Output: {}", output);
            eprintln!();

            let collector =
                BuildrootCollector::new(distro, release, repo_path).with_graph(graph_uri.clone());
            collector.collect(&output)
        }

        Commands::Openwrt {
            feed_path,
            distro,
            release,
            output,
        } => {
            eprintln!("=== PackageGraph OpenWRT Collector ===");
            eprintln!("Feed: {}", feed_path);
            eprintln!("Output: {}", output);
            eprintln!();

            let collector =
                OpenWrtCollector::new(distro, release, feed_path).with_graph(graph_uri.clone());
            collector.collect(&output)
        }

        Commands::OpenwrtFull {
            feeds,
            distro,
            release,
            output,
            release_url,
            arch,
            with_upstream,
            with_attestation,
            github_token,
            cache_dir,
            minio_endpoint,
            minio_bucket,
            minio_access_key,
            minio_secret_key,
            limit,
        } => {
            eprintln!("=== PackageGraph OpenWRT Full Collector ===");
            eprintln!("Release: {}", release);
            eprintln!("Feeds: {}", feeds.len());
            eprintln!("Output: {}", output);
            if let Some(ref url) = release_url {
                eprintln!("Release URL: {}", url);
                eprintln!(
                    "Arch: {}",
                    arch.as_ref().unwrap_or(&"<missing>".to_string())
                );
            }
            eprintln!();

            // Validation
            if release_url.is_some() && arch.is_none() {
                eprintln!("Error: --release-url requires --arch");
                std::process::exit(1);
            }

            if with_attestation && release_url.is_none() {
                eprintln!("Error: --with-attestation requires --release-url (needs digest_map from Stage 2)");
                std::process::exit(1);
            }

            if with_attestation && github_token.is_none() {
                eprintln!("Warning: --github-token not provided. GitHub API rate limit: 60/hr (vs 5000/hr with token)");
            }

            (|| -> std::io::Result<(usize, usize)> {
                use pg_collect::collect_openwrt_upstream::OpenwrtUpstreamCollector;
                use pg_collect::collect_opkg_index::OpkgIndexCollector;
                use pg_collect::enrich_openwrt_attestation::OpenwrtAttestationEnricher;
                use pg_collect::openwrt::OpenWrtCollector;
                use std::collections::{HashMap, HashSet};

                let file = std::fs::File::create(&output)?;
                let mut writer = pg_collect::ntriples::NTriplesWriter::new_maybe_graph(
                    file,
                    graph_uri.as_deref(),
                );

                // Emit distribution metadata (shared across all stages)
                let temp_collector =
                    OpenWrtCollector::new(distro.clone(), release.clone(), feeds[0].clone());
                temp_collector.emit_distribution_metadata(&mut writer)?;

                // Shared dedup and maps
                let mut seen = HashSet::new();
                let mut identity_map = HashMap::new();
                let mut parsed_meta = HashMap::new();
                let mut parent_map = HashMap::new();

                let mut total_packages = 0;
                let mut total_triples = 0;

                // Stage 1: Multi-feed collection
                eprintln!("--- Stage 1: Source Package Collection ---");
                for (i, feed_path) in feeds.iter().enumerate() {
                    let is_secondary = i > 0;
                    let collector =
                        OpenWrtCollector::new(distro.clone(), release.clone(), feed_path.clone());

                    eprintln!("Feed {} of {}: {}", i + 1, feeds.len(), feed_path);

                    let (pkgs, triples) = collector.collect_with_writer(
                        &mut writer,
                        &mut seen,
                        &mut identity_map,
                        &mut parsed_meta,
                        &mut parent_map,
                        is_secondary,
                    )?;

                    total_packages += pkgs;
                    total_triples += triples;
                    eprintln!("  {} packages, {} triples", pkgs, triples);
                }

                eprintln!(
                    "Stage 1 complete: {} total packages, {} triples",
                    total_packages, total_triples
                );

                // Stage 2: Binary package index (if --release-url provided)
                if let (Some(url), Some(a)) = (&release_url, &arch) {
                    eprintln!("\n--- Stage 2: Binary Package Index ---");
                    let index_collector = OpkgIndexCollector::new(
                        distro.clone(),
                        release.clone(),
                        url.clone(),
                        a.clone(),
                    );
                    let (bin_count, digest_map) =
                        index_collector.collect(&mut writer, &identity_map, None)?;
                    total_packages += bin_count; // Count binary packages too
                    eprintln!(
                        "  {} binary packages, {} digests",
                        bin_count,
                        digest_map.len()
                    );
                    total_triples += bin_count * 10; // Estimate (updated for dependencies)

                    // Stage 4: Attestation enrichment (if --with-attestation)
                    if with_attestation {
                        eprintln!("\n--- Stage 4: SLSA Attestation Enrichment ---");

                        let minio_cfg = if minio_endpoint.is_some()
                            && minio_access_key.is_some()
                            && minio_secret_key.is_some()
                        {
                            Some(pg_collect::cache::MinioConfig {
                                endpoint: minio_endpoint.clone().unwrap(),
                                bucket: minio_bucket.clone(),
                                access_key: minio_access_key.clone().unwrap(),
                                secret_key: minio_secret_key.clone().unwrap(),
                            })
                        } else {
                            None
                        };

                        let enricher = OpenwrtAttestationEnricher::new(
                            github_token.clone(),
                            cache_dir.as_deref(),
                            minio_cfg,
                        )?;

                        let att_triples = enricher.enrich(&mut writer, &digest_map)?;
                        total_triples += att_triples;
                        eprintln!("  {} attestation triples", att_triples);
                    }
                }

                // Stage 3: Upstream source enrichment (if --with-upstream)
                if with_upstream {
                    eprintln!("\n--- Stage 3: Upstream Source Enrichment ---");
                    let upstream_collector =
                        OpenwrtUpstreamCollector::new(distro.clone(), release.clone());
                    let upstream_triples = upstream_collector.collect(
                        &mut writer,
                        &identity_map,
                        &parsed_meta,
                        &parent_map,
                    )?;
                    total_triples += upstream_triples;
                    eprintln!("  {} upstream triples", upstream_triples);
                }

                writer.flush()?;
                eprintln!("\n=== Complete ===");
                eprintln!(
                    "Total: {} packages, {} triples",
                    total_packages, total_triples
                );

                Ok((total_packages, total_triples))
            })()
        }

        Commands::Osv { ecosystem, output } => {
            eprintln!("=== PackageGraph OSV Security Collector ===");
            eprintln!("Ecosystem: {}", ecosystem);
            eprintln!("Output: {}", output);
            eprintln!();

            let collector = pg_collect::osv::OsvCollector::new().with_graph(graph_uri.clone());
            collector.collect(&ecosystem, &output)
        }

        Commands::CollectBodhi {
            endpoint,
            release,
            output,
            since,
            cache_dir,
        } => {
            eprintln!("=== PackageGraph Bodhi Advisory Collector ===");
            eprintln!("Endpoint: {}", endpoint);
            eprintln!("Release: {}", release);
            if let Some(ref s) = since {
                eprintln!("Since: {}", s);
            }
            if let Some(ref cd) = cache_dir {
                eprintln!("Cache: {}", cd);
            }
            eprintln!("Output: {}", output);
            eprintln!();

            match BodhiCollector::new(
                &endpoint,
                release,
                since,
                cache_dir.as_deref(),
                auth.clone(),
                make_backend(),
            ) {
                Ok(collector) => collector.with_graph(graph_uri.clone()).collect(&output),
                Err(e) => Err(e),
            }
        }

        Commands::CollectGlsa {
            endpoint,
            output,
            since,
            cache_dir,
        } => {
            eprintln!("=== PackageGraph GLSA Advisory Collector ===");
            eprintln!("Endpoint: {}", endpoint);
            if let Some(ref s) = since {
                eprintln!("Since: {}", s);
            }
            if let Some(ref cd) = cache_dir {
                eprintln!("Cache: {}", cd);
            }
            eprintln!("Output: {}", output);
            eprintln!();

            match GlsaCollector::new(
                &endpoint,
                since,
                cache_dir.as_deref(),
                auth.clone(),
                make_backend(),
            ) {
                Ok(collector) => collector.with_graph(graph_uri.clone()).collect(&output),
                Err(e) => Err(e),
            }
        }

        Commands::Rubygems {
            packages_file,
            endpoint,
            api_base,
            output,
        } => {
            eprintln!("=== PackageGraph RubyGems Collector ===");
            eprintln!("API: {}", api_base);
            eprintln!("Output: {}", output);
            eprintln!();

            let collector = RubyGemsCollector::new(api_base).with_graph(graph_uri.clone());
            if let Some(ref seed) = packages_file {
                eprintln!("Seed: {}", seed);
                collector.collect(seed, &output)
            } else if let Some(ref ep) = endpoint {
                eprintln!("Mode: auto-discover from Fuseki at {}", ep);
                collector.collect_discover(ep, &auth, make_backend(), &output)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Either --packages-file or --endpoint is required for rubygems",
                ))
            }
        }

        Commands::Maven {
            packages_file,
            endpoint,
            search_base,
            repo_base,
            output,
            cache_dir,
            cache_refresh,
            max_depth,
            max_roots,
            max_packages,
            delay_ms,
        } => {
            eprintln!("=== PackageGraph Maven Collector ===");
            eprintln!("Output: {}", output);
            eprintln!();

            if max_packages == 0 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "max-packages must be > 0",
                ))
            } else if max_roots == 0 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "max-roots must be > 0",
                ))
            } else if !pg_collect::maven::is_maven_central(&repo_base) {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("Only Maven Central is supported. Got: {}", repo_base),
                ))
            } else {
                let http_cache =
                    cache_dir.as_ref().and_then(
                        |dir| match pg_collect::http_cache::HttpCache::new(dir, "maven") {
                            Ok(c) => {
                                eprintln!("Cache: {} (refresh={})", dir, cache_refresh);
                                Some(c)
                            }
                            Err(e) => {
                                eprintln!(
                                "WARNING: cache init failed for {}: {}, proceeding without cache",
                                dir, e
                            );
                                None
                            }
                        },
                    );
                let mut collector = MavenCollector::new(search_base, repo_base);
                if let Some(cache) = http_cache {
                    collector.set_cache(cache);
                }
                let mut collector = collector
                    .with_refresh(cache_refresh)
                    .with_graph(graph_uri.clone());
                collector.max_depth = max_depth;
                collector.max_roots = max_roots;
                collector.max_packages = max_packages;
                collector.delay_ms = delay_ms;

                if max_depth > 0 {
                    eprintln!(
                        "Traversal: depth={}, max_roots={}, max_packages={}, delay={}ms",
                        max_depth, max_roots, max_packages, delay_ms
                    );
                }

                // All modes route through collect_recursive
                if let Some(ref seed) = packages_file {
                    eprintln!("Seed: {}", seed);
                    collector.collect(seed, &output)
                } else if let Some(ref ep) = endpoint {
                    eprintln!("Mode: auto-discover from Fuseki at {}", ep);
                    collector.collect_discover(ep, &auth, make_backend(), &output)
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "Either --packages-file or --endpoint required",
                    ))
                }
            }
        }

        Commands::Cpan {
            packages_file,
            endpoint,
            api_base,
            output,
        } => {
            eprintln!("=== PackageGraph CPAN Collector ===");
            eprintln!("Output: {}", output);
            eprintln!();

            let collector = CpanCollector::new(api_base).with_graph(graph_uri.clone());
            if let Some(ref seed) = packages_file {
                eprintln!("Seed: {}", seed);
                collector.collect(seed, &output)
            } else if let Some(ref ep) = endpoint {
                eprintln!("Mode: auto-discover from Fuseki at {}", ep);
                collector.collect_discover(ep, &auth, make_backend(), &output)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Either --packages-file or --endpoint required",
                ))
            }
        }

        Commands::Cran { mirror, output } => {
            eprintln!("=== PackageGraph CRAN Collector ===");
            eprintln!("Mirror: {}", mirror);
            eprintln!("Output: {}", output);
            eprintln!();

            let collector = CranCollector::new(mirror).with_graph(graph_uri.clone());
            collector.collect(&output)
        }

        Commands::Hackage {
            packages_file,
            endpoint,
            base_url,
            output,
        } => {
            eprintln!("=== PackageGraph Hackage Collector ===");
            eprintln!("Output: {}", output);
            eprintln!();

            let collector = HackageCollector::new(base_url).with_graph(graph_uri.clone());
            if let Some(ref seed) = packages_file {
                eprintln!("Seed: {}", seed);
                collector.collect(seed, &output)
            } else if let Some(ref ep) = endpoint {
                eprintln!("Mode: auto-discover from Fuseki at {}", ep);
                collector.collect_discover(ep, &auth, make_backend(), &output)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Either --packages-file or --endpoint required",
                ))
            }
        }

        Commands::Nuget {
            packages_file,
            endpoint,
            service_index,
            output,
        } => {
            eprintln!("=== PackageGraph NuGet Collector ===");
            eprintln!("Output: {}", output);
            eprintln!();

            match NugetCollector::new_from_service_index(&service_index) {
                Ok(collector) => {
                    let collector = collector.with_graph(graph_uri.clone());
                    if let Some(ref seed) = packages_file {
                        eprintln!("Seed: {}", seed);
                        collector.collect(seed, &output)
                    } else if let Some(ref ep) = endpoint {
                        eprintln!("Mode: auto-discover from Fuseki at {}", ep);
                        collector.collect_discover(ep, &auth, make_backend(), &output)
                    } else {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "Either --packages-file or --endpoint required",
                        ))
                    }
                }
                Err(e) => Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
            }
        }

        Commands::Hex {
            packages_file,
            endpoint,
            api_base,
            output,
        } => {
            eprintln!("=== PackageGraph Hex Collector ===");
            eprintln!("Output: {}", output);
            eprintln!();

            let collector = HexCollector::new(api_base).with_graph(graph_uri.clone());
            if let Some(ref seed) = packages_file {
                eprintln!("Seed: {}", seed);
                collector.collect(seed, &output)
            } else if let Some(ref ep) = endpoint {
                eprintln!("Mode: auto-discover from Fuseki at {}", ep);
                collector.collect_discover(ep, &auth, make_backend(), &output)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Either --packages-file or --endpoint required",
                ))
            }
        }

        Commands::Freebsd {
            mirror,
            distro,
            release,
            arch,
            output,
        } => {
            eprintln!("=== PackageGraph FreeBSD Collector ===");
            eprintln!("Mirror: {}", mirror);
            eprintln!("Release: {}", release);
            eprintln!("Arch: {}", arch);
            eprintln!("Output: {}", output);
            eprintln!();

            let collector =
                FreebsdCollector::new(distro, mirror, release, arch).with_graph(graph_uri.clone());
            collector.collect(&output)
        }

        Commands::Nix {
            channel_url,
            distro,
            release,
            output,
        } => {
            eprintln!("=== PackageGraph Nix Collector ===");
            eprintln!("Channel: {}", channel_url);
            eprintln!("Output: {}", output);
            eprintln!();

            let collector =
                NixCollector::new(distro, release, channel_url).with_graph(graph_uri.clone());
            collector.collect(&output)
        }

        Commands::Chocolatey {
            api_url,
            distro,
            release,
            output,
        } => {
            eprintln!("=== PackageGraph Chocolatey Collector ===");
            eprintln!("API: {}", api_url);
            eprintln!("Output: {}", output);
            eprintln!();

            let collector =
                ChocolateyCollector::new(distro, release, api_url).with_graph(graph_uri.clone());
            collector.collect(&output)
        }

        Commands::Load {
            file,
            graph,
            endpoint,
            batch_size,
        } => {
            if write_backend == "minio" {
                (|| -> std::io::Result<(usize, usize)> {
                    eprintln!("=== PackageGraph Minio Loader ===");
                    eprintln!("File: {}", file);
                    eprintln!("Graph: {}", graph);
                    eprintln!("Bucket: {}", minio_bucket);
                    eprintln!();

                    let filename = std::path::Path::new(&file)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&file);
                    let nt_path = format!("pgraph/{}/nt-output/{}", minio_bucket, filename);
                    let graph_path =
                        format!("pgraph/{}/nt-output/{}.graph", minio_bucket, filename);

                    let mc = |args: &[&str]| -> std::io::Result<String> {
                        let output = std::process::Command::new("mc")
                            .args(args)
                            .output()
                            .map_err(|e| {
                                std::io::Error::new(
                                    std::io::ErrorKind::Other,
                                    format!("mc failed: {}", e),
                                )
                            })?;
                        if !output.status.success() {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::Other,
                                format!("mc error: {}", stderr),
                            ));
                        }
                        Ok(String::from_utf8_lossy(&output.stdout).to_string())
                    };

                    let minio_ep = minio_endpoint.as_deref().ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "--minio-endpoint is required for --write-backend=minio",
                        )
                    })?;
                    let minio_ak = std::env::var("MINIO_ACCESS_KEY").map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "MINIO_ACCESS_KEY env var is required for --write-backend=minio",
                        )
                    })?;
                    let minio_sk = std::env::var("MINIO_SECRET_KEY").map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "MINIO_SECRET_KEY env var is required for --write-backend=minio",
                        )
                    })?;
                    mc(&[
                        "alias", "set", "pgraph", minio_ep, &minio_ak, &minio_sk, "--api", "S3v4",
                    ])?;

                    eprintln!("Uploading {} → {}...", file, nt_path);
                    mc(&["cp", &file, &nt_path])?;

                    let tmp_graph = std::env::temp_dir().join(format!("{}.graph", filename));
                    std::fs::write(&tmp_graph, &graph)?;
                    mc(&["cp", tmp_graph.to_str().unwrap(), &graph_path])?;
                    std::fs::remove_file(&tmp_graph).ok();

                    let count = pg_collect::sparql::count_triples_pub(&file)?;
                    eprintln!("✓ Loaded {} triples to Minio ({})", count, nt_path);
                    Ok((count, count))
                })()
            } else {
                eprintln!("=== PackageGraph SPARQL Loader ===");
                eprintln!("File: {}", file);
                eprintln!("Graph: {}", graph);
                eprintln!("Endpoint: {}", endpoint);
                eprintln!("Batch size: {}", batch_size);
                eprintln!();

                let client =
                    pg_collect::sparql::make_sparql_client(&endpoint, &auth, make_backend());
                client
                    .load_file(&file, &graph, batch_size)
                    .map(|count| (count, count))
            }
        }

        Commands::Drop { graph, endpoint } => {
            if write_backend == "minio" {
                (|| -> std::io::Result<(usize, usize)> {
                    eprintln!("=== PackageGraph Minio Graph Drop ===");
                    eprintln!("Graph: {}", graph);
                    eprintln!("Bucket: {}", minio_bucket);
                    eprintln!();

                    let nt_dir = format!("pgraph/{}/nt-output/", minio_bucket);

                    let mc = |args: &[&str]| -> std::io::Result<String> {
                        let output = std::process::Command::new("mc")
                            .args(args)
                            .output()
                            .map_err(|e| {
                                std::io::Error::new(
                                    std::io::ErrorKind::Other,
                                    format!("mc failed: {}", e),
                                )
                            })?;
                        if !output.status.success() {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::Other,
                                format!("mc error: {}", stderr),
                            ));
                        }
                        Ok(String::from_utf8_lossy(&output.stdout).to_string())
                    };

                    let minio_ep = minio_endpoint.as_deref().ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "--minio-endpoint is required for --write-backend=minio",
                        )
                    })?;
                    let minio_ak = std::env::var("MINIO_ACCESS_KEY").map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "MINIO_ACCESS_KEY env var is required for --write-backend=minio",
                        )
                    })?;
                    let minio_sk = std::env::var("MINIO_SECRET_KEY").map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "MINIO_SECRET_KEY env var is required for --write-backend=minio",
                        )
                    })?;
                    mc(&[
                        "alias", "set", "pgraph", minio_ep, &minio_ak, &minio_sk, "--api", "S3v4",
                    ])?;

                    let listing = mc(&["ls", &nt_dir])?;
                    let mut removed = 0;
                    let mut errors = 0;
                    for line in listing.lines() {
                        let name = line.split_whitespace().last().unwrap_or("");
                        if !name.ends_with(".graph") {
                            continue;
                        }
                        let graph_file = format!("{}{}", nt_dir, name);
                        let stored_uri = match mc(&["cat", &graph_file]) {
                            Ok(uri) => uri,
                            Err(e) => {
                                eprintln!("WARNING: failed to read {}: {}", graph_file, e);
                                errors += 1;
                                continue;
                            }
                        };
                        if stored_uri.trim() == graph {
                            let nt_file = graph_file.trim_end_matches(".graph");
                            eprintln!("Removing {} + {}...", nt_file, name);
                            // Remove .graph first: if it fails, skip .nt to avoid
                            // orphan sidecar that blocks QLever rebuilds
                            if let Err(e) = mc(&["rm", &graph_file]) {
                                eprintln!("WARNING: failed to remove {}: {} — skipping .nt to avoid orphan sidecar", graph_file, e);
                                errors += 1;
                                continue;
                            }
                            if let Err(e) = mc(&["rm", nt_file]) {
                                eprintln!("WARNING: failed to remove {}: {}", nt_file, e);
                                errors += 1;
                            }
                            removed += 1;
                        }
                    }

                    if errors > 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!(
                                "Dropped graph <{}> with {} errors ({} files removed)",
                                graph, errors, removed
                            ),
                        ));
                    }
                    eprintln!("✓ Dropped graph <{}> ({} files removed)", graph, removed);
                    Ok((0, 0))
                })()
            } else {
                eprintln!("=== PackageGraph Graph Drop ===");
                eprintln!("Graph: {}", graph);
                eprintln!("Endpoint: {}", endpoint);
                eprintln!();

                let client =
                    pg_collect::sparql::make_sparql_client(&endpoint, &auth, make_backend());
                client.drop_graph(&graph).map(|_| (0, 0))
            }
        }

        Commands::ExtractTestCorpus {
            endpoint,
            config,
            ontology_dir,
            output_dir,
            max_triples,
            depth,
            fan_out,
        } => pg_collect::extract::run(
            &endpoint,
            std::path::Path::new(&config),
            std::path::Path::new(&ontology_dir),
            std::path::Path::new(&output_dir),
            max_triples,
            depth,
            fan_out,
            auth.clone(),
            make_backend(),
        )
        .map(|_| (0, 0)),

        // ─── Enricher Commands ─────────────────────────────────────────
        Commands::EnrichGithub {
            endpoint,
            output,
            github_token,
            cache_dir,
            minio_endpoint,
            minio_bucket,
            minio_access_key,
            minio_secret_key,
            max_repos,
            load_graph,
        } => {
            eprintln!("=== PackageGraph GitHub Enricher ===");
            eprintln!("Endpoint: {}", endpoint);
            eprintln!("Output: {}", output);
            eprintln!();

            // Validate flag combination
            if load_graph.is_some() && max_repos.is_none() {
                eprintln!("Error: --load-graph requires --max-repos to be specified");
                eprintln!(
                    "Usage: pg-collect enrich-github --max-repos 5000 --load-graph <graph_uri>"
                );
                std::process::exit(1);
            }
            if max_repos.is_some() && load_graph.is_none() {
                eprintln!("Error: --max-repos requires --load-graph to be specified");
                eprintln!(
                    "For incremental mode, use both: --max-repos 5000 --load-graph <graph_uri>"
                );
                eprintln!("For full corpus file-only mode, omit both flags");
                std::process::exit(1);
            }

            let minio = match (minio_endpoint, minio_access_key, minio_secret_key) {
                (Some(ep), Some(ak), Some(sk)) => Some(MinioConfig {
                    endpoint: ep,
                    bucket: minio_bucket,
                    access_key: ak,
                    secret_key: sk,
                }),
                _ => None,
            };

            let enricher = GitHubEnricher::new(
                &endpoint,
                github_token,
                cache_dir.as_deref(),
                minio,
                auth.clone(),
                make_backend(),
            )
            .with_graph(graph_uri.clone());

            // Choose execution mode
            match (max_repos, load_graph) {
                (Some(max), Some(graph_uri)) => {
                    eprintln!(
                        "Mode: Incremental (max {} repos, loading to {})",
                        max, graph_uri
                    );
                    enricher.enrich_incremental(&output, max, &graph_uri)
                }
                _ => {
                    eprintln!("Mode: Full corpus (file-only)");
                    enricher.enrich(&output)
                }
            }
        }

        Commands::EnrichAdvisory {
            advisory_type,
            output,
            days_back,
            cache_dir,
        } => {
            let at = if advisory_type == "rhsa" {
                AdvisoryType::Rhsa
            } else {
                AdvisoryType::Dsa
            };
            eprintln!(
                "=== PackageGraph Advisory Enricher ({}) ===",
                advisory_type.to_uppercase()
            );
            eprintln!("Output: {}", output);
            eprintln!();

            let enricher = AdvisoryEnricher::new(at, days_back, cache_dir.as_deref())
                .with_graph(graph_uri.clone());
            enricher.enrich(&output)
        }

        Commands::EnrichNpmProvenance { endpoint, output } => {
            eprintln!("=== PackageGraph npm Provenance Enricher ===");
            eprintln!("Endpoint: {}", endpoint);
            eprintln!("Output: {}", output);
            eprintln!();

            let enricher = NpmProvenanceEnricher::new(&endpoint, auth.clone(), make_backend())
                .with_graph(graph_uri.clone());
            enricher.enrich(&output)
        }

        Commands::EnrichKoji {
            endpoint,
            output,
            koji_hub,
            distro,
            release,
            cache_dir,
            limit,
            srpm_list,
        } => {
            eprintln!("=== PackageGraph Koji Enricher ===");
            eprintln!("Koji: {}", koji_hub);
            if let Some(ref path) = srpm_list {
                eprintln!("SRPM list: {}", path);
            } else {
                eprintln!("Endpoint: {}", endpoint);
            }
            if let Some(n) = limit {
                eprintln!("Limit: {} packages", n);
            }
            eprintln!("Output: {}", output);
            eprintln!();

            if let Some(ref path) = srpm_list {
                let content = std::fs::read_to_string(path)
                    .unwrap_or_else(|e| panic!("Failed to read --srpm-list {}: {}", path, e));
                let nvrs: Vec<String> = content
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| l.trim().to_string())
                    .collect();
                let enricher = KojiEnricher::new_standalone(
                    &koji_hub,
                    &distro,
                    &release,
                    cache_dir.as_deref(),
                )
                .with_graph_uri(graph_uri.clone());
                enricher.enrich_from_nvrs(&nvrs, &output, limit)
            } else {
                if endpoint.is_empty() {
                    panic!("Either --endpoint or --srpm-list is required for enrich-koji");
                }
                let enricher = KojiEnricher::new(
                    &endpoint,
                    &koji_hub,
                    &distro,
                    &release,
                    cache_dir.as_deref(),
                    auth.clone(),
                    make_backend(),
                )
                .with_graph_uri(graph_uri.clone());
                enricher.enrich_with_limit(&output, limit)
            }
        }

        Commands::EnrichRepology {
            endpoint,
            output,
            cache_dir,
        } => {
            eprintln!("=== PackageGraph Repology Enricher ===");
            eprintln!("Endpoint: {}", endpoint);
            eprintln!("Output: {}", output);
            eprintln!();

            let enricher = RepologyEnricher::new(
                &endpoint,
                cache_dir.as_deref(),
                auth.clone(),
                make_backend(),
            )
            .with_graph(graph_uri.clone());
            enricher.enrich(&output)
        }

        Commands::EnrichSecurity {
            endpoint,
            ecosystem,
            output,
            cache_dir,
        } => {
            eprintln!("=== PackageGraph Security Enricher ===");
            eprintln!("Endpoint: {}", endpoint);
            eprintln!("Ecosystem: {}", ecosystem);
            eprintln!("Output: {}", output);
            eprintln!();

            let enricher = SecurityEnricher::new(
                &endpoint,
                &ecosystem,
                cache_dir.as_deref(),
                auth.clone(),
                make_backend(),
            )
            .with_graph(graph_uri.clone());
            enricher.enrich(&output)
        }

        Commands::EnrichNvd {
            endpoint,
            output,
            mode,
            graph,
            nvd_api_key,
            cache_dir,
        } => {
            eprintln!("=== PackageGraph NVD CVE Metadata Enricher ===");
            eprintln!("Mode: {}", mode);
            eprintln!("Endpoint: {}", endpoint);

            (|| -> std::io::Result<(usize, usize)> {
                match mode.as_str() {
                    "feed" => {
                        // Feed mode requires --output
                        let output_path = output.ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "--output is required for feed mode",
                            )
                        })?;

                        if let Some(ref cd) = cache_dir {
                            eprintln!("Cache: {}", cd);
                        }
                        eprintln!("Output: {}", output_path);
                        eprintln!();

                        let enricher = NvdEnricher::new(
                            &endpoint,
                            nvd_api_key,
                            cache_dir.as_deref(),
                            auth.clone(),
                            make_backend(),
                        )?
                        .with_graph(graph_uri.clone());
                        enricher.enrich(&output_path)
                    }
                    "api" => {
                        eprintln!("Graph: {}", graph);
                        if nvd_api_key.is_some() {
                            eprintln!("API Key: [provided]");
                        } else {
                            eprintln!("API Key: [none] (rate limit: 5 req/30s)");
                        }
                        eprintln!();

                        let enricher = NvdEnricher::new(
                            &endpoint,
                            nvd_api_key,
                            None,
                            auth.clone(),
                            make_backend(),
                        )?
                        .with_graph(graph_uri.clone());
                        enricher.enrich_api(&graph)
                    }
                    _ => Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("Invalid mode '{}' — must be 'feed' or 'api'", mode),
                    )),
                }
            })()
        }

        Commands::EnrichForgeVersion {
            endpoint,
            output,
            cache_dir,
            gitlab_token,
        } => {
            eprintln!("=== PackageGraph Forge Version Enricher ===");
            eprintln!("Endpoint: {}", endpoint);
            eprintln!("Output: {}", output);
            eprintln!();

            let enricher = ForgeVersionEnricher::new(
                &endpoint,
                cache_dir.as_deref(),
                gitlab_token,
                auth.clone(),
                make_backend(),
            )
            .with_graph(graph_uri.clone());
            enricher.enrich(&output)
        }

        Commands::EnrichDiff {
            endpoint,
            output,
            github_token,
            cache_dir,
        } => {
            eprintln!("=== PackageGraph Diff Enricher ===");
            eprintln!("Endpoint: {}", endpoint);
            eprintln!("Output: {}", output);
            eprintln!();

            let enricher = pg_collect::enrich_diff::DiffEnricher::new(
                &endpoint,
                github_token,
                cache_dir.as_deref(),
                auth.clone(),
                make_backend(),
            )
            .with_graph(graph_uri.clone());

            enricher.enrich(&output)
        }

        Commands::EnrichEpss {
            endpoint,
            output,
            min_score,
        } => {
            eprintln!("=== PackageGraph EPSS Enricher ===");
            eprintln!("Endpoint: {}", endpoint);
            eprintln!("Output: {}", output);
            eprintln!("Min score: {}", min_score);
            eprintln!();

            let enricher = EpssEnricher::new(&endpoint, min_score, auth.clone(), make_backend())
                .with_graph(graph_uri.clone());
            enricher.enrich(&output)
        }

        Commands::EnrichTaxonomy { endpoint, output } => {
            eprintln!("=== PackageGraph Taxonomy Enricher ===");
            eprintln!("Endpoint: {}", endpoint);
            eprintln!("Output: {}", output);
            eprintln!();

            let enricher = TaxonomyEnricher::new(&endpoint, auth.clone(), make_backend())
                .with_graph(graph_uri.clone());
            enricher.enrich(&output)
        }

        Commands::EnrichRevdeps {
            endpoint,
            output,
            graph,
        } => {
            eprintln!("=== PackageGraph Reverse Dependency Count Enricher ===");
            eprintln!("Endpoint: {}", endpoint);
            eprintln!("Output: {}", output);
            if let Some(ref g) = graph {
                eprintln!("Graph: {}", g);
            }
            eprintln!();

            let enricher =
                RevdepsEnricher::new(&endpoint, graph.as_deref(), auth.clone(), make_backend())
                    .with_graph(graph_uri.clone());
            enricher.enrich(&output)
        }

        Commands::EnrichBlastRadius { endpoint, output } => {
            eprintln!("=== PackageGraph Blast Radius Enricher ===");
            eprintln!("Endpoint: {}", endpoint);
            eprintln!("Output: {}", output);
            eprintln!();

            let enricher = BlastRadiusEnricher::new(&endpoint, auth.clone(), make_backend())
                .with_graph(graph_uri.clone());
            enricher.enrich(&output)
        }

        Commands::DerivePackageHistory {
            endpoint,
            output,
            graph: graph_args,
            load,
        } => {
            eprintln!("=== PackageGraph Release Date Deriver ===");
            eprintln!("Endpoint: {}", endpoint);
            eprintln!("Output: {}", output);
            eprintln!();

            (|| -> std::io::Result<(usize, usize)> {
                let deriver = ReleaseDeriver::new(&endpoint, auth.clone(), make_backend())
                    .with_graph(graph_uri.clone());

                // Track whether this is a full run (all graphs) or targeted run (subset)
                let is_full_run = graph_args.is_empty();

                // Determine graphs to process
                let graphs = if is_full_run {
                    // Auto-discover package-source graphs using allowlist patterns
                    eprintln!("Auto-discovering package-source graphs...");
                    let allowlist_prefixes = vec![
                        "https://packagegraph.github.io/graph/fedora/",
                        "https://packagegraph.github.io/graph/rhel/",
                        "https://packagegraph.github.io/graph/centos-stream/",
                        "https://packagegraph.github.io/graph/opensuse/",
                        "https://packagegraph.github.io/graph/alpine/",
                        "https://packagegraph.github.io/graph/debian/",
                    ];

                    let mut discovered = Vec::new();
                    for prefix in allowlist_prefixes {
                        let sparql = format!(
                            r#"SELECT ?g WHERE {{ GRAPH ?g {{ ?s ?p ?o }} FILTER(STRSTARTS(STR(?g), "{}")) }} GROUP BY ?g"#,
                            prefix
                        );
                        match pg_collect::sparql::make_sparql_client(
                            &endpoint,
                            &auth,
                            make_backend(),
                        )
                        .query(&sparql)
                        {
                            Ok(bindings) => {
                                for binding in bindings {
                                    if let Some(g) = binding.get("g") {
                                        discovered.push(g.clone());
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!(
                                    "Warning: failed to discover graphs for {}: {}",
                                    prefix, e
                                );
                            }
                        }
                    }

                    if discovered.is_empty() {
                        eprintln!(
                            "No graphs found matching allowlist. Use --graph to specify manually."
                        );
                        return Ok((0, 0));
                    }

                    eprintln!("Found {} graphs", discovered.len());
                    discovered
                } else {
                    graph_args
                };

                // Derive lastReleaseDate
                let report = deriver.derive(&output, &graphs)?;

                // Optionally load into derived graph
                if load {
                    eprintln!();
                    let client =
                        pg_collect::sparql::make_sparql_client(&endpoint, &auth, make_backend());
                    let derived_graph =
                        "https://packagegraph.github.io/graph/derived/package-history";

                    if is_full_run {
                        // Full run: drop-and-replace (safe — we recomputed everything)
                        eprintln!("Full run: dropping existing graph before load...");
                        match client.drop_graph(derived_graph) {
                            Ok(_) => eprintln!("Dropped existing graph"),
                            Err(e) => {
                                eprintln!("Note: could not drop graph (may not exist): {}", e)
                            }
                        }
                    } else {
                        // Partial run: upsert-by-identity — delete existing lastReleaseDate
                        // triples for affected identities before loading new values.
                        // GSP POST is additive, so without this step a partial rerun
                        // would append a second triple rather than replacing the old one.
                        eprintln!("Partial run: deleting stale lastReleaseDate for affected identities...");
                        let identities =
                            pg_collect::derive_releases::extract_identity_uris(&output)?;
                        let pkg_lrd = "https://purl.org/packagegraph/ontology/core#lastReleaseDate";
                        let batch_size = 200;
                        for chunk in identities.chunks(batch_size) {
                            let values: String = chunk
                                .iter()
                                .map(|uri| format!("<{}>", uri))
                                .collect::<Vec<_>>()
                                .join(" ");
                            let sparql = format!(
                                "DELETE {{ GRAPH <{derived_graph}> {{ ?id <{pkg_lrd}> ?d }} }} \
                                 WHERE {{ GRAPH <{derived_graph}> {{ ?id <{pkg_lrd}> ?d }} VALUES ?id {{ {values} }} }}"
                            );
                            client.update(&sparql)?;
                        }
                        eprintln!(
                            "  Deleted stale triples for {} identities",
                            identities.len()
                        );
                    }

                    // Load new triples
                    client.load_file(&output, derived_graph, 10000)?;
                }

                Ok((report.derived, report.triples))
            })()
        }

        Commands::Seed {
            endpoint,
            graph,
            output,
        } => {
            eprintln!("=== PackageGraph Seed Generator ===");
            eprintln!("Endpoint: {}", endpoint);
            eprintln!("Graph: {}", graph);
            eprintln!("Output: {}", output);
            eprintln!();

            seed::generate_seed(&endpoint, &graph, &output, auth.clone(), make_backend())
                .map(|_| (0, 0))
        }

        Commands::Fetch {
            collector,
            cache_dir,
            url,
            distro,
            release,
            arch,
            repo,
        } => {
            eprintln!("=== PackageGraph Artifact Fetcher ===");
            eprintln!("Collector: {}", collector);
            eprintln!("Cache: {}", cache_dir);
            eprintln!("URL: {}", url);
            eprintln!();

            (|| -> std::io::Result<(usize, usize)> {
                match collector.as_str() {
                    "rpm" | "debian" => {
                        // Fetch happens automatically when collect() is called with cache enabled.
                        // Standalone fetch (without normalize/emit) requires exposing internal
                        // fetch methods publicly. For now, use collect() with cache to trigger fetch.
                        eprintln!("Fetch stage: use pg-collect {} --cache-dir {} --url {} --output <file.nt>",
                            collector, cache_dir, url);
                        eprintln!("(Standalone fetch is embedded in collect workflow for now)");
                        Ok((0, 0))
                    }
                    _ => Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("Unsupported collector: {}", collector),
                    )),
                }
            })()
        }

        Commands::Normalize {
            collector,
            from_cache,
            output_ir,
        } => {
            eprintln!("=== PackageGraph IR Normalizer ===");
            eprintln!("Collector: {}", collector);
            eprintln!("Cache: {}", from_cache);
            eprintln!("Output IR: {}", output_ir);
            eprintln!();

            match collector.as_str() {
                "rpm" | "debian" | "alpine" | "arch" => {
                    // For now, the normalize step is embedded in collect() when cache_dir is provided.
                    // Standalone normalize (read cache → write IR without emitting .nt) requires
                    // exposing the parse methods publicly or adding dedicated normalize() methods.
                    // This is deferred — use the all-in-one collect with cache for now.
                    eprintln!(
                        "Normalize stage: use pg-collect {} --cache-dir {} --output <file.nt>",
                        collector, from_cache
                    );
                    eprintln!("(Standalone normalize → IR output is not yet decoupled from emit)");
                    Ok((0, 0))
                }
                _ => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("Unsupported collector: {}", collector),
                )),
            }
        }

        Commands::EmitFromIr {
            from_ir,
            output,
            collector,
            distro,
            release,
        } => {
            use pg_collect::emit::debian_ext::emit_debian_extras;
            use pg_collect::emit::rdf::{emit_distribution_metadata, emit_rdf, EmitPolicy};
            use pg_collect::emit::rpm_ext::emit_rpm_extras;
            use pg_collect::ir::IrReader;
            use std::collections::HashSet;

            eprintln!("=== PackageGraph IR Emitter ===");
            eprintln!("IR directory: {}", from_ir);
            if let Some(ref c) = collector {
                eprintln!("Filter collector: {}", c);
            }
            if let Some(ref d) = distro {
                eprintln!("Filter distro: {}", d);
            }
            if let Some(ref r) = release {
                eprintln!("Filter release: {}", r);
            }
            eprintln!("Output: {}", output);
            eprintln!();

            (|| -> std::io::Result<(usize, usize)> {
                let ir_dir = std::path::Path::new(&from_ir);
                let nt_file = std::fs::File::create(&output)?;
                let mut writer = pg_collect::ntriples::NTriplesWriter::new_maybe_graph(
                    nt_file,
                    graph_uri.as_deref(),
                );
                let policy = EmitPolicy::default();

                let mut total_records = 0;
                let mut total_triples = 0;
                let mut emitted_distros: HashSet<String> = HashSet::new();

                for entry in walkdir::WalkDir::new(ir_dir).into_iter().flatten() {
                    let path = entry.path();
                    if !path.to_string_lossy().ends_with(".jsonl.zst") {
                        continue;
                    }

                    eprintln!("Reading {}", path.display());
                    let reader = IrReader::new(path)?;

                    for record_result in reader.records() {
                        let ir = record_result?;

                        // Apply filters
                        if let Some(ref c) = collector {
                            if &ir.scope.collector != c {
                                continue;
                            }
                        }
                        if let Some(ref d) = distro {
                            if &ir.scope.distro != d {
                                continue;
                            }
                        }
                        if let Some(ref r) = release {
                            if &ir.scope.release != r {
                                continue;
                            }
                        }

                        let key = format!("{}:{}", ir.scope.distro, ir.scope.release);
                        if emitted_distros.insert(key) {
                            let display_name = match ir.scope.distro.as_str() {
                                "fedora" => "Fedora",
                                "debian" => "Debian",
                                "ubuntu" => "Ubuntu",
                                "alpine" => "Alpine",
                                "arch" => "Arch",
                                _ => &ir.scope.distro,
                            };
                            total_triples += emit_distribution_metadata(
                                &mut writer,
                                &ir.scope.distro,
                                &ir.scope.release,
                                display_name,
                            )?;
                        }

                        // Shared emitter
                        total_triples += emit_rdf(&ir, &mut writer, &policy)?;

                        // Collector-specific extensions
                        match ir.scope.collector.as_str() {
                            "rpm" => total_triples += emit_rpm_extras(&ir, &mut writer)?,
                            "debian" => total_triples += emit_debian_extras(&ir, &mut writer)?,
                            _ => {}
                        }

                        total_records += 1;

                        if total_records % 1000 == 0 {
                            eprintln!("Emitted {} records", total_records);
                        }
                    }
                }

                writer.flush()?;
                Ok((total_records, total_triples))
            })()
        }

        Commands::RpmFull {
            urls,
            distro,
            release,
            output,
            with_koji,
            koji_hub,
            with_spec,
            with_buildrequires,
            with_maintainers,
            cache_dir,
            minio_endpoint,
            minio_bucket,
            minio_access_key,
            minio_secret_key,
            limit,
        } => {
            eprintln!("=== PackageGraph Consolidated RPM Collector ===");
            eprintln!("Distro: {} / Release: {}", distro, release);
            eprintln!("URLs: {:?}", urls);
            if with_koji {
                eprintln!("Koji: {}", koji_hub);
            }
            if with_spec {
                eprintln!("Spec: enabled");
            }
            if with_buildrequires {
                eprintln!("BuildRequires: enabled");
            }
            if with_maintainers {
                eprintln!("Maintainers: enabled");
            }
            eprintln!("Output: {}", output);
            eprintln!();

            (|| -> std::io::Result<(usize, usize)> {
                let file = std::fs::File::create(&output)?;
                let mut writer = NTriplesWriter::new_maybe_graph(file, graph_uri.as_deref());

                // Shared dedup sets across arches
                let mut noarch_seen = std::collections::HashSet::new();
                let mut srpm_seen = std::collections::HashSet::new();
                let mut srpm_nvrs = std::collections::HashSet::new();
                let mut srpm_names = std::collections::HashSet::new();
                let mut srpm_identity_map = std::collections::HashMap::new();

                let mut total_packages = 0;
                let mut total_triples = 0;

                // Stage 1: Multi-arch RPM collection
                for (i, url) in urls.iter().enumerate() {
                    eprintln!("\n--- Arch {} of {} ---", i + 1, urls.len());
                    let collector = RpmCollector::new(url.clone(), distro.clone(), release.clone());
                    let collector = if let Some(ref dir) = cache_dir {
                        collector.with_cache(dir)?
                    } else {
                        collector
                    };

                    let (pkgs, triples) = collector.collect_with_writer_limit(
                        &mut writer,
                        &mut noarch_seen,
                        &mut srpm_seen,
                        &mut srpm_nvrs,
                        &mut srpm_names,
                        &mut srpm_identity_map,
                        i > 0, // is_secondary for all after first
                        limit,
                    )?;
                    total_packages += pkgs;
                    total_triples += triples;
                }

                eprintln!(
                    "\nSRPM dedup: {} unique names, {} unique NVRs",
                    srpm_names.len(),
                    srpm_nvrs.len()
                );

                // Stage 2: Spec file collection (optional)
                if with_spec {
                    eprintln!("\n--- Spec File Collection ---");
                    let spec_collector =
                        SpecCollector::new(&distro, &release, cache_dir.as_deref())?;
                    let existing_ecosystem = std::collections::HashSet::new(); // TODO: track from RPM Provides
                    let (specs, triples) = spec_collector.collect(
                        &mut writer,
                        &srpm_names,
                        &srpm_identity_map,
                        &existing_ecosystem,
                        with_buildrequires,
                        with_maintainers,
                    )?;
                    total_packages += specs;
                    total_triples += triples;
                }

                // Stage 3: Koji enrichment (optional)
                if with_koji {
                    eprintln!("\n--- Koji Enrichment ---");
                    let nvr_list: Vec<String> = srpm_nvrs.into_iter().collect();
                    let minio_config = match (&minio_endpoint, &minio_access_key, &minio_secret_key)
                    {
                        (Some(ep), Some(ak), Some(sk)) => Some(MinioConfig {
                            endpoint: ep.clone(),
                            bucket: minio_bucket.clone(),
                            access_key: ak.clone(),
                            secret_key: sk.clone(),
                        }),
                        _ => None,
                    };
                    let koji_enricher = KojiEnricher::new_standalone_with_minio(
                        &koji_hub,
                        &distro,
                        &release,
                        cache_dir.as_deref(),
                        minio_config,
                    )
                    .with_graph_uri(graph_uri.clone());
                    // Write to a temp file, then append (Koji enricher creates its own writer)
                    let koji_tmp = format!("{}.koji.tmp", output);
                    let (builds, triples) =
                        koji_enricher.enrich_from_nvrs(&nvr_list, &koji_tmp, limit)?;
                    // Append Koji triples to main output
                    let koji_content = std::fs::read_to_string(&koji_tmp)?;
                    use std::io::Write;
                    writer.flush()?;
                    let mut main_file = std::fs::OpenOptions::new().append(true).open(&output)?;
                    main_file.write_all(koji_content.as_bytes())?;
                    let _ = std::fs::remove_file(&koji_tmp);
                    total_packages += builds;
                    total_triples += triples;
                }

                writer.flush()?;
                Ok((total_packages, total_triples))
            })()
        }

        Commands::DebFull {
            repo,
            distro,
            dist,
            component,
            arch,
            output,
            with_sources,
            with_builddeps,
            with_maintainers,
            with_salsa,
            cache_dir,
            limit,
        } => {
            eprintln!("=== PackageGraph Consolidated Debian Collector ===");
            eprintln!(
                "Distro: {} / Dist: {} / Component: {}",
                distro, dist, component
            );
            eprintln!("Repository: {}", repo);
            eprintln!("Architectures: {:?}", arch);
            if with_sources {
                eprintln!("Sources.gz: enabled");
            }
            if with_builddeps {
                eprintln!("Build-Depends: enabled");
            }
            if with_maintainers {
                eprintln!("Maintainers: enabled");
            }
            if with_salsa {
                eprintln!("salsa.debian.org: enabled");
            }
            eprintln!("Output: {}", output);
            eprintln!();

            (|| -> std::io::Result<(usize, usize)> {
                let file = std::fs::File::create(&output)?;
                let mut writer = NTriplesWriter::new_maybe_graph(file, graph_uri.as_deref());

                // Stage 0: Create collector instance and emit distribution metadata once
                let collector = DebianCollector::new(
                    repo.clone(),
                    distro.clone(),
                    dist.clone(),
                    component.clone(),
                );
                let collector = if let Some(ref dir) = cache_dir {
                    collector.with_cache(dir)?
                } else {
                    collector
                };

                // Normalize arch args (amd64 → binary-amd64)
                let normalized_arches: Vec<String> = arch
                    .iter()
                    .map(|a| {
                        let (repo_path, _) = normalize_arch(a);
                        repo_path
                    })
                    .collect();

                // Get release info and emit distribution metadata once
                let release_info = collector.get_release_info()?;
                eprintln!(
                    "Resolved '{}' to Origin='{}', Suite='{}', Codename='{}'",
                    dist, release_info.origin, release_info.suite, release_info.codename
                );
                collector.emit_distribution_metadata(
                    &mut writer,
                    &release_info,
                    &normalized_arches,
                )?;

                // Shared dedup sets across arches
                let mut all_arch_seen = std::collections::HashSet::new();
                let mut source_names = std::collections::HashSet::new();
                let mut source_identity_map = std::collections::HashMap::new();
                let mut vcs_urls = std::collections::HashMap::new();
                let mut source_pkg_uris = std::collections::HashMap::new();

                let mut total_packages = 0;
                let mut total_triples = 0;

                // Stage 1: Multi-arch Packages.gz collection
                for (i, arch_arg) in normalized_arches.iter().enumerate() {
                    eprintln!("\n--- Arch {} of {} ---", i + 1, normalized_arches.len());

                    // Extract arch_name (binary-amd64 → amd64)
                    let (_, arch_name_owned) = normalize_arch(arch_arg);
                    let arch_name = arch_name_owned.as_str();

                    let (pkgs, triples) = collector.collect_with_writer(
                        &mut writer,
                        arch_arg,
                        arch_name,
                        &release_info.codename,
                        &release_info.suite,
                        &mut all_arch_seen,
                        &mut source_names,
                        &mut source_identity_map,
                        &mut vcs_urls,
                        &mut source_pkg_uris,
                        i > 0, // is_secondary for all after first
                        limit,
                    )?;
                    total_packages += pkgs;
                    total_triples += triples;
                }

                eprintln!(
                    "\nSource package dedup: {} unique names",
                    source_names.len()
                );

                // Stage 2: Sources.gz parsing (optional)
                if with_sources {
                    eprintln!("\n--- Sources.gz Collection ---");
                    let sources_collector = SourcesCollector::new(
                        repo.clone(),
                        distro.clone(),
                        dist.clone(),
                        component.clone(),
                    );
                    let sources_collector = if let Some(ref dir) = cache_dir {
                        sources_collector.with_cache(dir)?
                    } else {
                        sources_collector
                    };

                    let (sources, triples) = sources_collector.collect(
                        &mut writer,
                        &source_names,
                        &source_identity_map,
                        &source_pkg_uris,
                        &vcs_urls,
                        &release_info.codename,
                        with_builddeps,
                        with_maintainers,
                    )?;
                    total_packages += sources;
                    total_triples += triples;
                }

                // Stage 3: salsa.debian.org enrichment (optional)
                if with_salsa {
                    eprintln!("\n--- salsa.debian.org Enrichment ---");
                    let salsa_collector =
                        SalsaCollector::new(dist.clone()).with_graph(graph_uri.clone());
                    let salsa_collector = if let Some(ref dir) = cache_dir {
                        salsa_collector.with_cache(dir)?
                    } else {
                        salsa_collector
                    };

                    let (salsa_pkgs, triples) = salsa_collector.collect(
                        &mut writer,
                        &source_names,
                        &source_identity_map,
                        &source_pkg_uris,
                        &vcs_urls,
                        with_maintainers,
                    )?;
                    total_packages += salsa_pkgs;
                    total_triples += triples;
                }

                writer.flush()?;
                Ok((total_packages, total_triples))
            })()
        }
    };

    match result {
        Ok((packages, triples)) => {
            let elapsed = start.elapsed();
            eprintln!();
            eprintln!(
                "Collected {} packages, {} triples in {:.2}s",
                packages,
                triples,
                elapsed.as_secs_f64()
            );
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}
