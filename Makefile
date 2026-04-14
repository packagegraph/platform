.PHONY: build-etl build-fuseki push-etl push-fuseki deploy-dev deploy-prod \
       scale-readers port-forward

REGISTRY ?= ghcr.io/packagegraph
ETL_IMAGE = $(REGISTRY)/etl
FUSEKI_IMAGE = $(REGISTRY)/fuseki
TAG ?= latest

build-etl:
	podman build -t $(ETL_IMAGE):$(TAG) -f etl/Containerfile etl/

build-fuseki:
	podman build -t $(FUSEKI_IMAGE):$(TAG) -f fuseki/Containerfile fuseki/

build: build-etl build-fuseki

push-etl:
	podman push $(ETL_IMAGE):$(TAG)

push-fuseki:
	podman push $(FUSEKI_IMAGE):$(TAG)

push: push-etl push-fuseki

deploy-dev:
	oc apply -k deploy/overlays/dev

deploy-prod:
	oc apply -k deploy/overlays/prod

# Scale read replicas (default: 2). Each replica loads TDB2 from Minio independently.
# Usage: make scale-readers N=3
N ?= 2
scale-readers:
	oc scale deployment/fuseki-reader --replicas=$(N) -n packagegraph
	@echo "Scaled fuseki-reader to $(N) replicas."
	@echo "Readers load TDB2 from Minio on startup — allow 2-5 min for init."
	@echo "Query endpoint: http://fuseki-reader.packagegraph.svc:3030/packagegraph/sparql"

# Refresh read replicas with latest data (rolling restart triggers Minio reload)
refresh-readers:
	oc rollout restart deployment/fuseki-reader -n packagegraph
	@echo "Rolling restart triggered. Replicas will reload TDB2 from Minio."

# Port-forward for local querying
port-forward:
	@echo "Forwarding Fuseki to localhost:3030 — press Ctrl+C to stop"
	oc port-forward svc/fuseki 3030:3030 -n packagegraph
