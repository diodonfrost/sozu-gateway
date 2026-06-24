# sozu-gateway — developer + release tasks.
#
# Container image + chart artifacts are published to ghcr.io under the
# CleverCloud org (see .github/workflows/release.yml). Override variables on the
# command line, e.g. `just IMAGE=my/repo TAG=v0.2.0 image`.

IMAGE := "ghcr.io/clevercloud/sozu-gateway"
TAG := "dev"
# Helm chart SemVer derived from TAG (v0.2.0 -> 0.2.0; dev -> dev).
CHART_VERSION := trim_start_match(TAG, "v")
CHART := "charts/sozu-gateway"
HELM_RELEASE := "sozu-gateway"
HELM_NS := "sozu-system"

# List the available recipes.
default:
    @just --list

# Build + test.
all: build test

# Build the whole workspace.
build:
    cargo build --workspace

# Unit + golden/snapshot tests.
test:
    cargo test --workspace

# CI gate: fmt check + clippy -D warnings.
lint: fmt-check clippy

# Format the workspace (write).
fmt:
    cargo fmt

# Check formatting without writing.
fmt-check:
    cargo fmt --check

# Clippy with warnings denied.
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Build the controller container image ({{IMAGE}}:{{TAG}}).
image:
    docker build -t {{IMAGE}}:{{TAG}} .

# Lint + render the Helm chart (also with rbac.allowStatusWrites=true).
chart-lint:
    helm lint {{CHART}}
    helm template {{HELM_RELEASE}} {{CHART}} > /dev/null
    helm template {{HELM_RELEASE}} {{CHART}} --set rbac.allowStatusWrites=true > /dev/null

# Package the Helm chart into dist/ (use TAG=v<semver>).
chart-package:
    mkdir -p dist
    helm package {{CHART}} --version {{CHART_VERSION}} --app-version {{TAG}} --destination dist

# Full in-cluster end-to-end on the current kube-context (build+push image,
# install the add-on, deploy the demo app, verify HTTP/HTTPS through Sōzu).
# Defaults to an ephemeral ttl.sh image so no registry credentials are needed.
e2e:
    bash scripts/e2e.sh

# Tear down e2e resources + cargo clean.
clean:
    -helm uninstall {{HELM_RELEASE}} -n {{HELM_NS}}
    -kubectl delete -f examples/ingress/demo-app.yaml
    rm -rf dist
    cargo clean
