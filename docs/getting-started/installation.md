# Installation

This guide walks you through deploying the Sōzu gateway on a Kubernetes cluster using Helm.

## Prerequisites

- A running Kubernetes cluster (the controller pins `kube` 4 / `k8s-openapi` v1_36; older clusters
  work for the stable core objects it uses)
- `kubectl` pointing at your cluster, with cluster-admin access
- `helm` v3.x
- A way to expose the proxy: a `Service type=LoadBalancer` provider, or override `service.type`

> **Note:** No external credentials are required. The controller talks only to the in-cluster
> Kubernetes API (via its ServiceAccount) and to the local Sōzu command socket.

Each release publishes the controller image and the Helm chart to ghcr.io, so the default path
below needs no local build. Chart versions follow the release tags without the `v` prefix: release
`v0.1.0` publishes chart version `0.1.0` and image tag `v0.1.0`. To build and deploy your own image
instead, see [Installing from source](#installing-from-source).

## Step 1 — Install with Helm

The chart bundles the controller and the Sōzu data plane in one Pod, plus the `IngressClass`, RBAC
and Sōzu's `ConfigMap`. There are no CRDs to install (Phase 1 uses the built-in `networking.k8s.io`
Ingress types).

```sh
helm upgrade --install sozu-gateway \
  oci://ghcr.io/clevercloud/sozu-gateway \
  --version <version> --namespace sozu-system --create-namespace --wait
```

No image settings are needed: the chart's `appVersion` pins the matching published image
(`ghcr.io/clevercloud/sozu-gateway:v<version>`). See [the chart values](../../charts/sozu-gateway/values.yaml)
for everything you can tune.

## Step 2 — Verify

```sh
kubectl get pods -n sozu-system
```

The Pod should reach `Running` with both containers ready (`2/2`). Inspect the controller if it
does not:

```sh
kubectl logs -n sozu-system deployment/sozu-gateway -c controller
```

On a healthy start-up the controller logs `caches synced` and then `applying changes to sozu`.
Image-pull, RBAC or socket problems show up here.

Confirm the `IngressClass` exists and the LoadBalancer has an address:

```sh
kubectl get ingressclass sozu
kubectl get svc -n sozu-system sozu-gateway
```

## Optional — enable the Gateway API

Ingress works out of the box. To also use the Gateway API, install its CRDs (the controller
auto-detects them and otherwise stays in Ingress-only mode):

```sh
kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.2.1/standard-install.yaml
```

Then create a `GatewayClass` (with `controllerName: sozu.io/gateway-controller`), a `Gateway` and an
`HTTPRoute` — see [examples/gateway-api.yaml](../../examples/gateway-api.yaml). The controller
reports `Accepted`/`Programmed`/`ResolvedRefs` status back on those objects.

## Step 3 — Route an application

Deploy a demo app and an Ingress of class `sozu` (see [examples/](../../examples/README.md)):

```sh
kubectl apply -f examples/demo-app.yaml
```

Then send a request through the proxy (replace `<lb-ip>` with the Service's external IP):

```sh
curl -H 'Host: app.example.com' http://<lb-ip>/
```

You should get a `200` served by the backend pod. Deleting the Ingress hot-removes the route
(subsequent requests get `404` from Sōzu) — no proxy restart involved.

## Installing from source

Build and push your own image, then install the local chart pointing at it:

```sh
make image IMAGE=<your-registry>/sozu-gateway TAG=v0.1.0
docker push <your-registry>/sozu-gateway:v0.1.0

helm upgrade --install sozu-gateway charts/sozu-gateway \
  --namespace sozu-system --create-namespace \
  --set image.controller.repository=<your-registry>/sozu-gateway \
  --set image.controller.tag=v0.1.0 \
  --wait
```

For a one-command build-install-verify cycle on the current context, run `make e2e` (uses the
anonymous `ttl.sh` registry by default — no credentials needed).

## Upgrade

```sh
helm upgrade sozu-gateway oci://ghcr.io/clevercloud/sozu-gateway \
  --version <new-version> --namespace sozu-system --reuse-values --wait
```

A new version bumps both the controller image and the bundled Sōzu version together (they are
released as a matched pair, because the controller speaks Sōzu's command protocol through a pinned
`sozu-command-lib`). The chart rolls the Pod with `maxUnavailable=0`; for the LoadBalancer path to
stay gap-free during a data-plane bump, run with `replicaCount >= 2`.

## Uninstall

```sh
helm uninstall sozu-gateway --namespace sozu-system
kubectl delete namespace sozu-system
```

The `IngressClass` is part of the chart and is removed with it; your `Ingress` objects are left
untouched (they simply stop being reconciled).
