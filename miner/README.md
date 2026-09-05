# cog-miner

Koparka CognitiveChain. Wykrywa GPU, pobiera zadanie z węzła, liczy je na
tensorach i odsyła dowód.

```
cog-miner --wallet <TWÓJ_ADRES_COG> --pool <IP_WĘZŁA>
```

## Instalacja

**Gotowa binarka** (nic więcej nie trzeba): pobierz `cog-miner.exe` (Windows)
albo `cog-miner` (Linux) z sekcji Releases.

**Ze źródeł:**

```bash
pip install -r requirements.txt
python -m cog_miner --wallet cog<adres> --pool <ip>
```

**Wsparcie GPU NVIDIA** — zainstaluj PyTorch pasujący do swojej wersji CUDA:

```bash
pip install torch --index-url https://download.pytorch.org/whl/cu124
```

Koparka wykrywa PyTorch przy starcie. Bez niego działa na CPU (NumPy + BLAS),
tylko wolniej.

## Opcje

| Flaga | Domyślnie | Opis |
|---|---|---|
| `--wallet` | wymagana | adres COG, na który idą nagrody |
| `--pool` | wymagana | `IP`, `IP:PORT` albo `http://HOST:PORT` (port domyślny 26657) |
| `--device` | `auto` | `auto` \| `cuda` \| `cpu` |
| `--precision` | `auto` | `fast` (limby 8-bit na fp32) \| `fp64` \| `auto` |
| `--quiet` | — | jedna linia na zdarzenie zamiast panelu |
| `--benchmark` | — | selftest + pomiar wydajności, bez łączenia z siecią |

## Co robi jeden cykl

1. `cog_getWork` — pobranie wierzchołka łańcucha i trudności.
2. Wyprowadzenie prywatnego zadania z `(prev_hash, adres, losowy salt)`.
3. `C = A · B mod p` na 1024×1024 nad GF(65521) — **właściwa praca**.
4. Zobowiązanie do korzenia Merkle i przeszukanie 65 536 nonce'ów.
5. Trafienie → `cog_submitSolution`, potem otwarcie 32 wylosowanych wierszy
   przez `cog_submitReveal`. Dopiero otwarcie uruchamia wypłatę nagrody.

Koparka trzyma w pamięci cztery ostatnie rozwiązane zadania, żeby móc
odpowiedzieć na wyzwanie natychmiast po wyprodukowaniu bloku.

## Backendy obliczeniowe

| Backend | Kiedy | Uwagi |
|---|---|---|
| `cuda-fast` | GPU NVIDIA, domyślny | rozkład na 8-bitowe człony, fp32, bloki po 256 kolumn |
| `cuda-fp64` | `--precision fp64` | podwójna precyzja, wolniejsza na kartach GeForce |
| `cpu` | brak GPU | NumPy fp64 przez BLAS |

Wszystkie trzy dają **identyczny wynik bit w bit** — to warunek, żeby węzeł mógł
w ogóle zweryfikować dowód. TF32 jest jawnie wyłączony, bo obcinałby mantysę
i psuł dokładność całkowitoliczbową.

Przy starcie backend porównuje wybrane wiersze z referencją fp64. Jeśli karta
liczy niepoprawnie (podkręcona pamięć, wadliwy sterownik), koparka **odmawia
startu** zamiast produkować dowody, które sieć odrzuci.

## Diagnostyka

**`OFFLINE`** — sprawdź `curl http://<ip>:26657/health` z maszyny koparki.

**`solution rejected: ... difficulty`** — praca była liczona na starym
wierzchołku; inny górnik był szybszy. Normalne, nic nie tracisz.

**`backend cuda-fast produced an incorrect row`** — uruchom z `--precision fp64`;
jeśli błąd zostaje, cofnij podkręcenie GPU.

**Niski hashrate przy dobrej karcie** — sprawdź, czy `--device` pokazuje `cuda-*`.
Jeśli widnieje `cpu`, PyTorch nie widzi karty: `python -c "import torch; print(torch.cuda.is_available())"`.

## Budowanie binarki do dystrybucji

```bash
pip install pyinstaller
python build_release.py           # ~40 MB, wersja CPU (zalecana do dystrybucji)
python build_release.py --gpu     # ~2,5 GB, z wbudowanym CUDA-PyTorch
```

PyInstaller nie kompiluje skrośnie — buduj na docelowym systemie.
