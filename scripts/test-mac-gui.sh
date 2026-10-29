#!/usr/bin/env bash
# RustBond — test GUI macOS (menu bar).
# Uso:  curl -sL https://raw.githubusercontent.com/togioia/RustBond/main/scripts/test-mac-gui.sh | bash
# Vars: VERSION=v0.1.3-debug  PSK=<chiave>  WORK=~/rustbond-test
set -uo pipefail

VERSION="${VERSION:-v0.3.1-debug}"
BASE="https://github.com/togioia/RustBond/releases/download/${VERSION}"
WORK="${WORK:-$HOME/rustbond-test}"
APP="$WORK/RustBond.app"
BIN="$APP/Contents/MacOS/RustBond"
CFG_DIR="$HOME/Library/Application Support/rustbond"
CFG="$CFG_DIR/config.toml"
LOG="$WORK/gui.log"

mkdir -p "$WORK" || exit 1
cd "$WORK" || exit 1

echo "[1/5] scarico $VERSION"
if ! curl -fsSL -o app.zip "$BASE/RustBond-macos-universal.zip"; then
    echo "download fallito: controlla VERSION=$VERSION"
    exit 1
fi
rm -rf "$APP"
unzip -oq app.zip
chmod +x "$BIN"
xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true
codesign --verify --deep --strict "$APP" 2>/dev/null && echo "      firma ad-hoc valida"

echo "[2/5] recupero PSK"
if [ -z "${PSK:-}" ]; then
    PSK="$(kubectl -n rustbond get secret rustbond-key -o jsonpath='{.data.key}' 2>/dev/null | base64 -d 2>/dev/null || true)"
fi
if [ -z "${PSK:-}" ]; then
    echo "PSK non disponibile: esporta PSK=... (oppure configura kubectl) e rilancia"
    exit 1
fi

echo "[3/5] config GUI → $CFG"
mkdir -p "$CFG_DIR"
cat >"$CFG" <<EOF
server_addr = "130.110.10.154:5000"
key = "$PSK"
interfaces = []
proxy_port = 11080

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

echo "[4/5] chiudo eventuali istanze e avvio l'app"
pkill -x RustBond 2>/dev/null || true
pkill -x rustbond 2>/dev/null || true
sleep 1
: >"$LOG"
nohup "$BIN" >>"$LOG" 2>&1 &
sleep 4

echo "[5/5] stato"
if pgrep -x RustBond >/dev/null; then
    echo "      RustBond è in esecuzione (icona nella menu bar)."
else
    echo "      ATTENZIONE: processo non attivo. Log:"
    tail -n 20 "$LOG"
    exit 1
fi

echo
echo "Log live (lascia aperto questo comando in un'altra finestra):"
echo "    tail -f \"$LOG\""
echo
echo "Nella finestra di RustBond:"
echo "  1. controlla Server (130.110.10.154:5000) e Key (già compilati)"
echo "  2. spunta le interfacce da bondare (vuoto = route di sistema)"
echo "  3. premi Start"
echo
echo "Poi verifica con:"
cat <<'EOT'
    curl -sk -o /dev/null -w 'apiserver %{http_code}\n' https://127.0.0.1:15443/version
    curl -s  -o /dev/null -w 'traefik   %{http_code}\n' http://127.0.0.1:18080/
    dig @127.0.0.1 -p 15353 kubernetes.default.svc.cluster.local +short
    curl -sx socks5h://127.0.0.1:11080 -o /dev/null -w 'socks     %{http_code}\n' --max-time 15 https://example.com/
EOT
echo
echo "Stop: pkill -x RustBond    |    Config GUI: \"$CFG\""
