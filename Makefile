# sozu-gateway — developer + CI tasks.
# (A justfile equivalent could wrap these; Make is used since it's preinstalled.)

IMAGE ?= sozu-gw-controller:dev
HELM_RELEASE ?= sozu-gateway
HELM_NS ?= sozu-system

.PHONY: build test lint fmt fmt-check clippy docker-build helm-lint helm-template e2e clean

## Build the whole workspace.
build:
	cargo build --workspace

## Unit + golden tests.
test:
	cargo test --workspace

## fmt check + clippy with warnings denied (CI gate).
lint: fmt-check clippy

fmt:
	cargo fmt

fmt-check:
	cargo fmt --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

## Build the controller container image.
docker-build:
	docker build -t $(IMAGE) .

## Validate the Helm chart renders and lints.
helm-lint:
	helm lint deploy/helm

helm-template:
	helm template $(HELM_RELEASE) deploy/helm

## Full end-to-end on the current kube-context: build+push image, install the
## add-on, deploy the demo app, and verify HTTP/HTTPS traffic through Sōzu.
## Override IMAGE to use your own registry; defaults to an ephemeral ttl.sh tag.
e2e:
	bash scripts/e2e.sh

## Tear down the e2e resources.
clean:
	-helm uninstall $(HELM_RELEASE) -n $(HELM_NS)
	-kubectl delete -f examples/demo-app.yaml
	cargo clean
