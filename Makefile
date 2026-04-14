.PHONY: build-etl build-fuseki push-etl push-fuseki deploy-dev deploy-prod

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
