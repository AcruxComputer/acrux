# Acrux deploy

`Dockerfile` builds the node once for two images: `net` (Acrux Net, twelve validators in three hubs behind one gateway) and `api` (the API, with the node binary it runs for the schedule preview and the Fault Lab sandboxes). The build context is the repository root, and the chain programs are built first (`chain/target/deploy/demo_pool.so`, see [../chain/README.md](../chain/README.md)).

## The public host

`host/` runs Acrux on one Linux server: the twelve validators, the gateway, the API with its Fault Lab sandboxes, Postgres and Caddy with TLS. The Hub Registry stays on Solana mainnet. Anyone uses the net through the public RPC, the event stream, the API and its faucet.

| Container | Role | CPU weight | Data |
|---|---|---|---|
| `acrux-net` | 12 validators, gateway on 4930/4931, metrics and the faucet's grant route on 4989 (never proxied) | 4096 | volume `acrux-net`: the run store, current and previous run |
| `acrux-api` | API on 4920, registry read model, faucet, Fault Lab sandboxes (QUIC 5016-5027 inside the container) | 512 | |
| `acrux-postgres` | faucet grants, lab runs and their events, hubs and members | default | volume `acrux-pg` |
| `acrux-caddy` | TLS and the three public names on 80/443 | default | volume `acrux-caddy` (certificates) |

Only 22, 80 and 443 are open (`install.sh` sets the firewall); the node's QUIC, metrics and the API are reachable only inside Docker.

### Public limits

| Limit | Default | Variable |
|---|---|---|
| JSON-RPC reads per client IP | 50 a second, bursts of 100 | `ACRUX_RPC_RATE` |
| Transaction and fragment submits per client IP | 5 a second, bursts of 20 | `ACRUX_SUBMIT_RATE` |
| Event-stream connections per client IP | 8 | `ACRUX_WS_PER_IP` |
| Faucet grants | 3 per key and 10 per IP per 24 h, 2 SOL plus 100 AXA and 100 AXB each | `FAUCET_PER_KEY_LIMIT`, `FAUCET_PER_IP_LIMIT`, `FAUCET_WINDOW_SECONDS`, `FAUCET_GRANT_LAMPORTS` |
| Fault Lab runs | 6 per IP an hour, at most 10 waiting or running | `LAB_RUNS_PER_IP_HOUR`, `LAB_QUEUE_MAX` |

The gateway and the API take the client's address from the last `X-Forwarded-For` entry, the one Caddy appends.

### Browser origins

`SITE_ORIGINS` is an exact, comma-separated list that the gateway (`ACRUX_CORS_ORIGINS`) and the API (`CORS_ORIGINS`) both read. A listed origin gets the preflight allow headers and its WebSocket upgrade accepted; any other origin is refused with 403 (`ORIGIN_NOT_ALLOWED`). Requests without an `Origin` (a CLI, the SDK in Node) are served.

### Install

On a fresh Ubuntu 24.04 host, as root:

```bash
git clone <repo> /opt/acrux && bash /opt/acrux/deploy/host/install.sh
```

Then the host's secrets, made once and kept only on the host:

1. The genesis, from a secret seed (the faucet's and the mint authority's keys derive from it): in `chain/`, `npm ci`, then `ACRUX_GENESIS_SEED=<secret> node scripts/genesis.mjs genesis.json`, and copy `genesis.json` to `/etc/acrux/genesis.json` (mode 600).
2. `/etc/acrux/acrux.env` from `host/acrux.env.example`: a Postgres password, `ACRUX_IDENTITY_SEED` (a secret: the twelve validator identities derive from it) and `SOLANA_RPC_URL` (a mainnet RPC URL).
3. Point `rpc.<domain>`, `events.<domain>` and `api.<domain>` at the server's address.

### Start

```bash
cd /opt/acrux/deploy/host && docker compose --env-file /etc/acrux/acrux.env up -d --build
```

The first build compiles the node; later builds reuse its cache. Caddy fetches certificates on the first request to each name. `docker compose ps` and `curl https://api.<domain>/health` show it up; `docker compose logs -f net` follows the net.

The validators' identity keypairs, for joining the Hub Registry on mainnet: `docker compose run --rm net acrux identities --out /var/lib/acrux/identities` writes them into the `acrux-net` volume.

### Update

```bash
cd /opt/acrux && git pull && cd deploy/host && docker compose --env-file /etc/acrux/acrux.env up -d --build
```

Rebuilt containers restart; the net starts a fresh genesis. Postgres keeps every lab run, faucet grant and hub. `docker compose --env-file /etc/acrux/acrux.env restart net` starts a fresh genesis on its own.
