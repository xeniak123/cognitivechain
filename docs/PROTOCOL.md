# CognitiveChain — specyfikacja protokołu v1

Dokument opisuje wszystko, co jest potrzebne do napisania niezależnej koparki
albo węzła zgodnego z siecią. Każdy preimage hasha jest podany dokładnie;
odchylenie o jeden bajt oznacza odrzucenie dowodu.

Implementacje referencyjne: `node/src/pouw.rs` (Rust) oraz
`miner/cog_miner/protocol.py` (Python).

---

## 1. Stałe

| Nazwa | Wartość | Znaczenie |
|---|---|---|
| `N` | 1024 | wymiar macierzy |
| `N_LOG2` | 10 | głębokość drzewa Merkle wierszy |
| `P` | 65521 | moduł ciała (największa liczba pierwsza < 2¹⁶) |
| `CHALLENGE_ROWS` | 32 | liczba wierszy otwieranych przy reveal |
| `MAX_NONCE` | 65536 | rozmiar przestrzeni nonce na jedno zadanie |
| `REVEAL_WINDOW` | 1 | otwarcie musi trafić do bloku bezpośrednio następnego |
| `COG` | 100 000 000 | acog w jednym COG |

Funkcja skrótu: **BLAKE3**, wyjście 32 bajty, tryb XOF tam gdzie potrzeba
dłuższego strumienia.

Etykiety domenowe (bajty ASCII, bez terminatora):

```
"cog/task/v1"     seed zadania
"cog/matA/v1"     strumień macierzy A
"cog/matB/v1"     strumień macierzy B
"cog/pow/v1"      preimage proof-of-work
"cog/chal/v1"     wybór wierszy wyzwania
"cog/commit/v1"   identyfikator zobowiązania
"cog/tx/v2"       bajty podpisywane transakcji
"cog/header/v1"   preimage nagłówka bloku
"cog/state/v1"    korzeń stanu
"cog/reveal/v1"   korzeń payloadu otwarcia
"cog/genesis/v1"  hash dokumentu genesis
0x00              prefiks liścia Merkle
0x01              prefiks węzła Merkle
```

Liczby całkowite w preimage'ach są **little-endian**, chyba że zaznaczono inaczej.

---

## 2. Adresy i podpisy

```
address = BLAKE3(ed25519_public_key)[0..20]
zapis   = "cog" + hex(address)          # 43 znaki
```

Transakcja podpisywana jest nad:

```
"cog/tx/v2" ‖ chain_id_len_le32 ‖ chain_id ‖ pubkey[32] ‖ to[20]
            ‖ amount_le64 ‖ fee_le64 ‖ nonce_le64 ‖ memo_len_le32 ‖ memo
```

Podpis: ed25519 (RFC 8032), 64 bajty. `nonce` musi być równy bieżącemu nonce
konta; `memo` maksymalnie 256 bajtów; przelew do samego siebie jest odrzucany.

> **`chain_id` wchodzi do podpisu i to nie jest ozdobnik.** Bez niego transakcja
> podpisana na devnecie byłaby bajt w bajt ważna na mainnecie — a to ten sam
> portfel, którym testujesz, zanim uruchomisz sieć produkcyjną. Węzeł wstawia
> własny `chain_id` przy przyjmowaniu transakcji, więc podpis złożony dla innej
> sieci po prostu nie przechodzi weryfikacji.

---

## 3. Zadanie Proof-of-Useful-Work

### 3.1 Seed

```
task_seed = BLAKE3("cog/task/v1" ‖ prev_hash[32] ‖ miner[20] ‖ salt_le64)
```

`salt` jest wybierany losowo przez górnika. Ponieważ w seed wchodzi jego adres,
każdy górnik pracuje nad rozłącznym zadaniem — nie ma duplikacji wysiłku.

### 3.2 Macierze

```
strumień_A = BLAKE3_XOF("cog/matA/v1" ‖ task_seed) → 2·N·N bajtów
A[i][j]    = (u16_le(strumień_A[2k], strumień_A[2k+1])) mod P,   k = i·N + j
```

Analogicznie `B` z etykietą `"cog/matB/v1"`. Kolejność wierszowa (row-major).

> Uwaga: `mod P` na wartościach 0..65535 daje lekko niejednorodny rozkład
> (wartości 0..14 są odrobinę częstsze). Nie ma to znaczenia dla
> bezpieczeństwa — macierze nie są tajne, mają jedynie wymuszać pracę.

### 3.3 Iloczyn

```
C[i][j] = ( Σ_{k=0}^{N-1} A[i][k] · B[k][j] ) mod P
```

Akumulator jest ograniczony przez `N·(P−1)² < 2⁴²`, więc jest **dokładny**
zarówno w `u64`, jak i w `float64` (mantysa 53 bity). To jest warunek
konieczny determinizmu między urządzeniami.

