#!/usr/bin/env bash
#
# Jednokomendowa instalacja węzła CognitiveChain na świeżym VPS (Debian/Ubuntu).
#
#   curl -fsSL https://raw.githubusercontent.com/xeniak123/cognitivechain/main/scripts/install-node.sh | sudo bash
#
# Albo z peerami, jeśli sieć już działa:
#
#   ... | sudo bash -s -- --peer seed1.example.com:26656 --peer seed2.example.com:26656
#
# Skrypt jest idempotentny: ponowne uruchomienie aktualizuje kod i restartuje
# węzeł, nie kasując bazy danych łańcucha.

set -euo pipefail

REPO="https://github.com/xeniak123/cognitivechain.git"
DIR="${COG_DIR:-/opt/cognitivechain}"
RPC_PORT=26657
P2P_PORT=26656
PEERS=()

while [ $# -gt 0 ]; do
  case "$1" in
    --peer)
      [ $# -ge 2 ] || { echo "błąd: --peer wymaga argumentu host:port" >&2; exit 1; }
      PEERS+=("$2"); shift 2 ;;
    --dir)
      [ $# -ge 2 ] || { echo "błąd: --dir wymaga ścieżki" >&2; exit 1; }
      DIR="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)
      echo "błąd: nieznany argument $1" >&2; exit 1 ;;
  esac
done

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[33m!! %s\033[0m\n' "$*" >&2; }
die() { printf '\033[31m!! %s\033[0m\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "uruchom przez sudo: curl ... | sudo bash"
command -v apt-get >/dev/null || die "ten skrypt obsługuje Debiana/Ubuntu; na innym systemie zainstaluj Dockera ręcznie i użyj docker/docker-compose.yml"

say "Instaluję zależności"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends git curl ca-certificates >/dev/null

if ! command -v docker >/dev/null; then
  say "Instaluję Dockera"
  curl -fsSL https://get.docker.com | sh >/dev/null
fi
systemctl enable --now docker >/dev/null 2>&1 || true

if docker compose version >/dev/null 2>&1; then
  COMPOSE=(docker compose)
elif command -v docker-compose >/dev/null; then
  COMPOSE=(docker-compose)
else
  die "brak wtyczki docker compose; zainstaluj docker-compose-plugin"
fi

say "Pobieram kod do $DIR"
if [ -d "$DIR/.git" ]; then
  git -C "$DIR" fetch --quiet origin
  git -C "$DIR" reset --quiet --hard origin/main
else
  git clone --quiet "$REPO" "$DIR"
fi

say "Konfiguruję genesis"
mkdir -p "$DIR/docker/config"
if [ -f "$DIR/docker/config/genesis.json" ]; then
  echo "genesis.json już istnieje — zostawiam bez zmian"
else
  cp "$DIR/genesis/genesis.mainnet.json" "$DIR/docker/config/genesis.json"
fi

# Ostrzeż, jeśli sieć nie może wystartować, bo genesis_time jest w przyszłości.
GENESIS_TIME=$(grep -o '"genesis_time"[[:space:]]*:[[:space:]]*[0-9]*' \
  "$DIR/docker/config/genesis.json" | grep -o '[0-9]*$' || echo 0)
NOW=$(date +%s)
if [ "$GENESIS_TIME" -gt "$NOW" ]; then
  warn "genesis_time to $GENESIS_TIME, czyli $(( (GENESIS_TIME - NOW) / 86400 )) dni w przyszłość."
  warn "Węzeł wystartuje, ale ŻADEN BLOK nie powstanie przed tą datą."
  warn "Jeśli to nie jest zaplanowany start, popraw genesis_time w $DIR/docker/config/genesis.json"
fi

say "Uruchamiam węzeł"
COMPOSE_CMD=(run --genesis=/config/genesis.json --data-dir=/data
             --rpc=0.0.0.0:$RPC_PORT --p2p=0.0.0.0:$P2P_PORT)
if [ ${#PEERS[@]} -gt 0 ]; then
  for peer in "${PEERS[@]}"; do
    COMPOSE_CMD+=("--peer=$peer")
  done
fi

# Nadpisujemy `command:` z compose'a, żeby dołożyć peery bez edycji pliku.
cat > "$DIR/docker/docker-compose.override.yml" <<YML
services:
  node:
    command:
$(for arg in "${COMPOSE_CMD[@]}"; do echo "      - $arg"; done)
YML

cd "$DIR"
"${COMPOSE[@]}" -f docker/docker-compose.yml -f docker/docker-compose.override.yml up -d --build

say "Otwieram porty"
if command -v ufw >/dev/null && ufw status | grep -q "Status: active"; then
  ufw allow "$P2P_PORT"/tcp >/dev/null && echo "ufw: $P2P_PORT/tcp (P2P)"
  ufw allow "$RPC_PORT"/tcp >/dev/null && echo "ufw: $RPC_PORT/tcp (RPC dla koparek)"
else
  echo "ufw nieaktywny — upewnij się, że porty $P2P_PORT i $RPC_PORT są otwarte"
  echo "w panelu dostawcy VPS (security group / firewall)."
fi

say "Czekam, aż węzeł odpowie"
for _ in $(seq 1 30); do
  if curl -fsS "http://127.0.0.1:$RPC_PORT/health" >/dev/null 2>&1; then
    break
  fi
  sleep 2
done

if ! curl -fsS "http://127.0.0.1:$RPC_PORT/health" >/dev/null 2>&1; then
  warn "węzeł nie odpowiada na /health — sprawdź logi:"
  warn "  cd $DIR && ${COMPOSE[*]} -f docker/docker-compose.yml logs --tail=50"
  exit 1
fi

IP=$(curl -fsS --max-time 5 https://api.ipify.org 2>/dev/null || echo "<IP_TWOJEGO_SERWERA>")

say "Gotowe"
curl -fsS -X POST "http://127.0.0.1:$RPC_PORT" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"cog_status","params":{}}'
echo
echo
echo "Twój węzeł:      http://$IP:$RPC_PORT"
echo "Adres dla peerów: $IP:$P2P_PORT"
echo
echo "Podaj ludziom tę komendę, żeby kopali u Ciebie:"
echo
echo "    cog-miner --wallet <ICH_ADRES> --pool $IP"
echo
echo "Logi:    cd $DIR && ${COMPOSE[*]} -f docker/docker-compose.yml logs -f"
echo "Restart: cd $DIR && ${COMPOSE[*]} -f docker/docker-compose.yml restart"
