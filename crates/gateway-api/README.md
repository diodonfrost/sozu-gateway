# sozu-gw-gateway-api

Rust types for the [Gateway API](https://gateway-api.sigs.k8s.io/) CRDs
(`gateway.networking.k8s.io`), generated from the upstream **v1.2.1** standard channel.

The published `gateway-api` crate targets `kube` 3 / `k8s-openapi` 0.27, which conflicts with this
workspace's `kube` 4 / `k8s-openapi` 0.28 — so the types are generated locally against our exact
versions with [`kopium`](https://github.com/kube-rs/kopium) instead.

## Regenerate

```sh
GWVER=v1.2.1
base="https://raw.githubusercontent.com/kubernetes-sigs/gateway-api/$GWVER/config/crd/standard"
for f in gatewayclasses gateways httproutes referencegrants; do
  curl -sSL "$base/gateway.networking.k8s.io_${f}.yaml" -o "/tmp/${f}.yaml"
done

cargo install kopium --version 0.24.0
kopium -f /tmp/gatewayclasses.yaml  > src/gatewayclass.rs
kopium -f /tmp/gateways.yaml        > src/gateway.rs
kopium -f /tmp/httproutes.yaml      > src/httproute.rs
kopium -f /tmp/referencegrants.yaml > src/referencegrant.rs
```

Default `kopium` settings (`--schema=disabled`) are used: the types deserialize Gateway API
objects and derive `kube::Resource`, but carry no JSON schema (we consume the CRDs, we do not
publish them). Do not hand-edit the generated files — regenerate instead.