### 3.4 Drzewo Merkle wierszy

```
liść(i)  = BLAKE3(0x00 ‖ i_le32 ‖ C[i] jako N × u16_le)
węzeł    = BLAKE3(0x01 ‖ lewy ‖ prawy)
root     = korzeń pełnego drzewa binarnego nad N = 1024 liśćmi
```

`N` jest potęgą dwójki, więc dopełnianie nie występuje. Dowód inkluzji ma
dokładnie `N_LOG2 = 10` skrótów, podanych od dołu do góry.

### 3.5 Proof-of-work

```
pow = BLAKE3("cog/pow/v1" ‖ task_seed ‖ matmul_root ‖ nonce_le64)
```

Warunek trudności, w arytmetyce dokładnej:

```
int_be(pow) × difficulty < 2²⁵⁶      oraz      nonce < MAX_NONCE
```

Węzeł liczy to jako mnożenie 256×64 → 320 bitów ze sprawdzeniem przeniesienia
(`types::meets_difficulty`); w Pythonie wystarczy `int.from_bytes(pow,"big")`.

### 3.6 Identyfikator zobowiązania

```
commit_id = BLAKE3("cog/commit/v1" ‖ task_seed ‖ matmul_root ‖ nonce_le64)
```

Celowo **nie** zależy od nagłówka bloku: nagłówek commituje się do korzenia
stanu, a korzeń stanu zawiera tablicę oczekujących zobowiązań — klucz oparty
o hash bloku byłby cykliczny.

### 3.7 Wyzwanie

```
strumień = BLAKE3_XOF("cog/chal/v1" ‖ hash_bloku_z_zobowiązaniem) → 4·32 bajty
wiersz_s = u32_le(strumień[4s .. 4s+4]) mod N,   s = 0..31
```

Powtórzenia indeksów są dozwolone i sprawdzane normalnie.

Otwarcie musi podać wiersze **w tej samej kolejności**, w jakiej wypadły
w wyzwaniu, każdy z pełnym dowodem Merkle względem zacommitowanego `matmul_root`.

---

## 4. Bloki

### 4.1 Nagłówek

```
"cog/header/v1" ‖ version_le16 ‖ height_le64 ‖ prev_hash[32] ‖ timestamp_le64
                ‖ difficulty_le64 ‖ tx_root[32] ‖ state_root[32] ‖ reveal_root[32]
```

### 4.2 Hash bloku

```
BLAKE3( encode(header) ‖ znacznik ‖ [ miner[20] ‖ salt_le64 ‖ nonce_le64 ‖ matmul_root[32] ] )
```

gdzie `znacznik` = `0x01` gdy blok ma rozwiązanie (wszystkie poza genesis)
albo `0x00` dla genesis (wtedy pola rozwiązania są pominięte).

### 4.3 Reguły walidacji

Blok jest ważny wtedy i tylko wtedy, gdy:

1. `version == 1`
2. `height == rodzic.height + 1` oraz `prev_hash == hash(rodzic)`
3. `timestamp > rodzic.timestamp` i `timestamp ≤ teraz + 120 s`
4. `difficulty` równa się wartości wyliczonej przez retarget
5. `tx_root` i `reveal_root` zgadzają się z zawartością
6. `nonce < MAX_NONCE` i `pow` spełnia warunek trudności
7. wykonanie bloku na stanie rodzica daje `state_root` z nagłówka

### 4.4 Retarget trudności

Co `retarget_interval` bloków (domyślnie 60), licząc wstecz po **tej konkretnej
gałęzi**:

```
actual   = max(1, timestamp(rodzic) − timestamp(rodzic − interval))
expected = interval × target_block_time_secs
next     = clamp(difficulty × expected / actual,  difficulty/4,  difficulty×4)
next     = max(next, 1)
```

Wybór łańcucha: największa suma `difficulty` (skumulowana praca). Przy remisie
wygrywa łańcuch przyjęty wcześniej.

---

## 5. Stan i emisja

### 5.1 Struktura stanu

```
accounts:        address → { balance: u64, nonce: u64 }
minted:          u64      wyemitowane łącznie (premine + nagrody)
tasks_completed: u64      liczba zweryfikowanych zadań
pending:         commit_id → { height, miner, task_seed, matmul_root, reward, expires_at }
```

Korzeń stanu:

```
BLAKE3( "cog/state/v1" ‖ liczba_kont_le64
      ‖ dla każdego konta rosnąco po adresie: address[20] ‖ balance_le64 ‖ nonce_le64
      ‖ minted_le64 ‖ tasks_completed_le64 ‖ liczba_pending_le64
      ‖ dla każdego pending rosnąco po commit_id:
            commit_id[32] ‖ height_le64 ‖ miner[20] ‖ task_seed[32]
          ‖ matmul_root[32] ‖ reward_le64 ‖ expires_at_le64 )
```

