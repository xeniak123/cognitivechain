#!/usr/bin/env bash
#
# Pobiera koparkę CognitiveChain, weryfikuje sumę kontrolną i zaczyna kopać.
#
#   curl -fsSL https://raw.githubusercontent.com/xeniak123/cognitivechain/main/scripts/mine.sh \
#     | bash -s -- --wallet cog<TWÓJ_ADRES> --pool <IP_WĘZŁA>
#
# Nie masz jeszcze portfela? Skrypt pobiera też cog-node, więc:
#   ./cog-node keygen --out wallet.json
#
# Skrypt nigdy nie prosi o klucz prywatny. Do kopania wystarczy adres publiczny.

set -euo pipefail

REPO="xeniak123/cognitivechain"
DIR="${COG_HOME:-$HOME/.cognitivechain}"
WALLET=""
POOL=""
EXTRA=()

while [ $# -gt 0 ]; do
  case "$1" in
    --wallet) WALLET="${2:-}"; shift 2 ;;
    --pool)   POOL="${2:-}"; shift 2 ;;
    -h|--help) sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) EXTRA+=("$1"); shift ;;
  esac
done

die() { printf '\033[31m!! %s\033[0m\n' "$*" >&2; exit 1; }
say() { printf '\033[1m==> %s\033[0m\n' "$*"; }

[ -n "$WALLET" ] || die "podaj --wallet cog<40 znaków hex>"
[ -n "$POOL" ]   || die "podaj --pool <IP lub host węzła>"
[[ $WALLET =~ ^cog[0-9a-fA-F]{40}$ ]]   || die "adres portfela musi mieć postać cog + 40 znaków hex, dostałem: $WALLET"

case "$(uname -s)" in
  Linux)  ASSET="cog-miner"; NODE_ASSET="cog-node"; LABEL="linux-x64" ;;
  *)      die "ten skrypt jest dla Linuksa; na Windows użyj scripts/mine.ps1" ;;
esac

mkdir -p "$DIR"
cd "$DIR"

if [ ! -x "./$ASSET" ]; then
  say "Pobieram koparkę z najnowszego wydania"
  BASE="https://github.com/$REPO/releases/latest/download"
  curl -fsSL -o "$ASSET"      "$BASE/$ASSET"      || die "nie udało się pobrać $ASSET — sprawdź, czy wydanie jest już opublikowane"
  curl -fsSL -o "$NODE_ASSET" "$BASE/$NODE_ASSET" || true
  curl -fsSL -o "SHA256SUMS-$LABEL.txt" "$BASE/SHA256SUMS-$LABEL.txt" \
    || die "brak pliku z sumami kontrolnymi — przerywam, nie uruchomię niesprawdzonej binarki"

  say "Sprawdzam sumę kontrolną"
  EXPECTED=$(grep -E "[[:space:]]\*?${ASSET}\$" "SHA256SUMS-$LABEL.txt" | awk '{print $1}' | head -1)
  [ -n "$EXPECTED" ] || die "nie znalazłem sumy dla $ASSET w SHA256SUMS-$LABEL.txt"
  ACTUAL=$(sha256sum "$ASSET" | awk '{print $1}')
  if [ "$EXPECTED" != "$ACTUAL" ]; then
    rm -f "$ASSET"
    die "SUMA KONTROLNA SIĘ NIE ZGADZA. Plik został skasowany. Oczekiwano $EXPECTED, jest $ACTUAL"
  fi
  echo "OK: $ACTUAL"
  chmod +x "$ASSET" "$NODE_ASSET" 2>/dev/null || true
fi

say "Startuję koparkę"
echo "portfel: $WALLET"
echo "węzeł:   $POOL"
echo
if [ ${#EXTRA[@]} -gt 0 ]; then
  exec "./$ASSET" --wallet "$WALLET" --pool "$POOL" "${EXTRA[@]}"
else
  exec "./$ASSET" --wallet "$WALLET" --pool "$POOL"
fi
