# PROGRESS

Journal de bord du contrôleur Ingress + Gateway API basé sur Sōzu.
Voir le prompt de cadrage pour le périmètre complet. On livre **Phase 1** (Ingress + TLS) avant tout le reste.

## Phase 1 — MVP Ingress + TLS

### Étape 1 — Vérification du protocole Sōzu ✅ (vérifiée contre un Sōzu réel)

Environnement confirmé :
- Rust `1.96.0` (stable), édition 2024 supportée. Docker, kubectl `1.36`, helm `4.2`, minikube (via devcontainer), `protoc 3.12`.
- `cargo` fonctionne (index sparse OK) ; l'API REST publique crates.io est bloquée mais sans impact.
- **`sozu-command-lib` v2.1.0** est la dernière version publiée. Deps notables : `prost 0.14`, `mio 1.2`, `nix 0.31`, `nom 7`. Licence **LGPL-3.0** (compatible plan de contrôle propriétaire).
- **Sōzu 2.1.0** : release GitHub + image `clevercloud/sozu:2.1.0` (binaire **musl** → exécuté via Docker). CLI client complet (`cluster`/`backend`/`frontend`/`listener`/`certificate`/`state`/`reload`) utilisable pour recouper la sonde. Crypto = rustls+ring.
- Cluster de test **poc-sozu-gateway-2** : propre (CNI Cilium, control-plane managé via konnectivity), aucun ingress controller.

Fait :
- Workspace Cargo scaffoldé (`ir`, `translator`, `builder`, `sozu-agent`, `controller`). `cargo check --workspace` **vert**.
- Pins de versions validés : `kube 4.0` + `k8s-openapi 0.28` (feature `v1_36`, = version du cluster).
- Source réelle de `sozu-command-lib` 2.1.0 explorée en profondeur (proto, code généré, channel/framing, state/diff, request/response, certificats).
- Cert de test auto-signé `app.example.com` généré (pour le test HTTPS de la sonde).

Fait (suite) :
- [x] `PROTOCOL.md` rédigé : types/champs/enum réellement observés (source de vérité du Translator).
- [x] Sonde `crates/sozu-agent/examples/probe.rs` + harnais `.scratch/run-probe.sh` : **HTTP 200 + HTTPS 200** à travers Sōzu, SNI OK (cert servi = le nôtre).
- [x] Ambiguïtés tranchées empiriquement : transport `Request` nu ; ack `Processing`→`Ok` (boucle obligatoire) ; listeners statiques du `config.toml` suffisent ; `ConfigState::diff` réutilisable.

Décisions validées (chat) : réutiliser `ConfigState::diff` ; listeners statiques dans `config.toml` ; e2e sur `poc-sozu-gateway-2`.

### Étape 2 — IR + Translator ✅
- `ir` : structs neutres sans I/O. `translator` : mapping pur + diff. Réutilise `ConfigState::diff` pour le graphe de routage ; **diff des certificats fait maison** (par fingerprint) → `ReplaceCertificate` pour rotation sans coupure, et contourne un debug_assert trop strict de sozu-command-lib 2.1.0 (bucket de cert vide laissé lors du retrait du dernier cert d'une adresse). Ordonnancement par tiers de dépendance. 8 golden tests.

### Étape 3 — sozu-agent ✅
- Cœur synchrone (connexion/reconnexion, boucle d'ack `Processing`→`Ok`, lectures bornées) + handle async (thread dédié + mpsc) sérialisant la socket. Validé bout-en-bout contre un vrai Sōzu (`agent_smoke`).

### Étape 4 — Builder + boucle kube-rs ✅
- `builder` pur : Ingress→IR, résolution Service→EndpointSlice→IPs de pods (jamais ClusterIP), TLS Secret→cert, filtrage IngressClass, `Problem`s par objet. 6 tests.
- `controller` : reflectors + reconcile global débouncé + resync ; shadow = IR ; jamais de panic. Validé contre `poc-sozu-gateway-2` (ajout + suppression à chaud).

### Étape 5 — Packaging + e2e ✅
- `Dockerfile` multi-stage, chart Helm (controller + Sōzu dans un Pod, Service LoadBalancer, IngressClass, RBAC, ConfigMap), `deploy/sozu/config.toml`, `Makefile`, CI GitHub Actions.
- **e2e in-cluster réel réussi** sur `poc-sozu-gateway-2` (image via ttl.sh) : HTTP 200 + HTTPS 200 (SNI), backend = pod réel, LoadBalancer avec IP externe (Cilium LB-IPAM), suppression à chaud → 404.

## Phase 1 — TERMINÉE ✅ (Ingress + TLS, vérifiée de bout en bout)

Limitations connues / Phase 1.x :
- Redémarrage du *seul* conteneur contrôleur (sans Sōzu) : le shadow repart vide → ré-applique tout (idempotent) mais ne nettoie pas un éventuel état résiduel côté Sōzu tant qu'un changement ne le supprime pas. Mitigation future : reconstruire le shadow depuis Sōzu au démarrage, ou `saved_state`.
- Status K8s des Ingress pas encore réécrit (les `Problem`s sont seulement loggés) — l'architecture le permet (RBAC `ingresses/status` déjà accordé).
- Conteneurs en uid 1000 partagé (socket) ; durcissement (séparation d'uid, capabilities) à faire.