`supply_cap` jest stałą genesis i **nie** wchodzi do korzenia stanu.

### 5.2 Kolejność wykonania bloku

1. Zastosuj transakcje po kolei; opłaty sumują się.
2. Jeśli blok ma otwarcie: zweryfikuj je, usuń zobowiązanie, **wyemituj**
   nagrodę górnikowi, `tasks_completed += 1`.
3. Usuń zobowiązania z `expires_at ≤ height` (nagroda przepada bezpowrotnie).
4. Zarejestruj zobowiązanie z tego bloku (`expires_at = height + 1`) i wypłać
   górnikowi zebrane opłaty.

Opłaty są transferem, nie emisją — nie zwiększają `minted`.

### 5.3 Harmonogram emisji

```
reward(tasks_completed) = initial_block_reward >> (tasks_completed / halving_interval_tasks)
```

Przy `initial = 45 COG` i `interval = 10 000 000`:

| Epoka | Zadania | Nagroda | Emisja epoki |
|---|---|---|---|
| 0 | 0 – 10M | 45 COG | 450 000 000 COG |
| 1 | 10M – 20M | 22,5 COG | 225 000 000 COG |
| 2 | 20M – 30M | 11,25 COG | 112 500 000 COG |
| … | … | … | … |
| **Σ** | | | **≈ 900 000 000 COG** |

Plus 100 000 000 COG premine = 1 000 000 000 COG.

Dwa niezależne zabezpieczenia limitu:

* `GenesisConfig::validate` odmawia startu, jeśli suma szeregu przekroczyłaby cap;
* `State::mint` przycina każdą wypłatę do `supply_cap − minted`, a `apply_block`
  na końcu sprawdza niezmiennik `minted ≤ supply_cap`.

---

## 6. JSON-RPC

Endpoint: `POST /`, JSON-RPC 2.0. Dodatkowo `GET /health` → `ok`.

Wszystkie hashe i klucze są w hex bez prefiksu `0x`. Kwoty w acog jako
**stringi** (u64 nie mieści się bezpiecznie w liczbie JSON).

### `cog_getWork`

```json
{"height":1483,"prev_hash":"a339...","difficulty":5241300,
 "matrix_dim":1024,"field_prime":65521,"max_nonce":65536,
 "challenge_rows":32,"chain_id":"cognitivechain-1"}
```

### `cog_submitSolution`

Parametry: `miner`, `salt`, `nonce`, `matmul_root`.
Odpowiedź: `{"status":"accepted","block_hash":"...","height":1483,"commit_id":"..."}`.

### `cog_getRevealRequests`

Parametry: `miner`. Odpowiedź: lista `{commit_id, task_seed, rows}`,
gdzie `rows` to 32 indeksy do otwarcia.

### `cog_submitReveal`

```json
{"commit_id":"...","rows":[{"index":732,
  "values":"<2048 bajtów hex, N × u16 little-endian>",
  "proof":["<32B hex>", ... 10 sztuk]}]}
```

---

## 7. Test zgodności między implementacjami

Poniższy skrypt musi wypisać te same wartości, co `cog-node selftest`:

```python
from cog_miner import protocol as pr
from cog_miner.compute import detect_device, Engine

seed = pr.task_seed(b'\x22' * 32, b'\x11' * 20, 7)
assert seed.hex() == "3614b0a51492d64b22b390ffe27a0a3988ddcb595f83d181b26e1047f14be87e"

engine = Engine(detect_device())
a, b = pr.gen_matrix_a(seed), pr.gen_matrix_b(seed)
c = engine.matmul(a, b)
root = pr.merkle_root(pr.build_leaves(c))
assert root.hex() == "bd3ee4836b4dc78c5a8d3141542a25998ded48ca10f33a68ffae6478a69415e7"
print("zgodność Rust <-> Python: OK")
```

Wektory testowe dla `prev_hash = 0x22×32`, `miner = 0x11×20`, `salt = 7`:

```
task_seed   3614b0a51492d64b22b390ffe27a0a3988ddcb595f83d181b26e1047f14be87e
matmul_root bd3ee4836b4dc78c5a8d3141542a25998ded48ca10f33a68ffae6478a69415e7
```

Wyzwanie dla `hash_bloku = 0x33×32` zaczyna się od:
`[151, 784, 494, 734, 303, 178, 659, 928, ...]`.

---

## 8. P2P

Ramka: długość `u32` little-endian, potem `bincode` z wariantem `Message`.
Limit ramki: 8 MiB.

```
Hello     { chain_id: String, genesis: [u8;32], height: u64 }
NewBlock  ( Box<Block> )
GetBlocks { from_height: u64 }        # zwraca maks. 128 bloków
Blocks    ( Vec<Block> )
```

Handshake wymaga zgodnego `chain_id` **i** hasha genesis. Bloki przed
handshake'em są odrzucane wraz z zerwaniem połączenia.
