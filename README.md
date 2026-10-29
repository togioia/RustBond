# RustBond

Bonding L4 (TCP/UDP) tra un Mac e un relay Rust minimale, pensato per girare su
nodi edge (k3s, container Alpine, binario statico da ~2 MB).

```
 app locale ──► 127.0.0.1:PORTA ──► client macOS ──(N trasporti, 1 per interfaccia)──► relay ──► target
```

## Caratteristiche

- **TCP e UDP raw**, nessun overhead HTTP.
- **Multiplexing su una singola connessione** per trasporto: tutti gli stream
  locali viaggiano su un solo socket, con frame `[stream_id][kind][len][payload]`.
- **Bonding multi-interfaccia sul Mac**: scegli Wi-Fi, Ethernet, hotspot iPhone…
  ogni interfaccia apre il proprio trasporto e gli stream vengono distribuiti
  round-robin. Se un link cade, il suo supervisore riconnette senza toccare gli altri.
- **Fallback multi-target sul relay** (primario + backup, health check TCP).
- **PSK** condivisa per autenticare il client.
- **Binario server statico** (`musl`, `scratch` image) per amd64 e arm64.
- **GUI macOS "liquid glass"**: finestra trasparente senza decorazioni, con
  vero `NSGlassEffectView` su macOS 26 (vibrancy come fallback), schede in vetro,
  pill di stato e pulsanti con riflesso speculare.
- **Contatori per link in tempo reale**: per ogni interfaccia vedi byte inviati e
  ricevuti, percentuale di traffico e stato del link; totale complessivo e reset.
- **Proxy di sistema automatico (stile WireGuard)**: al via la GUI imposta il
  proxy SOCKS di macOS (`networksetup`) su tutte le interfacce attive, allo stop
  ripristina esattamente le impostazioni precedenti. Safari/Chrome passano dal
  bond senza configurazione manuale.
- **Menu bar macOS**: URL server, chiave, interfacce, rotte, Start/Stop,
  salvataggio config, finestra riducibile nella barra dei menu.

## Struttura

```
shared/   protocollo + tipi di configurazione
server/   relay (statico musl, immagine scratch)
client/   app macOS (menu bar + engine)
deploy/   manifest k3s
examples/ configurazioni di esempio
```

## Configurazione

File TOML, usato sia dal client sia dal server (ognuno legge ciò che gli serve):

```toml
server_addr = "vps.example.com:5000"
key = "cambia-questa-chiave"
interfaces = ["en0", "en1"]      # solo client, vuoto = route di sistema
proxy_port = 11080                # solo client: SOCKS5 locale su 127.0.0.1
auto_proxy = true                 # solo client macOS: gestisce il proxy di sistema

[[routes]]
id = 1                            # id stabile 1..65000
proto = "tcp"
local_port = 5432                 # solo client: dove ascolta in locale
targets = ["10.0.0.5:5432", "10.0.0.6:5432"]  # solo server: primario + fallback

[[routes]]
id = 2
proto = "udp"
local_port = 5353
targets = ["10.0.0.5:5353"]
```

- Client: `~/Library/Application Support/rustbond/config.toml`
  (override con `RUSTBOND_CONFIG=/percorso/file.toml`).
- Server: `/etc/rustbond/config.toml` (override con `CONFIG=...`).
- Chiave server: env `RNB_KEY` (ha priorità sul file).

## Build

### Server (Linux statico)

```bash
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo build --release -p rnb-server --target x86_64-unknown-linux-musl
# → target/x86_64-unknown-linux-musl/release/rustbond-server (~2 MB, statico)
```

### Client (macOS universale)

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --release -p rnb-client --target aarch64-apple-darwin
cargo build --release -p rnb-client --target x86_64-apple-darwin
lipo -create -output rustbond \
  target/aarch64-apple-darwin/release/rustbond \
  target/x86_64-apple-darwin/release/rustbond
```

Per vedere i log da terminale (utile in debug):

```bash
./rustbond --headless          # solo engine, log su stderr
./rustbond                     # GUI, log su stderr se lanciata dal terminale
```

### Docker

```bash
docker build -f server/Dockerfile -t rustbond .
docker run --rm -e RNB_KEY=... -p 5000:5000 -v $PWD/examples/server.toml:/etc/rustbond/config.toml:ro rustbond
```

## Deploy su k3s

```bash
kubectl apply -f deploy/k3s.yaml
# la chiave NON è nel repo:
kubectl -n rustbond create secret generic rustbond-key \
  --from-literal=key="$(openssl rand -hex 32)" --dry-run=client -o yaml | kubectl apply -f -
kubectl -n rustbond rollout status deploy/rustbond
kubectl -n rustbond logs deploy/rustbond --tail=50
```

Il Service espone `NodePort 30500` (TCP). Il manifest distribuisce i pod sui nodi
edge (b602 amd64 + worker-oracle-01 arm64) e limita le risorse a 64 MiB.

## Sicurezza

v0.1 è pensato per reti fidate:

- PSK in chiaro sul canale: **nessun TLS**. Non esporre il relay su Internet
  senza un tunnel (WireGuard/Tailscale) o senza aggiungere TLS in una versione
  successiva.
- La chiave va in un Secret Kubernetes / variabile d'ambiente, mai nel repo.

## Licenza

Dual-licensed: **MIT OR Apache-2.0**. Vedi `LICENSE-MIT` e `LICENSE-APACHE`.
Puoi usarlo in progetti commerciali; se preferisci una licenza copyleft
(es. AGPL-3.0) è sufficiente sostituire i file di licenza e il campo `license`.
