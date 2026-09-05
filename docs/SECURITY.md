# Bezpieczeństwo — model zagrożeń, ustalenia i zakres audytu

**Ten kod nie przeszedł audytu zewnętrznego.** Audyt zewnętrzny to z definicji
praca kogoś, kto nie pisał tego kodu — autor sprawdzający własną robotę nie jest
audytem, tylko przeglądem. Poniżej jest to, czym naprawdę dysponujesz: rzetelny
przegląd wewnętrzny z ustaleniami, oraz dokument zakresu, który firma audytorska
i tak od Ciebie zażąda.

---

## 1. Ustalenia przeglądu wewnętrznego

Znalezione i **naprawione**:

### KRYTYCZNE — sieć zatrzymywała się na stałe przy pierwszym retargecie
*Naprawione w [`d7e5ae1`](https://github.com/xeniak123/cognitivechain/commit/d7e5ae1)*

Okno retargetu obejmuje `interval` bloków, więc pierwszy z nich leży
`interval − 1` kroków od rodzica. Kod cofał się o pełne `interval`, co przy
**pierwszym** retargecie trafiało jeden blok poniżej genesis. `cog_getWork`
zwracał błąd i żaden węzeł nie mógł już wyprodukować bloku — deterministycznie,
bo to własność wysokości, nie węzła.

Na mainnecie: cel 30 s, okno 60 bloków → **sieć umiera ok. pół godziny po
starcie**, z wyemitowanym premine i bez możliwości wykonania przelewu.

Wykryte przez uruchomienie devnetu powyżej bloku 60. Żaden wcześniejszy test ani
przebieg CI nie przekroczył wysokości 30 — dlatego błąd zabójczy dla mainnetu
przeżył zielony pipeline. Test regresyjny
`the_chain_survives_its_retarget_boundaries` przechodzi teraz przez dwie granice.

### WYSOKIE — transakcje można było powtórzyć między sieciami
*Naprawione w [`60f465e`](https://github.com/xeniak123/cognitivechain/commit/60f465e)*

Podpisywane bajty obejmowały nadawcę, odbiorcę, kwotę, opłatę, nonce i memo —
ale **nie łańcuch**. Transakcja podpisana w jednej sieci była bajt w bajt ważna
w każdej innej, gdzie ten sam adres miał saldo przy tym samym nonce.

To nie było ryzyko teoretyczne: dokumentacja tego projektu każe testować
wszystko najpierw na devnecie, zakładanym tym samym `cog-node keygen` i
w praktyce często tym samym portfelem. Każdy przelew wykonany na devnecie był
podpisaną, gotową do powtórzenia instrukcją wobec salda na mainnecie.

`chain_id` jest teraz polem transakcji i częścią podpisywanego preimage'u
(`cog/tx/v2`). Węzeł wstawia własny `chain_id`, więc podpis z innej sieci nie
przechodzi **weryfikacji**, a nie tylko polityki — różnica ma znaczenie, bo
politykę można obejść, łącząc się z węzłem bezpośrednio.

---

## 2. Znane problemy otwarte

Nienaprawione, świadomie. Kolejność według tego, co zrobiłbym najpierw.

### ŚREDNIE/WYSOKIE — reorganizacja odtwarza łańcuch od genesis
`chain.rs`, `accept_block` i `rebuild_state_to`

Blok trafiający na gałąź boczną powoduje odtworzenie **całej** gałęzi od bloku
zerowego, żeby zbudować stan do walidacji. Koszt rośnie liniowo z długością
łańcucha. Peer może wysyłać kolejne poprawne bloki na tanią gałąź boczną i za
każdym razem wymusić pełne odtworzenie.

Przy wysokości 1 000 bloków to niezauważalne. Przy 100 000 — jeden pakiet
kosztuje węzeł sekundy pracy CPU. **To jest wektor DoS, który dojrzewa razem
z siecią.**

Rozwiązanie: okresowe migawki stanu i cofanie tylko do najbliższego wspólnego
przodka zamiast do genesis.

### ŚREDNIE — okno otwarcia zobowiązania wynosi dokładnie jeden blok
`pouw.rs`, `REVEAL_WINDOW = 1`

Górnik, którego otwarcie nie dotrze do węzła przed kolejnym blokiem, traci całą
nagrodę. W modelu bez puli górnicy zależą od jednego węzła, do którego wysyłają
rozwiązania — **operator tego węzła może celowo pomijać otwarcia konkurencji**,
przejmując ich nagrody bez łamania żadnej reguły konsensusu.

Łagodzi to fakt, że nagroda przepada, a nie trafia do pomijającego. Ale to nadal
griefing o zerowym koszcie.

Rozwiązanie wymaga rozdzielenia bloku, który przyjmuje otwarcie, od tego, kto go
propaguje — czyli propagacji otwarć przez P2P, nie tylko przez RPC węzła.

### ŚREDNIE — brak limitowania zapytań na RPC i P2P
`rpc.rs`, `p2p.rs`

Publiczny endpoint 26657 nie ma limitów ani uwierzytelnienia. Zgłoszenie
nieprawidłowego rozwiązania kosztuje węzeł jeden skrót (tanio), ale
`cog_getAddressHistory` ze `scan_depth: 100000` kosztuje odczyt 100 tysięcy
bloków — **to jest tanie do wysłania i drogie do obsłużenia**.

Peer wysyłający nieprawidłowe bloki zostaje rozłączony, ale może natychmiast
wrócić: nie ma listy banów ani punktacji reputacji.

Obejście na dziś: nginx albo Caddy z limitem przed portem 26657.

### NISKIE — adresy bez sumy kontrolnej
`types.rs`, `Address::parse`

Literówka daje adres poprawny składniowo, prowadzący do środków nie do
odzyskania. `Address::parse` przyjmuje też adresy bez prefiksu `cog`.
Docelowo: bech32 z sumą kontrolną.

### NISKIE — manipulacja znacznikiem czasu
`chain.rs`, `validate_block`

Znacznik musi rosnąć i nie przekraczać `teraz + 120 s`. Górnik może przesuwać go
w tym oknie, żeby lekko wpłynąć na retarget. Wpływ jest ograniczony przez
zacisk ±4× na okno i przez to, że okno obejmuje 60 bloków.

### NISKIE — mempool nie jest trwały
`mempool.rs`

Restart węzła gubi niepotwierdzone transakcje. To utrudnienie, nie luka, ale
integracje muszą rozgłaszać ponownie.

---

## 3. Model zagrożeń

### Co ten system chroni

1. **Integralność podaży.** Nigdy nie może powstać więcej niż 1 000 000 000 COG.
   Egzekwowane dwukrotnie: przy walidacji genesis i przy każdej emisji, plus
   niezmiennik `minted ≤ supply_cap` po zastosowaniu bloku.
2. **Powiązanie nagrody z pracą.** Nagroda powstaje wyłącznie po zweryfikowanym
   otwarciu zobowiązania. Praca pominięta oznacza nagrodę niewyemitowaną.
3. **Autoryzację przelewów.** Tylko posiadacz klucza prywatnego może wydać
   środki; nonce zapobiega powtórzeniu w obrębie sieci, `chain_id` — między
   sieciami.
4. **Zgodność stanu.** Każdy węzeł niezależnie odtwarza korzeń stanu; rozbieżność
   oznacza odrzucenie bloku.

### Przed kim nie chroni

- **Atak 51%.** Koszt jest proporcjonalny do mocy sieci. Przy starcie moc jest
  mała, więc **koszt reorganizacji jest niski**. To nie jest wada implementacji,
  tylko własność każdego młodego PoW — ale giełdy i użytkownicy muszą dobierać
  liczbę potwierdzeń do realnej mocy, nie do tabelki.
- **Operatora puli.** Pula trzyma cudze środki na jednym kluczu.
- **Utraty klucza.** Nie ma odzyskiwania, nie ma resetu.
- **Węzła jako cenzora.** Węzeł może odrzucać transakcje i otwarcia. Zaradza temu
  wiele niezależnych węzłów, nie kod.

### Granica zaufania

| Element | Zaufanie |
|---|---|
| Twój węzeł | pełne — waliduje wszystko sam |
| Cudzy węzeł (jako `--pool`) | wysokie — widzi Twoje rozwiązania i może je pominąć |
| Pula | wysokie — trzyma Twoje środki do wypłaty |
| Peer P2P | zerowe — każdy blok jest walidowany od zera |
| Koparka | zerowe z punktu widzenia sieci — dowód jest sprawdzany |

---

## 4. Zakres dla audytora zewnętrznego

Do wysłania firmie audytorskiej razem z repozytorium.

### Priorytet 1 — konsensus (ok. 1 800 linii Rusta)

| Plik | Co dokładnie sprawdzić |
|---|---|
| `node/src/pouw.rs` | Poprawność i niepodrabialność schematu commit–reveal. Czy 32 wiersze wystarczą? Czy wyzwanie jest naprawdę nieprzewidywalne w chwili zobowiązania? Czy da się grindować znacznik czasu, żeby trafić w policzone wiersze? |
| `node/src/state.rs` | Emisja i limit podaży. Przepełnienia. Czy `apply_block` jest deterministyczne dla każdego węzła? |
| `node/src/chain.rs` | Walidacja bloków, retarget, wybór łańcucha, reorganizacja. Tu znaleziono błąd krytyczny — traktować jako obszar podwyższonego ryzyka. |
| `node/src/types.rs` | Preimage'y hashy, `meets_difficulty`, bajty podpisu. |

### Priorytet 2 — środki i sieć

`node/src/pool.rs` (księgowość, wypłaty, weryfikacja udziałów),
`node/src/wallet_ui.rs` (podpisywanie lokalne), `node/src/p2p.rs`,
`node/src/mempool.rs`.

### Priorytet 3 — zgodność implementacji

`miner/cog_miner/protocol.py` musi zgadzać się z `node/src/pouw.rs` **co do
bajtu**. Rozbieżność oznacza dowody odrzucane przez sieć albo — gorzej —
przyjmowane przez część węzłów.

### Konkretne pytania, na które chcę odpowiedzi

1. Czy 4 wiersze spot-checku w puli to wystarczający margines przy założeniu
   racjonalnego, nastawionego na zysk oszusta?
2. Czy odtwarzanie gałęzi od genesis da się wykorzystać do DoS-u taniej, niż
   szacuję?
3. Czy pominięcie otwarcia zobowiązania przez węzeł da się wykorzystać
   dochodowo, a nie tylko destrukcyjnie?
4. Czy arytmetyka nad GF(65521) jest dokładna na każdym realnym GPU, czy
   znajdzie się sprzęt łamiący determinizm?
5. Czy zacisk ±4× przy retargecie jest odporny na manipulację znacznikami czasu
   przy niskiej mocy sieci?

### Materiały

- Specyfikacja co do bajtu: [PROTOCOL.md](PROTOCOL.md)
- Testy: `node/tests/mining.rs` (8 integracyjnych, w tym cztery próby oszustwa),
  18 jednostkowych
- Odtworzenie: `cargo test --release`, `cargo run --release -- selftest`
- CI: pełny przebieg devnetu weryfikujący księgowość nagród

### Czego audyt nie obejmie

Ekonomii tokena, zgodności regulacyjnej, bezpieczeństwa operacyjnego kluczy
alokacji genesis. To osobne dziedziny i osobni specjaliści.

---

## 5. Zgłaszanie podatności

Zgłoszenia: Issues w repozytorium
https://github.com/xeniak123/cognitivechain/issues

Do czasu uruchomienia sieci z realną wartością nie ma sensu udawać procesu
odpowiedzialnego ujawniania z embargiem — nie ma czego chronić poza kodem, który
i tak jest publiczny. Gdy sieć wystartuje, ten rozdział wymaga zastąpienia
prawdziwym adresem kontaktowym, kluczem PGP i zadeklarowanym czasem odpowiedzi.
