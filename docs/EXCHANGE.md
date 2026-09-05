# Integracja giełdowa — dokumentacja techniczna

Dokument dla zespołu integracyjnego giełdy. Zawiera wszystko, co jest potrzebne
do obsługi wpłat, wypłat i sald COG.

**Czego tu nie ma:** samego procesu notowania. Listing to decyzja giełdy oparta
na due diligence, podmiocie prawnym, opłatach i płynności — nie na kodzie.
Ten dokument sprawia, że część techniczna przestaje być przeszkodą.

---

## 1. Podstawowe parametry

| | |
|---|---|
| Nazwa | CognitiveChain |
| Symbol | COG |
| `chain_id` | `cognitivechain-1` |
| Hash genesis | patrz `cog-node inspect-genesis`; publikowany przy starcie sieci |
| Miejsca dziesiętne | **8** (1 COG = 100 000 000 acog) |
| Maksymalna podaż | 1 000 000 000 COG, twardo zakodowana |
| Docelowy czas bloku | 30 s |
| Konsensus | Proof-of-Useful-Work, najcięższy łańcuch |
| Model kont | kontowy (jak Ethereum), nie UTXO |
| Podpisy | ed25519 (RFC 8032) |
| Format adresu | `cog` + 40 znaków hex, razem 43 znaki |
| Minimalna opłata | `min_tx_fee_acog` z genesis, domyślnie 10 000 acog (0,0001 COG) |
| Smart kontrakty | brak — tylko przelewy z polem `memo` |
| Token natywny | tak, COG jest walutą łańcucha; nie ma tokenów pochodnych |

Adresy **nie mają sumy kontrolnej**. Literówka daje adres poprawny składniowo,
prowadzący do środków nie do odzyskania. Waliduj wyrażeniem
`^cog[0-9a-fA-F]{40}$` i wymagaj potwierdzenia adresu przy wypłacie.

---

## 2. Uruchomienie węzła

```bash
docker run -d --name cog-node -p 26657:26657 -p 26656:26656 \
  -v cog-data:/data -v $PWD/genesis.json:/config/genesis.json:ro \
  cognitivechain/node:1.0.0
```

Albo ze źródeł — patrz [DEPLOYMENT.md](../DEPLOYMENT.md).

Węzeł nie przechowuje żadnego klucza prywatnego. Klucze gorącego portfela
trzymaj poza nim.

**Zasoby:** 2 rdzenie, 2 GB RAM, 20 GB dysku wystarczają. Weryfikacja bloku to
ok. 10 ms CPU.

---

## 3. Wpłaty

### 3.1 Model

Konta, nie UTXO. Każdy użytkownik dostaje własny adres depozytowy; środki
zamiatasz na gorący portfel zwykłym przelewem.

### 3.2 Wykrywanie

Nie ma indeksu po adresach ani subskrypcji zdarzeń. Skanuj bloki:

```bash
curl -s -X POST http://127.0.0.1:26657 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"cog_getBlock","params":{"height":1234}}'
```

W praktyce: pobieraj `cog_status.height`, przetwarzaj bloki po kolei, dla każdego
czytaj transakcje i dopasowuj `to` do swoich adresów depozytowych. Zapisuj
ostatnią przetworzoną wysokość i hash bloku.

Dla pojedynczego adresu jest skrót:

```json
{"method":"cog_getAddressHistory","params":{"address":"cog…","limit":100,"scan_depth":5000}}
```

Odpowiedź zawiera `scanned_to_height` i `complete` — **sprawdzaj `complete`**,
inaczej weźmiesz uciętą historię za pełną.

### 3.3 Potwierdzenia

Wybór łańcucha to najcięższa gałąź, więc reorganizacja jest możliwa. Rekomendacja:

| Kwota | Potwierdzenia | Czas |
|---|---|---|
| < 100 COG | 12 | ~6 min |
| 100–10 000 COG | 30 | ~15 min |
| > 10 000 COG | 60 | ~30 min |

To ostrożne wartości dla młodej sieci o nieustabilizowanej mocy. Zweryfikuj je
po kilku tygodniach obserwacji rzeczywistej głębokości reorganizacji.

### 3.4 Obsługa reorganizacji

Węzeł adoptuje cięższą gałąź i przebudowuje stan. Twój indekser musi to wykryć:
zapisuj hash każdego przetworzonego bloku i przy każdym kroku sprawdzaj, czy
`cog_getBlock{height}` nadal zwraca ten sam hash. Jeśli nie — cofnij się do
ostatniej zgodnej wysokości i przetwórz ponownie.

