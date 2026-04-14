use clap::{Parser, Subcommand};
use pg_collect::alpine::AlpineCollector;
use pg_collect::arch::ArchCollector;
use pg_collect::debian::DebianCollector;
use pg_collect::homebrew::HomebrewCollector;
use pg_collect::rpm::RpmCollector;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "pg-collect")]
#[command(about = "PackageGraph bulk collector - streams N-Triples from package repositories")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Collect from Debian repository
    Debian {
        /// Repository URL
        #[arg(long, required = true)]
        repo: String,

        /// Distribution name
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
    },

    /// Collect from Alpine APK repository
    Alpine {
        /// Mirror URL (e.g., https://dl-cdn.alpinelinux.org/alpine)
        #[arg(long, default_value = "https://dl-cdn.alpinelinux.org/alpine")]
        mirror: String,

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

        /// Output file path
        #[arg(short, long, required = true)]
        output: String,
    },

    /// Collect from Arch Linux repositories
    Arch {
        /// Mirror URL (e.g., https://archive.archlinux.org/repos/last)
        #[arg(long, default_value = "https://archive.archlinux.org/repos/last")]
        mirror: String,

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
}

fn main() {
    let cli = Cli::parse();

    let start = Instant::now();

    let result = match cli.command {
        Commands::Debian {
            repo,
            dist,
            component,
            arch,
            output,
            workers: _,
        } => {
            eprintln!("=== PackageGraph Debian Collector ===");
            eprintln!("Repository: {}", repo);
            eprintln!("Distribution: {}", dist);
            eprintln!("Component: {}", component);
            eprintln!("Architectures: {:?}", arch);
            eprintln!("Output: {}", output);
            eprintln!();

            let collector = DebianCollector::new(repo, dist, component);
            collector.collect(&arch, &output)
        }

        Commands::Rpm {
            repo,
            rpm_repos,
            distro_name,
            release_name,
            output,
        } => {
            eprintln!("=== PackageGraph RPM Collector ===");

            if let Some(url) = repo {
                // Single --repo mode
                eprintln!("Repository: {}", url);
                eprintln!("Distribution: {}", distro_name);
                eprintln!("Release: {}", release_name);
                eprintln!("Output: {}", output);
                eprintln!();
                let collector = RpmCollector::new(url, distro_name, release_name);
                collector.collect(&output)
            } else if !rpm_repos.is_empty() {
                // Multi --rpm-repo mode: iterate ALL specs
                let mut total_packages = 0;
                let mut total_triples = 0;

                for (idx, repo_spec) in rpm_repos.iter().enumerate() {
                    let parts: Vec<&str> = repo_spec.splitn(3, ':').collect();
                    if parts.len() < 3 {
                        eprintln!("Error: --rpm-repo format is name:release:url, got: {}", repo_spec);
                        std::process::exit(1);
                    }
                    let rpm_distro = parts[0];
                    let rpm_release = parts[1];
                    let rpm_url = parts[2];

                    eprintln!("\n--- [{}/{}] {}/{} ---", idx + 1, rpm_repos.len(), rpm_distro, rpm_release);
                    eprintln!("Repository: {}", rpm_url);

                    // Each repo gets its own output file
                    let repo_output = if rpm_repos.len() == 1 {
                        output.clone()
                    } else {
                        let base = output.trim_end_matches(".nt");
                        format!("{}-{}-{}.nt", base, rpm_distro, rpm_release)
                    };

                    let collector = RpmCollector::new(
                        rpm_url.to_string(),
                        rpm_distro.to_string(),
                        rpm_release.to_string(),
                    );
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
            branch,
            repos,
            arch,
            output,
        } => {
            eprintln!("=== PackageGraph Alpine Collector ===");
            eprintln!("Mirror: {}", mirror);
            eprintln!("Branch: {}", branch);
            eprintln!("Repos: {:?}", repos);
            eprintln!("Arch: {}", arch);
            eprintln!("Output: {}", output);
            eprintln!();

            let collector = AlpineCollector::new(mirror, branch, repos, arch);
            collector.collect(&output)
        }

        Commands::Homebrew { api_base, output } => {
            eprintln!("=== PackageGraph Homebrew Collector ===");
            eprintln!("API: {}", api_base);
            eprintln!("Output: {}", output);
            eprintln!();

            let collector = HomebrewCollector::new(api_base);
            collector.collect(&output)
        }

        Commands::Arch {
            mirror,
            repos,
            include_aur,
            output,
        } => {
            eprintln!("=== PackageGraph Arch Linux Collector ===");
            eprintln!("Mirror: {}", mirror);
            eprintln!("Repos: {:?}", repos);
            eprintln!("Include AUR: {}", include_aur);
            eprintln!("Output: {}", output);
            eprintln!();

            let collector = ArchCollector::new(mirror, repos, include_aur);
            collector.collect(&output)
        }

        Commands::Load { file, graph, endpoint, batch_size } => {
            eprintln!("=== PackageGraph SPARQL Loader ===");
            eprintln!("File: {}", file);
            eprintln!("Graph: {}", graph);
            eprintln!("Endpoint: {}", endpoint);
            eprintln!("Batch size: {}", batch_size);
            eprintln!();

            let client = pg_collect::sparql::SparqlClient::new(&endpoint);
            client.load_file(&file, &graph, batch_size)
                .map(|count| (count, count))
        }

        Commands::Drop { graph, endpoint } => {
            eprintln!("=== PackageGraph Graph Drop ===");
            eprintln!("Graph: {}", graph);
            eprintln!("Endpoint: {}", endpoint);
            eprintln!();

            let client = pg_collect::sparql::SparqlClient::new(&endpoint);
            client.drop_graph(&graph)
                .map(|_| (0, 0))
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

