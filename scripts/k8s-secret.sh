#!/usr/bin/env bash
# Crea (o aggiorna) la PSK del relay nello namespace rustbond.
# La chiave non viene mai scritta nel repository.
set -euo pipefail

NS="${NS:-rustbond}"

if kubectl -n "$NS" get secret rustbond-key >/dev/null 2>&1; then
  echo "secret rustbond-key già presente in $NS, non lo tocco"
else
  echo "genero una nuova PSK..."
  kubectl -n "$NS" create secret generic rustbond-key \
    --from-literal=key="$(openssl rand -hex 32)"
  echo "fatto. Per leggerla:"
  echo "  kubectl -n $NS get secret rustbond-key -o jsonpath='{.data.key}' | base64 -d; echo"
fi
