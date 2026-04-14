use clap::{Parser, Subcommand};
use pg_collect::debian::DebianCollector;
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

            // Handle either --repo or --rpm-repo flags
            let repo_url = if let Some(url) = repo {
                url
            } else if !rpm_repos.is_empty() {
                // Parse first rpm-repo spec: name:release:url
                let parts: Vec<&str> = rpm_repos[0].split(':').collect();
                if parts.len() >= 3 {
                    parts[2..].join(":")
                } else {
                    eprintln!("Error: --rpm-repo format is name:release:url");
                    std::process::exit(1);
                }
            } else {
                eprintln!("Error: Either --repo or --rpm-repo must be specified");
                std::process::exit(1);
            };

            eprintln!("Repository: {}", repo_url);
            eprintln!("Distribution: {}", distro_name);
            eprintln!("Release: {}", release_name);
            eprintln!("Output: {}", output);
            eprintln!();

            let collector = RpmCollector::new(repo_url, distro_name, release_name);
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