---

## 4. Wypłaty

### 4.1 Podpisywanie

`chain_id` **jest częścią podpisywanych bajtów**. Podpis złożony dla innej sieci
nie przejdzie weryfikacji — to celowa ochrona przed powtórzeniem transakcji
między sieciami. Pełny preimage: [PROTOCOL.md §2](PROTOCOL.md#2-adresy-i-podpisy).

```
"cog/tx/v2" ‖ chain_id_len_le32 ‖ chain_id ‖ pubkey[32] ‖ to[20]
            ‖ amount_le64 ‖ fee_le64 ‖ nonce_le64 ‖ memo_len_le32 ‖ memo
```

Podpisujesz to ed25519 kluczem gorącego portfela. Nie ma tu nic niestandardowego
— każda biblioteka ed25519 wystarczy.

### 4.2 Rozgłoszenie

```json
{"method":"cog_sendTransaction","params":{
  "pubkey":"<64 hex>","to":"cog…","amount":123456789,
  "fee":10000,"nonce":42,"memo":"","signature":"<128 hex>"}}
```

Kwoty jako liczby całkowite w acog. Odpowiedź zawiera `tx_hash`.

### 4.3 Nonce

Nonce jest ściśle sekwencyjny per konto i musi być **dokładnie** równy bieżącemu
nonce konta. Mempool toleruje wyprzedzenie do 16, ale bloki wymagają dokładnego
dopasowania.

Przy wypłatach seryjnych przydzielaj nonce lokalnie (`n`, `n+1`, `n+2`…), **nie**
odczytuj go z węzła przed każdą transakcją — dwie transakcje z tym samym nonce
oznaczają, że tylko jedna kiedykolwiek wejdzie do bloku.

### 4.4 Potwierdzenie

Transakcja jest ostateczna, gdy znajdzie się w bloku i narośnie nad nim
odpowiednia liczba potwierdzeń. Nie ma mechanizmu zastąpienia ani anulowania
transakcji z mempoola — transakcja albo wejdzie, albo wygaśnie razem z restartem
węzła (mempool nie jest trwały).

---

## 5. Sprawdzanie salda

```json
{"method":"cog_getBalance","params":{"address":"cog…"}}
```

```json
{"balance_acog":"5000000000","balance_cog":"50.00000000","nonce":3}
```

Kwoty w acog są **stringami** — u64 nie mieści się bezpiecznie w liczbie JSON.
Parsuj je jako liczby całkowite o dowolnej precyzji, nie jako float.

Podaż w obiegu:

```json
{"method":"cog_getSupply","params":{}}
```

---

## 6. Pełne API

Zestawienie metod: [README](../README.md#api-json-rpc). Specyfikacja
protokołu co do bajtu: [PROTOCOL.md](PROTOCOL.md) — wystarczająca do napisania
niezależnej implementacji.

Węzeł serwuje też eksplorator bloków pod `GET /` na tym samym porcie, przydatny
przy debugowaniu integracji.

---

## 7. Ryzyka, o których giełda powinna wiedzieć

Uczciwe zestawienie — lepiej, żeby padło od nas niż z due diligence:

- **Kod nie przeszedł audytu zewnętrznego.** Jest przetestowany i uruchomiony,
  nieaudytowany. Patrz [SECURITY.md](SECURITY.md).
- **Sieć jest młoda i moc obliczeniowa mała.** Koszt ataku 51% jest
  proporcjonalny do mocy sieci; przy starcie jest niski. Dobierz liczbę
  potwierdzeń do realnej mocy, nie do tabelki wyżej.
- **Brak sumy kontrolnej w adresach.** Literówka to bezpowrotna strata.
- **P2P bez szyfrowania i bez odkrywania węzłów.** Statyczna lista peerów.
- **Mempool nie jest trwały.** Restart węzła gubi niepotwierdzone transakcje;
  rozgłaszaj ponownie, jeśli transakcja nie weszła.
- **Premine 10%** w trzech adresach; ich rozkład jest jawny w pliku genesis.
- **Nagroda za blok emitowana jest dopiero po otwarciu zobowiązania** w kolejnym
  bloku. Blok bez otwarcia oznacza nagrodę, która nigdy nie powstała — to nie
  jest anomalia księgowa, tylko projekt.

---

## 8. Kontakt techniczny

Repozytorium: https://github.com/xeniak123/cognitivechain
Zgłoszenia: przez Issues w repozytorium.
