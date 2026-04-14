# packagegraph/platform

ETL pipeline and OpenShift deployment for the PackageGraph knowledge graph.

## Repository Relationship

PackageGraph is split across two repositories:

- **`packagegraph/ontology`** — OWL 2 ontology defining the schema for software package metadata (classes, properties, SHACL shapes).
- **`packagegraph/platform`** (this repo) — ETL pipeline that collects package metadata, builds RDF graphs conforming to the ontology, and deploys a SPARQL endpoint.

## Directory Layout

```
platform/
  etl/              # Python ETL pipeline (collect, transform, load)
  fuseki/           # Apache Jena Fuseki SPARQL server configuration
  query/            # Query interfaces (YASGUI, Jupyter notebook)
  deploy/           # Kustomize manifests for OpenShift
    base/
    overlays/
      dev/
      prod/
  docs/             # Documentation
    QUERYING.md     # Query interface usage and administration guide
  Makefile          # Build, push, and deploy targets
```

## Usage

Build container images (requires `podman`):

```bash
make build          # Build both etl and fuseki images
make build-etl      # Build ETL image only
make build-fuseki   # Build Fuseki image only
```

Push to registry:

```bash
make push           # Push both images
```

Deploy to OpenShift (requires `oc` logged in):

```bash
make deploy-dev     # Apply dev overlay
make deploy-prod    # Apply prod overlay
```

Override the registry or tag:

```bash
make build REGISTRY=quay.io/myorg TAG=v1.0.0
```

## Querying the Data

See [docs/QUERYING.md](docs/QUERYING.md) for the full guide. Quick start:

```bash
# Port-forward to Fuseki
oc port-forward svc/fuseki 3030:3030 -n packagegraph

# Open the YASGUI web interface
open query/yasgui.html

# Or use the CLI
export FUSEKI_ENDPOINT=http://localhost:3030/packagegraph
packagegraph query-stats           # Distribution statistics
packagegraph query-search curl     # Search packages
packagegraph query-deps bash       # Dependencies
packagegraph query-vulns           # Vulnerabilities
```
