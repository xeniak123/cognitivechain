# CognitiveChain (COG)

[![CI](https://github.com/xeniak123/cognitivechain/actions/workflows/ci.yml/badge.svg)](https://github.com/xeniak123/cognitivechain/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

Warstwa 1 zabezpieczona **użyteczną pracą obliczeniową**: zamiast pustego
haszowania górnicy wykonują gęste mnożenie macierzy w ciele skończonym — tę samą
operację, która stanowi rdzeń inferencji sieci neuronowych — a sieć potrafi to
zweryfikować ~20× taniej, niż kosztowało wyprodukowanie dowodu.

```
┌──────────────┐   cog_getWork      ┌─────────────────┐   NewBlock   ┌──────────┐
│  cog-miner   │ ─────────────────► │   cog-node      │ ───────────► │ cog-node │
│  GPU / CUDA  │ ◄───────────────── │  mempool        │ ◄─────────── │  peer    │
│              │   cog_submit*      │  walidacja PoUW │   GetBlocks  └──────────┘
└──────────────┘                    │  sled storage   │
                                    └─────────────────┘
```

| | |
|---|---|
| Token | **COG**, 8 miejsc dziesiętnych (1 COG = 100 000 000 acog) |
| Maksymalna podaż | **1 000 000 000 COG**, twardo zakodowana |
| Premine | 100 000 000 COG (10%): 5% founders, 3% ekosystem, 2% płynność |
| Emisja | 45 COG za zweryfikowane zadanie, halving co 10 000 000 zadań |
| Konsensus | Proof-of-Useful-Work, najcięższy łańcuch |
| Czas bloku | 30 s (retarget co 60 bloków) |
| Konta | model kontowy, podpisy ed25519, adresy `cog<40 hex>` |

**Chcesz kopać?** [Jak zacząć](#kopanie-w-dwóch-krokach) · **Chcesz uruchomić
sieć?** [LAUNCH.md](LAUNCH.md) — runbook startu ·
**Pełne wdrożenie:** [DEPLOYMENT.md](DEPLOYMENT.md) ·
**Protokół:** [docs/PROTOCOL.md](docs/PROTOCOL.md) ·
**Pula:** [docs/POOL.md](docs/POOL.md)

## Eksplorator i portfel

Węzeł serwuje **eksplorator bloków** pod `GET /` na tym samym porcie co RPC —
`http://<IP_WĘZŁA>:26657`. Lista bloków, szczegóły z otwartymi wierszami macierzy,
historia adresu, wyszukiwarka. Strona jest wkompilowana w binarkę, więc działa na
serwerze bez dostępu do internetu.

**Portfel graficzny** uruchamiasz lokalnie:

```bash
cog-node wallet-ui --key wallet.json --rpc <IP_WĘZŁA>:26657
# otwórz http://127.0.0.1:26658
```

Podpisywanie odbywa się w procesie Rusta, tą samą biblioteką ed25519, którą
weryfikuje węzeł. Przeglądarka nigdy nie dostaje klucza prywatnego — wysyła
`{to, amount, fee}` i dostaje hash transakcji. Serwer odmawia nasłuchu poza
loopbackiem bez jawnego `--allow-remote`, bo kto sięgnie do tego portu, ten
wyda Twoje monety.

## Kopanie w dwóch krokach

Portfel — pobierz `cog-node` z
[Releases](https://github.com/xeniak123/cognitivechain/releases) i uruchom
`cog-node keygen --out wallet.json`. Zapisz adres, zrób kopię pliku.

Koparka:

```bash
# Linux
curl -fsSL https://raw.githubusercontent.com/xeniak123/cognitivechain/main/scripts/mine.sh   | bash -s -- --wallet cog<TWÓJ_ADRES> --pool <IP_WĘZŁA>
```

```powershell
# Windows
irm https://raw.githubusercontent.com/xeniak123/cognitivechain/main/scripts/mine.ps1 -OutFile mine.ps1
.\mine.ps1 -Wallet cog<TWÓJ_ADRES> -Pool <IP_WĘZŁA>
```

Skrypty weryfikują sumę SHA-256 pobranej binarki i odmawiają uruchomienia pliku,
który się nie zgadza. Masz kartę NVIDIA? Doinstaluj
[PyTorch z CUDA](https://pytorch.org/get-started/locally/) — koparka wykryje ją sama.

Własny węzeł na VPS, jedną komendą:

```bash
curl -fsSL https://raw.githubusercontent.com/xeniak123/cognitivechain/main/scripts/install-node.sh | sudo bash
```

---

## Struktura repozytorium

```
node/           węzeł, portfel i pula (Rust)
  src/types.rs    formaty wire i preimage'e hashy (krytyczne dla konsensusu)
  src/pouw.rs     zadanie PoUW, zobowiązanie, wyzwanie, dowody Merkle
  src/state.rs    maszyna stanu, emisja, weryfikacja otwarcia zobowiązań
  src/chain.rs    walidacja bloków, wybór łańcucha, produkcja bloków
  src/rpc.rs      JSON-RPC dla koparek i portfeli
  src/p2p.rs      gossip TCP między węzłami
  tests/          testy integracyjne, w tym próby oszustwa
miner/          koparka (Python + CUDA przez PyTorch)
  cog_miner/protocol.py   odpowiednik pouw.rs, bajt w bajt
  cog_miner/compute.py    backendy: cuda-fast, cuda-fp64, cpu
genesis/        genesis.mainnet.json
docker/         Dockerfile + docker-compose
  src/pool.rs     pula: weryfikacja udziałów, PPLNS, wypłaty
scripts/        install-node.sh (węzeł na VPS), mine.sh i mine.ps1 (koparka)
```

## Szybki start (lokalny devnet)

```bash
# 0. Sklonuj
git clone https://github.com/xeniak123/cognitivechain.git
cd cognitivechain

# 1. Zbuduj węzeł
cd node && cargo build --release && cd ..

# 2. Portfel + genesis o niskiej trudności
./node/target/release/cog-node keygen --out wallet.json      # zapisz adres
./node/target/release/cog-node genesis-template \
    --out devnet.json --chain-id cognitivechain-devnet \
    --founders cog<adres> --ecosystem cog<adres> --liquidity cog<adres> \
    --initial-difficulty 300000 --block-time 5

# 3. Węzeł
./node/target/release/cog-node run --genesis devnet.json --data-dir ./data \
    --rpc 127.0.0.1:26657 --p2p 127.0.0.1:26656

# 4. Koparka (w drugim terminalu)
cd miner && pip install -r requirements.txt
python -m cog_miner --wallet cog<adres> --pool 127.0.0.1:26657
```

Po kilku sekundach zobaczysz `BLOCK FOUND` i rosnące saldo.

---

## Jak działa Proof-of-Useful-Work

### Zadanie

Górnik wyprowadza **prywatne** zadanie z `(hash rodzica, własny adres, losowy salt)`:

```
seed = BLAKE3("cog/task/v1" ‖ prev_hash ‖ miner ‖ salt)
A, B ← BLAKE3-XOF(seed)          macierze 1024×1024 nad GF(65521)
C    = A · B mod p               ~2,1 mld operacji — to jest ta użyteczna praca
root = MerkleRoot(wiersze C)
```

Następnie przeszukuje **ograniczoną** przestrzeń nonce'ów szukając:

```
BLAKE3("cog/pow/v1" ‖ seed ‖ root ‖ nonce) × difficulty < 2²⁵⁶,   nonce < 2¹⁶
```

Limit `2¹⁶` jest kluczowy: po jego wyczerpaniu górnik **musi** policzyć nowe
mnożenie macierzy, żeby szukać dalej. Haszowania nie da się podstawić w miejsce
pracy tensorowej.

### Dlaczego pracy nie da się udawać

Weryfikacja całego `C` kosztowałaby tyle, co jego policzenie, więc jest
odroczona o dokładnie jeden blok (commit–reveal):

1. **Blok T** niesie samo zobowiązanie `root`. Nagroda nie jest jeszcze
   emitowana. Wyzwanie w tym momencie **nie istnieje**.
2. **Blok T+1** musi zawierać otwarcie: 32 wiersze `C` wskazane przez
   `BLAKE3("cog/chal/v1" ‖ hash bloku T)`, każdy z dowodem Merkle.
   Hash bloku T zależy od znacznika czasu, transakcji i cudzego otwarcia —
   górnik nie kontroluje żadnego z tych składników.
3. Walidator przelicza tylko te 32 wiersze: `O(k·N²)` zamiast `O(N³)`.

Górnik, który policzył uczciwie tylko ułamek `f` wierszy, przechodzi z
prawdopodobieństwem `f³²`. Zaoszczędzenie połowy pracy to zakład 1 : 2³²,
a nieudane otwarcie oznacza, że nagroda **nigdy nie zostaje wyemitowana**.

Ta własność jest testowana, nie tylko opisana — `node/tests/mining.rs`:

```
a_reveal_with_a_forged_row_is_rejected            ... ok
a_miner_that_skipped_rows_cannot_open_the_challenge ... ok
an_unopened_commitment_forfeits_its_reward        ... ok
a_nonce_outside_the_bounded_space_is_rejected     ... ok
```

### Dokładność arytmetyki

Wszystko liczy się nad `p = 65521` (największa liczba pierwsza < 2¹⁶), więc
każdy akumulator iloczynu skalarnego mieści się poniżej `1024 · (p−1)² < 2⁴²` —
jest dokładny zarówno w `u64` na walidatorze, jak i w `float64` na GPU. Wynik
jest bit w bit identyczny na każdej karcie, co jest warunkiem koniecznym, żeby
w ogóle dało się go zweryfikować.

Backend `cuda-fast` rozkłada wartości na dwa 8-bitowe człony i liczy w `float32`
blokami po 256 kolumn (`255² · 256 < 2²⁴`), dzięki czemu korzysta z przepustowości
tensor cores **bez utraty dokładności**. TF32 jest jawnie wyłączony, a przy
starcie backend porównuje się z referencją fp64 — niepoprawne urządzenie nie
zostanie dopuszczone do kopania.

---

## Weryfikacja poprawności

```bash
cd node && cargo test --release     # 21 testów: 14 jednostkowych + 7 integracyjnych
./target/release/cog-node selftest  # pełny cykl PoUW z pomiarem czasu
cd ../miner && python -m cog_miner --wallet cog0000000000000000000000000000000000000001 \
                                   --pool 127.0.0.1:26657 --benchmark
```

Zgodność implementacji Rust i Python jest sprawdzalna wprost: `cog-node selftest`
i skrypt z `docs/PROTOCOL.md` dla tych samych danych wejściowych wypisują ten sam
`task seed` i `matmul root`.

---

## API JSON-RPC

`POST /` na porcie 26657, JSON-RPC 2.0.

| Metoda | Do czego |
|---|---|
| `cog_status` | wysokość, trudność, podaż, rozmiar mempoola |
| `cog_getWork` | parametry bieżącego zadania dla koparki |
| `cog_submitSolution` | zgłoszenie zobowiązania (`miner`, `salt`, `nonce`, `matmul_root`) |
| `cog_getRevealRequests` | które zobowiązania czekają na otwarcie i jakie wiersze |
| `cog_submitReveal` | otwarcie wyzwania z dowodami Merkle |
| `cog_getBalance` | saldo i nonce konta |
| `cog_sendTransaction` | rozgłoszenie podpisanego przelewu |
| `cog_getBlock` | blok po `height` albo `hash` |
| `cog_getBlocks` | ostatnie bloki, od najnowszego |
| `cog_getAddressHistory` | zdarzenia adresu: wykopane bloki i przelewy |
| `cog_getSupply` | wyemitowana i pozostała podaż, bieżąca nagroda |

```bash
curl -s -X POST http://127.0.0.1:26657 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"cog_status","params":{}}'
```

---

## Ograniczenia, o których warto wiedzieć

Uczciwy opis tego, czym ten system **nie** jest:

* **To nie jest ZK-ML.** Dowód nie jest zero-wiedzy ani zwięzły — to
  probabilistyczny spot-check commit–reveal. Jest za to w pełni działający,
  tani w weryfikacji i dowodliwie odporny na pomijanie pracy. Prawdziwy zkSNARK
  dla inferencji dokłada sekundy–minuty na dowód i jest tematem osobnego etapu.
* **„Użyteczność" jest strukturalna, nie aplikacyjna.** Praca to prawdziwe
  GEMM-y, ta sama operacja co w inferencji, ale macierze są pseudolosowe —
  nie trenują niczyjego modelu. Podpięcie realnych zadań wymaga warstwy
  zleceniodawców, która potrafi wskazać wejścia bez łamania weryfikowalności.
* **Wybór łańcucha jest prosty.** Najcięższy łańcuch, reorganizacja przez
  odtworzenie gałęzi od genesis. Poprawne, ale przy bardzo długiej historii
  reorg jest kosztowny — docelowo wymaga snapshotów stanu.
* **P2P jest minimalne.** Statyczna lista peerów, bez odkrywania węzłów,
  bez szyfrowania transportu, bez systemu reputacji. Do startu wystarcza;
  do sieci publicznej dużej skali warto dołożyć te elementy.

---

## Licencja

Apache-2.0.
