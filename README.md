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
  deploy/           # Kustomize manifests for OpenShift
    base/
    overlays/
      dev/
      prod/
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
