# waystoned

The self-hosted Waystone server: admin panel and WSS signaling on `TCP 8443`,
STUN/TURN on `UDP 3478`, state in one data directory.

## Run with Docker

Build from the repository root, then run with host networking (TURN relays
use extra UDP ports, and the daemon advertises the host's LAN address):

```bash
docker build -f crates/waystoned/Dockerfile -t waystoned .
docker run -d --name waystoned --network host -v waystone-data:/data waystoned
```

Behind NAT or on a cloud VM, tell clients where to reach you:

```bash
docker run -d --name waystoned --network host -v waystone-data:/data \
  -e WAYSTONE_ENDPOINT=home.example.com:8443 \
  -e WAYSTONE_TURN_ENDPOINT=home.example.com:3478 \
  -e WAYSTONE_TURN_PUBLIC_IP=203.0.113.7 \
  waystoned
```

Without `--endpoint` the daemon advertises its primary LAN IPv4 and logs the
choice at startup.

## First visit

Open `https://<server>:8443/` and choose an admin password. That claim works
once; afterwards the panel asks for the password. The panel adds devices,
shows their pairing QR codes and removes devices (removal also disconnects
the device).

## Scriptable pairing

`waystoned pair` prints a one-time pairing URI on stdout and exits. It reads
the data directory directly, so it needs no password, only access to the data
directory. It reuses the endpoints the running daemon advertises. Start the
daemon once first so the instance exists.

```bash
# Docker
docker exec waystoned waystoned pair --role host
docker exec waystoned waystoned pair --role controller

# Local
cargo run -p waystoned -- pair --role host --data-dir /tmp/wsdata
cargo run -p waystoned -- pair --role controller --data-dir /tmp/wsdata
```

Tokens expire after 10 minutes; set `WAYSTONE_PAIR_TTL=<seconds>` for longer
lived development tokens.

## Options

| Flag | Env | Default |
|---|---|---|
| `--data-dir` | `WAYSTONE_DATA_DIR` | `./data` (`/data` in Docker) |
| `--listen` | `WAYSTONE_LISTEN` | `0.0.0.0:8443` |
| `--endpoint` | `WAYSTONE_ENDPOINT` | `<LAN IPv4>:<listen port>` |
| `--turn-endpoint` | `WAYSTONE_TURN_ENDPOINT` | `<LAN IPv4>:<TURN port>` |
| `--turn-listen` | `WAYSTONE_TURN_LISTEN` | `0.0.0.0:3478` |
| `--turn-public-ip` | `WAYSTONE_TURN_PUBLIC_IP` | LAN IPv4 |
| `--turn-secret` | `WAYSTONE_TURN_SECRET` | random, kept in `<data-dir>/turn.secret` |
| `--relay-bitrate-ceiling` | `WAYSTONE_RELAY_BITRATE_CEILING` | `4000000` bps; `0` disables |
| `--no-turn` | `WAYSTONE_NO_TURN` | off |
| `--hostname` | `WAYSTONE_HOSTNAMES` | extra certificate names |

The data directory holds the TLS key, the TURN secret and the SQLite database
(admin password hash, devices). Back it up and keep it private.
