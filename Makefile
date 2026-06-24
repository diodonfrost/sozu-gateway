# sozu-gateway — developer + release tasks.

# Container image + chart artifacts are published to ghcr.io under the
# CleverCloud org (see .github/workflows/release.yaml).
IMAGE ?= ghcr.io/clevercloud/sozu-gateway
TAG ?= dev
# Helm chart SemVer derived from TAG (v0.2.0 -> 0.2.0; dev -> dev).
CHART_VERSION ?= $(TAG:v%=%)
CHART ?= charts/sozu-gateway
HELM_RELEASE ?= sozu-gateway
HELM_NS ?= sozu-system

.PHONY: all build test lint fmt fmt-check clippy image chart-lint chart-package e2e clean help

all: build test ## Build + test

build: ## Build the whole workspace
	cargo build --workspace

test: ## Unit + golden tests
	cargo test --workspace

lint: fmt-check clippy ## CI gate: fmt check + clippy -D warnings

fmt: ## Format
	cargo fmt

fmt-check:
	cargo fmt --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

image: ## Build the controller container image ($(IMAGE):$(TAG))
	docker build -t $(IMAGE):$(TAG) .

chart-lint: ## Lint + render the Helm chart
	helm lint $(CHART)
	helm template $(HELM_RELEASE) $(CHART) > /dev/null
	helm template $(HELM_RELEASE) $(CHART) --set rbac.allowStatusWrites=true > /dev/null

chart-package: ## Package the Helm chart into dist/ (use TAG=v<semver>)
	mkdir -p dist
	helm package $(CHART) --version $(CHART_VERSION) --app-version $(TAG) --destination dist

## Full end-to-end on the current kube-context: build+push image, install the
## add-on, deploy the demo app, and verify HTTP/HTTPS traffic through Sōzu.
## Defaults to an ephemeral ttl.sh image so no registry credentials are needed.
e2e: ## Run the in-cluster end-to-end test
	bash scripts/e2e.sh

clean: ## Tear down e2e resources + cargo clean
	-helm uninstall $(HELM_RELEASE) -n $(HELM_NS)
	-kubectl delete -f examples/ingress/demo-app.yaml
	rm -rf dist
	cargo clean

help:
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "%-16s %s\n", $$1, $$2}'
