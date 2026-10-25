# astrx-suite — deployment

One command brings the whole Rust suite up behind Tor:

```sh
cp deploy/.env.example .env      # optional: roles / image tag
docker compose up -d             # builds + starts the published stack
docker compose exec tor sh -c 'cat /var/lib/tor/*/hostname'   # your .onions
```

or, equivalently, `make -C deploy up` then `make -C deploy onions`.

## What runs

A single `docker compose build` compiles the Cargo workspace **once** (release,
statically linked against musl) into the `suite` image: alpine, git (for
gitweb) and the one `astrx` binary, which runs any engine. Every role runs from
that image under `astrx run`, which starts each role as a child of the binary,
prefixes its log lines with `[role]`, and restarts a role that exits (1s,
doubling up to 60s) without touching the others. The roles are grouped into one
container per network:

| Container | Network | Roles (default ✓, optional ○) | Ports |
|-----------|---------|-------------------------------|-------|
| `serve`   | `internal` only | ✓ `gitweb` · ✓ `onion-search` · ✓ `web-search` · ✓ `torrent-search` · ✓ `dash` · ○ `tracker` · ○ `onion-crawl` | 8801–8806 |
| `crawl`   | `egress` only | ○ `web-crawl` · ○ `dht-index` | — |
| `tor`     | both | the only ingress; SOCKS egress for the onion crawler | 9050 |

`serve` also answers on the names the services had as separate containers
(`gitweb`, `onioncrawler-search`, `websearch-serve`, `torrentds-search`,
`torrentds-tracker`, `suitedash`), so `torrc` and anything else that addressed
them work unchanged. Each engine's data volume is mounted at its own path
(`/data/onion`, `/data/web`, `/data/torrent`, `/repos`); existing volumes keep
their contents.

The per-engine image targets in `deploy/Dockerfile` remain, for hosts that run
one engine each (see [FLEET.md](FLEET.md)).

### Why the images are empty

The standard-library-only default library builds are distinct from the
network-enabled service binaries. Networking uses external crates, including
Tokio/getrandom and their transitive dependencies.

Four service images use `FROM scratch` with statically linked binaries. This
reduces the operating-system components included in the image, but it does not
remove embedded runtime code, libc where linked, or dependency vulnerabilities.
Track the actual build inputs, scan dependencies and rebuild affected releases.
The absence of a shell is not a general protection against code execution.

`gitweb` is the one documented exception: it is a front-end for the real `git`
binary, which it execs **argv-only, never through a shell**, so its image is
alpine plus git and nothing else.

## Privacy posture

* **No app port is published to the host** — there is no `ports:` mapping
  anywhere in the compose file. The only ingress is Tor.
* The app containers sit on an `internal` Docker network with no route to the
  host or the internet. `tor` is dual-homed (`internal` + `egress`) and bridges
  ingress in.
* The **darknet crawler is not attached to `egress` at all**: its only route out
  is Tor's SOCKS port on the internal network, so a bug cannot leak a clearnet
  request. `torrc`'s `SocksPolicy` additionally allows only the internal
  subnet to use SOCKS, so a compromised clearnet crawler cannot use Tor as an
  open proxy. (Keep that CIDR in sync with the `internal` network's `ipam`
  subnet — both are pinned to `10.63.0.0/24`.)
* Hidden-service keys live in the `tor-keys` volume, so every generated
  `.onion` address is **stable across restarts**.
* Every app container runs unprivileged, `cap_drop: ALL`,
  `no-new-privileges`, and with a read-only root filesystem.

## Opt-in workloads

The always-on roles are the read-only, published ones. The crawlers, the DHT
harvester and the trackers are off until you list them in `.env`
(see `deploy/.env.example`), so you choose what actually goes out to the
network:

```sh
# serve: add the darknet crawler (Tor only) and the trackers
ASTRX_SERVE_ROLES=gitweb,onion-search,web-search,torrent-search,dash,onion-crawl,tracker
# crawl: the egress container, with the clearnet crawler and/or the DHT harvester
COMPOSE_PROFILES=crawl
ASTRX_CRAWL_ROLES=web-crawl,dht-index
```

then `docker compose up -d` (or `make -C deploy up`). The crawlers read their
seeds from `seeds.txt` in their engine's volume.

## Health

`suitedash` is the suite's health monitor: it polls every service over the
internal network (trying the configured health path then known fallbacks,
parsing metrics as both Prometheus text and JSON) and federates every service's
`/metrics` into one aggregate exposition. The `serve` container's healthcheck
is the dashboard's own probe of the four front-ends (adjust it in
`docker-compose.yml` if you turn one of them off).

```sh
make -C deploy check      # exits non-zero if any service is DOWN
```

## Useful targets

```
make -C deploy help         list every target
make -C deploy config       validate + render the resolved compose config
make -C deploy onions       print each service's generated .onion address
make -C deploy verify-tor   syntax-check torrc inside the tor image
make -C deploy logs S=serve  (each line is prefixed with its role)
make -C deploy test         run the full workspace test + lint gate on the host
make -C deploy clean        remove containers, networks AND volumes (destroys the .onion keys)
```

## Scaling out

See [FLEET.md](FLEET.md) for the multi-host story: HRW-sharded clearnet
crawling with a stateless scatter-gather aggregator, and how the other engines
scale.

## Storage coordination

All snapshot writers must use the patched suite and the same canonical store
path. A crawler/indexer holds an exclusive writer lease; administrative writes
return 503 while it is busy. Search readers adopt valid published changes on the
next request, including segmented web indexes. Torrent blocklist writes persist
before acknowledgement. See [operations](../../docs/OPERATIONS.md) for backup,
crash-window and filesystem assumptions. The web fetcher remains HTTP-only.
