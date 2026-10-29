#!/usr/bin/env bash
# RustBond — test rapido dal Mac (headless, senza GUI e senza Gatekeeper).
# Uso:  curl -sL https://raw.githubusercontent.com/togioia/RustBond/main/scripts/test-mac.sh | bash
# Variabili: VERSION=v0.1.3-debug  PSK=<chiave>  WORK=~/rustbond-test
set -uo pipefail

VERSION="${VERSION:-v0.1.4-debug}"
BASE="https://github.com/togioia/RustBond/releases/download/${VERSION}"
WORK="${WORK:-$HOME/rustbond-test}"
CFG="$HOME/rustbond.toml"

mkdir -p "$WORK" || exit 1
cd "$WORK" || exit 1

echo "[1/5] scarico $VERSION"
if ! curl -fsSL -o rb.tgz "$BASE/rustbond-cli-macos-universal.tar.gz"; then
    echo "download fallito: controlla VERSION=$VERSION"
    exit 1
fi
tar -xzf rb.tgz
chmod +x ./rustbond
xattr -d com.apple.quarantine ./rustbond 2>/dev/null || true

echo "[2/5] recupero PSK"
if [ -z "${PSK:-}" ]; then
    PSK="$(kubectl -n rustbond get secret rustbond-key -o jsonpath='{.data.key}' 2>/dev/null | base64 -d 2>/dev/null || true)"
fi
if [ -z "${PSK:-}" ]; then
    echo "PSK non disponibile: esporta PSK=... (oppure configura kubectl sul Mac) e rilancia"
    exit 1
fi

echo "[3/5] scrivo $CFG"
cat >"$CFG" <<EOF
server_addr = "130.110.10.154:5000"
key = "$PSK"
interfaces = []

[[routes]]
id = 1
proto = "tcp"
local_port = 15443
targets = ["kubernetes.default.svc.cluster.local:443"]

[[routes]]
id = 3
proto = "tcp"
local_port = 18080
targets = ["traefik.kube-system.svc.cluster.local:80"]

[[routes]]
id = 2
proto = "udp"
local_port = 15353
targets = ["kube-dns.kube-system.svc.cluster.local:53"]
EOF

echo "[4/5] avvio il client"
pkill -x rustbond 2>/dev/null || true
sleep 1
RUSTBOND_CONFIG="$CFG" ./rustbond --headless >"$WORK/rustbond.log" 2>&1 &
sleep 6

echo "[5/5] test"
echo "=== LOG ==="
cat "$WORK/rustbond.log"

echo "=== apiserver  (atteso 401) ==="
curl -sk --max-time 8 -o /dev/null -w '%{http_code}\n' https://127.0.0.1:15443/version

echo "=== traefik    (atteso 404) ==="
curl -s --max-time 8 -o /dev/null -w '%{http_code}\n' http://127.0.0.1:18080/

echo "=== DNS        (atteso 1+ risposta) ==="
if command -v dig >/dev/null 2>&1; then
    dig @127.0.0.1 -p 15353 kubernetes.default.svc.cluster.local +short +time=3 +tries=1
else
    python3 - <<'PY'
import socket, struct
q = (b'\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00'
     b'\x0ckubernetes\x07default\x03svc\x07cluster\x05local\x00\x00\x01\x00\x01')
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.settimeout(4)
s.sendto(q, ("127.0.0.1", 15353))
try:
    d, _ = s.recvfrom(4096)
    print("anscount =", struct.unpack(">H", d[6:8])[0])
except Exception:
    print("DNS timeout")
PY
fi

echo
echo "client attivo in background — per fermarlo: pkill -x rustbond"
echo "log completo: $WORK/rustbond.log"
