# CognitiveChain — instrukcja wdrożenia (mainnet + koparka)

Przewodnik krok po kroku: od pustego VPS do sieci, w której użytkownicy realnie
kopią COG na swoich kartach graficznych.

Całość dzieli się na cztery etapy:

1. [Przygotowanie kluczy i pliku genesis](#1-klucze-i-genesis)
2. [Uruchomienie węzła mainnet (Docker Compose)](#2-węzeł-mainnet-na-vps)
3. [Dołączenie kolejnych walidatorów](#3-kolejne-walidatory)
4. [Spakowanie i publikacja koparki dla użytkowników](#4-publikacja-koparki)

Na końcu znajdziesz [checklistę startu](#checklista-startu-mainnetu) i
[rozwiązywanie problemów](#rozwiązywanie-problemów).

---

## 0. Wymagania

**Węzeł (VPS):**

| Zasób | Minimum | Zalecane |
|---|---|---|
| CPU | 2 rdzenie | 4 rdzenie |
| RAM | 2 GB | 8 GB |
| Dysk | 20 GB SSD | 100 GB NVMe |
| Sieć | 100 Mbit, publiczne IP | 1 Gbit |
| System | Linux z Dockerem | Ubuntu 22.04/24.04 LTS |

Weryfikacja bloku to ~10 ms pracy CPU (32 przeliczone wiersze macierzy), więc
węzeł jest tani w utrzymaniu nawet przy dużej liczbie koparek.

**Koparka (użytkownik końcowy):** dowolny Windows/Linux. Karta NVIDIA z CUDA
jest opcjonalna — bez niej koparka działa na CPU, tylko wolniej.

**Do budowania:** Rust 1.83+ (`rustup`) i Python 3.9+ albo sam Docker.

---

## 1. Klucze i genesis

### 1.1 Zbuduj narzędzia

```bash
git clone <adres-twojego-repo> cognitivechain
cd cognitivechain/node
cargo build --release
```

Binarka: `node/target/release/cog-node` (na Windows `cog-node.exe`).

Sprawdź, czy silnik Proof-of-Useful-Work działa na tej maszynie:

```bash
./target/release/cog-node selftest
```

Powinieneś zobaczyć wyliczone `C = A*B mod p`, korzeń Merkle i informację, ile
razy tańsza jest weryfikacja od produkcji dowodu.

### 1.2 Wygeneruj portfele alokacji startowej

Trzy adresy z pliku genesis dostają premine (łącznie 10% podaży). Wygeneruj je
**na maszynie offline** i zrób kopie zapasowe — tych kluczy nie da się odzyskać.

```bash
./target/release/cog-node keygen --out founders.json
./target/release/cog-node keygen --out ecosystem.json
./target/release/cog-node keygen --out liquidity.json
```

Każde wywołanie wypisze adres w formacie `cog<40 znaków hex>`. Zapisz je.

> **Uwaga:** plik `*.json` zawiera klucz prywatny w postaci jawnej. Trzymaj go
> poza serwerem produkcyjnym. Węzeł do działania **nie potrzebuje** żadnego
> klucza prywatnego — nie kopiuj ich na VPS.

### 1.3 Wygeneruj plik genesis

```bash
./target/release/cog-node genesis-template \
  --out ../genesis/genesis.mainnet.json \
  --chain-id cognitivechain-1 \
  --founders  cog<adres_founders> \
  --ecosystem cog<adres_ecosystem> \
  --liquidity cog<adres_liquidity> \
  --initial-difficulty 5000000 \
  --block-time 30
```

Ustaw jeszcze `genesis_time` na planowany moment startu (unix seconds, np.
`date +%s`) — bloki z czasem wcześniejszym niż genesis nie zostaną przyjęte.

Sprawdź wynik:

```bash
./target/release/cog-node inspect-genesis --genesis ../genesis/genesis.mainnet.json
```

```
chain_id          : cognitivechain-1
genesis hash      : 5694a641ccfacd08...
max supply        : 1000000000.00000000 COG
premine           : 100000000.00000000 COG
initial reward    : 45.00000000 COG per verified task
halving every     : 10000000 tasks
```

**Ekonomia jest zamknięta matematycznie.** Węzeł odmówi startu, jeśli
harmonogram emisji mógłby przekroczyć twardy limit:

```
premine                    100 000 000 COG   (10%)
emisja dla górników        900 000 000 COG   (90%)
                           ----------------
maksymalna podaż         1 000 000 000 COG
```

45 COG za zadanie × 10 000 000 zadań w epoce, halving co epokę → suma szeregu
`45 × 10M × (1 + ½ + ¼ + …)` = 900M COG. Dodatkowo `State::mint` przycina
każdą wypłatę do pozostałego limitu, więc przekroczenie podaży jest niemożliwe
nawet przy błędnej konfiguracji.

### 1.4 Dobór `initial_difficulty`

`difficulty` to w przybliżeniu **oczekiwana liczba prób skrótu na blok**.
Koparka wykonuje 65 536 prób na jedno mnożenie macierzy, więc:

```
difficulty ≈ (liczba zadań/s w całej sieci) × 65536 × (docelowy czas bloku)
```

| Scenariusz | Zadania/s | `initial_difficulty` |
|---|---|---|
| Devnet, jedna koparka CPU | ~25 | `300000` |
| Start mainnetu, kilka GPU | ~50–200 | `5000000` |
| Sieć dojrzała | — | ustala się sama |

Wartość startowa nie musi być idealna: po 60 blokach algorytm retargetu
koryguje trudność (maks. ±4× na okno), aż trafi w `target_block_time_secs`.

---

## 2. Węzeł mainnet na VPS

### 2.1 Przygotuj serwer

```bash
ssh root@<IP_SERWERA>
apt update && apt install -y docker.io docker-compose-plugin git
systemctl enable --now docker
```

### 2.2 Wgraj kod i genesis

```bash
git clone <adres-twojego-repo> /opt/cognitivechain
cd /opt/cognitivechain
cp genesis/genesis.mainnet.json docker/config/genesis.json
```

Plik `docker/config/genesis.json` musi być **bajt w bajt identyczny** na
każdym węźle sieci — jego hash jest sprawdzany w handshake'u P2P i węzły
z różnym genesis nigdy się nie połączą.

### 2.3 Uruchom

```bash
docker compose -f docker/docker-compose.yml up -d --build
docker compose -f docker/docker-compose.yml logs -f
```

Oczekiwane logi:

```
INFO cog_node: chain cognitivechain-1 ready at height 0 (genesis 5694a641...)
INFO cog_node::p2p: P2P listening on 0.0.0.0:26656
INFO cog_node::rpc: JSON-RPC listening on http://0.0.0.0:26657
```

### 2.4 Otwórz porty

```bash
ufw allow 26656/tcp   # P2P — musi być dostępny dla innych walidatorów
ufw allow 26657/tcp   # JSON-RPC — dostępny dla koparek
ufw enable
```

### 2.5 Sprawdź, że żyje

```bash
curl -s http://<IP_SERWERA>:26657/health
# ok

curl -s -X POST http://<IP_SERWERA>:26657 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"cog_status","params":{}}' | jq
```

### 2.6 Uruchomienie bez Dockera (systemd)

```bash
install -m 0755 node/target/release/cog-node /usr/local/bin/cog-node
useradd --system --home /var/lib/cog cog
mkdir -p /var/lib/cog /etc/cog && chown cog:cog /var/lib/cog
cp genesis/genesis.mainnet.json /etc/cog/genesis.json

cat >/etc/systemd/system/cog-node.service <<'EOF'
[Unit]
Description=CognitiveChain node
After=network-online.target
Wants=network-online.target

[Service]
User=cog
Environment=RUST_LOG=info
ExecStart=/usr/local/bin/cog-node run \
  --genesis /etc/cog/genesis.json \
  --data-dir /var/lib/cog \
  --rpc 0.0.0.0:26657 \
  --p2p 0.0.0.0:26656
Restart=always
RestartSec=5
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload && systemctl enable --now cog-node
journalctl -u cog-node -f
```

---

## 3. Kolejne walidatory

Każdy dodatkowy węzeł dostaje ten sam `genesis.json` i wskazuje na już
działające węzły przez `--peer`:

```bash
cog-node run \
  --genesis /etc/cog/genesis.json \
  --data-dir /var/lib/cog \
  --rpc 0.0.0.0:26657 \
  --p2p 0.0.0.0:26656 \
  --peer seed1.twojadomena.pl:26656 \
  --peer seed2.twojadomena.pl:26656
```

W Docker Compose dopisz je do sekcji `command:`.

Nowy węzeł sam pobierze łańcuch (`GetBlocks`/`Blocks`) i **niezależnie
zweryfikuje** każdy blok: dowód pracy, dowody Merkle, przeliczenie wylosowanych
wierszy macierzy i korzeń stanu. Zsynchronizowanie potwierdzisz porównując
`tip_hash` z `cog_status` na obu węzłach.

### Zalecana topologia startowa

* 2–3 węzły seed z publicznym DNS, wzajemnie połączone.
* Endpoint RPC dla koparek za reverse proxy z TLS (nginx/Caddy) i rate limitem.
* Węzeł „skarbcowy" bez wystawionego RPC, tylko do odczytu stanu.

---

## 4. Publikacja koparki

### 4.1 Zbuduj plik wykonywalny

Na **Windowsie** (dla użytkowników Windows):

```powershell
cd miner
python -m pip install -r requirements.txt pyinstaller
python build_release.py
# -> miner\dist\cog-miner.exe
```

Na **Linuksie** (dla użytkowników Linux):

```bash
cd miner
python3 -m pip install -r requirements.txt pyinstaller
python3 build_release.py
# -> miner/dist/cog-miner
```

PyInstaller nie kompiluje skrośnie — każdą wersję zbuduj na docelowym systemie
(np. przez GitHub Actions z matrycą `windows-latest` + `ubuntu-latest`).

Buduj domyślnie **wersję CPU** (~40 MB). Koparka wykrywa PyTorch w czasie
działania, więc użytkownik z kartą NVIDIA instaluje go sam i automatycznie
dostaje ścieżkę GPU. Wersja `--gpu` z wbudowanym CUDA-PyTorch waży ~2,5 GB i
nadaje się raczej na osobny, opcjonalny download.

### 4.2 Opublikuj

Wrzuć na GitHub Releases (albo własny CDN):

```
cog-miner.exe            # Windows x64
cog-miner                # Linux x64
SHA256SUMS.txt
```

```bash
sha256sum cog-miner cog-miner.exe > SHA256SUMS.txt
```

Podpisz sumy kontrolne kluczem GPG projektu i opublikuj klucz publiczny —
użytkownicy pobierający plik .exe muszą mieć czym zweryfikować, że dostali
twoją binarkę.

### 4.3 Instrukcja dla użytkownika końcowego

Umieść ją w opisie release'u:

> **Jak kopać COG**
>
> 1. Pobierz `cog-miner.exe` (Windows) lub `cog-miner` (Linux).
> 2. Załóż portfel — pobierz `cog-node.exe` i uruchom:
>    `cog-node keygen --out wallet.json`
>    Zapisz wypisany adres `cog...` i zrób kopię pliku `wallet.json`.
> 3. Uruchom koparkę:
>    ```
>    cog-miner --wallet cog<TWÓJ_ADRES> --pool <IP_WĘZŁA>
>    ```
> 4. Chcesz kopać na GPU NVIDIA? Zainstaluj PyTorch z CUDA
>    (https://pytorch.org/get-started/locally/) — koparka wykryje kartę sama.
> 5. Saldo sprawdzisz w panelu koparki albo poleceniem:
>    `cog-node balance --address cog<TWÓJ_ADRES> --rpc <IP_WĘZŁA>:26657`

Panel koparki wygląda tak:

```
  CognitiveChain miner  ---  COG
  device     cuda-fast / NVIDIA GeForce RTX 4070  12.0 GiB VRAM, compute capability 8.9
  pool       http://203.0.113.10:26657  [connected]
  wallet     cogc46bf565db3756e2008880b1aad6519871957b7f
  chain      cognitivechain-1   height 1482   difficulty 5241300

  useful work      4.812 TOPS       12.30 tasks/s
  nonce search   806.1 kH/s          1478 tasks total
  blocks found          7          proofs revealed 7
  balance       315.00000000 COG
  uptime        18m 04s
```

### 4.4 Weryfikacja przed publikacją

```bash
cd miner
python -m cog_miner --wallet cog<adres> --pool <ip> --benchmark
```

Komenda uruchamia selftest urządzenia (porównanie z referencją fp64) i mierzy
przepustowość, nie wysyłając nic do sieci. Jeśli selftest nie przejdzie,
koparka **odmawia startu** — inaczej użytkownik paliłby prąd na dowody, które
sieć i tak odrzuci.

---

## Checklista startu mainnetu

- [ ] Klucze alokacji wygenerowane offline, kopie zapasowe w dwóch miejscach.
- [ ] `genesis.json` z prawdziwymi adresami i `genesis_time` = moment startu.
- [ ] `inspect-genesis` pokazuje poprawną podaż i hash genesis.
- [ ] Hash genesis opublikowany (strona/README) — użytkownicy muszą móc
      sprawdzić, że łączą się z właściwą siecią.
- [ ] Co najmniej 2 węzły seed działają i widzą się nawzajem (zgodny `tip_hash`).
- [ ] Porty 26656/26657 otwarte, `/health` odpowiada.
- [ ] `cargo test --release` przechodzi w całości (21 testów).
- [ ] Binarki koparki zbudowane dla Windows i Linux, `SHA256SUMS.txt` podpisane.
- [ ] Backup katalogu `data/` w cronie (`docker compose stop` → kopia → `start`).
- [ ] `initial_difficulty` dobrana do spodziewanej mocy startowej.

---

## Rozwiązywanie problemów

**`stored genesis ... does not match the supplied genesis file`**
Zmieniłeś `genesis.json` po pierwszym uruchomieniu. Albo przywróć stary plik,
albo skasuj katalog danych (`docker volume rm cognitivechain_cog-data`) —
skasowanie oznacza start łańcucha od zera.

**`data directory belongs to chain "X" but genesis says "Y"`**
Katalog danych pochodzi z innej sieci. Użyj innego `--data-dir`.

**Koparka pokazuje `OFFLINE`**
Sprawdź `curl http://<ip>:26657/health` z maszyny koparki. Najczęstsze przyczyny:
zamknięty port 26657 na firewallu VPS albo węzeł nasłuchujący na `127.0.0.1`
zamiast `0.0.0.0`.

**`solution rejected: solution does not satisfy the current difficulty`**
Praca była liczona na starym wierzchołku łańcucha (inny górnik był szybszy).
To normalne przy wielu koparkach; licznik `rejected` rośnie, ale nic nie tracisz.

**`reveal references unknown commitment`**
Koparka wysłała otwarcie zobowiązania po tym, jak okno się zamknęło — nagroda za
ten blok przepada, ale łańcuch działa dalej. Jeśli zdarza się często, węzeł jest
przeciążony albo łącze koparki ma bardzo wysokie opóźnienia.

**Bloki nie powstają mimo działającej koparki**
`difficulty` jest za wysoka względem mocy sieci. Przy uruchamianiu nowej sieci
zacznij od niższej `initial_difficulty` — po starcie zmiana wymaga już nowego
genesis, czyli nowej sieci.

**`backend cuda-fast produced an incorrect row`**
Karta lub sterownik liczą nieprecyzyjnie (najczęściej wymuszony TF32 albo
podkręcenie pamięci). Uruchom z `--precision fp64`; jeśli błąd zostaje, cofnij
podkręcenie GPU.
