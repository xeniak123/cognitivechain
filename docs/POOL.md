# Pula wydobywcza

Zamienia loterię „cały blok albo nic" na regularny dochód proporcjonalny do
włożonej pracy.

```
cog-miner ──udziały──► cog-pool ──zwycięskie rozwiązania──► cog-node ──► sieć
    ▲                     │
    └──wypłaty COG────────┘
```

---

## Kiedy pula ma sens

Solo górnik z 10% mocy sieci przy blokach co 30 s trafia blok średnio co
5 minut — wariancja jest do zniesienia i pula tylko dokłada zaufaną stronę
trzecią. Sens pojawia się, gdy trudność urośnie na tyle, że pojedynczy górnik
czeka na blok godzinami albo dniami. **Na starcie sieci pula nie jest potrzebna.**

Osobna sprawa: pula to konto, na którym leżą cudze pieniądze. Operator może je
zabrać. To ograniczenie modelu, nie tej implementacji.

---

## Dlaczego udziału nie da się tu policzyć tak jak w Bitcoinie

W łańcuchu opartym na haszowaniu udział sam się dowodzi: ten sam skrót, który
mógłby wygrać blok, jest dowodem wykonanej pracy. Tutaj pracą jest mnożenie
macierzy, a skrót proof-of-work **nie mówi nic** o tym, czy ta macierz w ogóle
została policzona. Górnik mógłby zgłaszać losowe korzenie Merkle z losowymi
nonce'ami i zbierać udziały za darmo — grindowanie samego skrótu przy trudności
udziału rzędu 50 000 kosztuje ułamek milisekundy.

Dlatego pula stosuje ten sam mechanizm co łańcuch, w mniejszej skali: **każdy
udział dostaje wyzwanie na 4 losowe wiersze**, które pula przelicza sama.
Wyzwanie jest losowane z własnej entropii puli **po** nadejściu udziału, więc
nie da się go przewidzieć. Górnik liczący ułamek `f` wierszy przechodzi jedno
sprawdzenie z prawdopodobieństwem `f⁴`, a **trzy nieudane otwarcia oznaczają
bana** i przepadek nierozliczonego salda.

Koszt weryfikacji: ~4 ms na udział. Przy 100 udziałach na sekundę to 40% jednego
rdzenia — pula skaluje się do kilkuset górników na jednym VPS-ie.

### Sprawdzone empirycznie

Oba realne warianty oszustwa zostały przetestowane na działającej puli:

| Atak | Wynik |
|---|---|
| Losowy korzeń Merkle, zero pracy | `row 0 fails its Merkle proof` → strike |
| **Poprawne** drzewo Merkle nad zmyśloną macierzą | `row 0 does not match the recomputed product` → strike |
| Trzeci strike | `this address is banned` — dalsze udziały odrzucane od razu |

Drugi wariant jest ważniejszy: dowody inkluzji przechodzą i złapać go może
wyłącznie faktyczne przeliczenie wierszy. Właśnie po to pula liczy sama.

---

## Rozliczenia

**Nagrody są odczytywane z łańcucha, nie z salda puli.** Pula skanuje bloki
kanoniczne w poszukiwaniu tych, które sama wykopała i których zobowiązanie
zostało otwarte w kolejnym bloku, i dopisuje dokładnie nagrodę tego bloku plus
jego opłaty. Schemat oparty na obserwowaniu salda ścigałby się z własnymi
wypłatami; ten jest od nich niezależny.

**PPLNS** (*pay per last N shares*): każdy blok dzieli się między ostatnie
`--pplns-window` zweryfikowanych udziałów. Udział zarabia na każdym bloku
znalezionym, dopóki mieści się w oknie. Przeskakiwanie między pulami przestaje
się opłacać, a kto kopie równo, nie traci nic.

Reszta z dzielenia całkowitego i prowizja trafiają do operatora — nic nie znika
z księgi.

**Księga** (`pool-ledger.json`) jest zapisywana przy każdej zmianie, przez plik
tymczasowy i `rename`, więc przerwany zapis nie zostawia połowicznej księgi.
Pula odmawia startu, jeśli księgi nie da się odczytać — utrata tego pliku to
utrata tego, co górnicy mają do odebrania.

---

## Uruchomienie puli

```bash
cog-node keygen --out pool-wallet.json     # portfel puli, zabezpiecz go
cog-pool \
  --key pool-wallet.json \
  --node 127.0.0.1:26657 \
  --bind 0.0.0.0:26659 \
  --share-difficulty 50000 \
  --fee-percent 1 \
  --min-payout 1.0 \
  --payout-interval 120
```

| Flaga | Domyślnie | Uwagi |
|---|---|---|
| `--share-difficulty` | 50 000 | Celuj w kilka udziałów na górnika na minutę. Za wysoka — mali górnicy wyglądają na bezczynnych; za niska — pula tonie w weryfikacji. |
| `--fee-percent` | 1.0 | Prowizja operatora. |
| `--pplns-window` | 10 000 | Ile ostatnich udziałów dzieli nagrodę. |
| `--min-payout` | 1.0 COG | Poniżej tego progu wypłata nie pokrywa własnej opłaty. |
| `--payout-fee` | 0.001 COG | Potrącana z wypłaty, nie dokładana przez pulę. |
| `--payout-interval` | 120 s | Odstęp między rundami wypłat. |

**Portfel puli musi mieć zapas COG na opłaty transakcyjne.** Wypłaty idą jedna
po drugiej z rosnącym lokalnie nonce'em, maksymalnie 12 na rundę — mempool
toleruje ograniczoną wyprzedzającą lukę nonce'ów.

Panel puli: `http://<IP>:26659` w przeglądarce.

---

## Dla górnika

Zmienia się jedna rzecz — adres w `--pool`:

```bash
cog-miner --wallet cog<TWÓJ_ADRES> --pool <IP_PULI>:26659
```

Koparka sama wykryje, że rozmawia z pulą: `cog_getWork` zwraca wtedy
`mining_address`, więc zadania są liczone pod adresem puli, a Twój adres służy
tylko do przypisania udziału. W panelu pojawi się dodatkowa linia
`pula  zadania pod cog…` i licznik przyjętych udziałów.

Saldo w puli sprawdzisz tak:

```bash
curl -s -X POST http://<IP_PULI>:26659 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"pool_getBalance","params":{"miner":"cog<TWÓJ_ADRES>"}}'
```

Odpowiedź zawiera `unpaid_cog`, `paid_cog`, liczbę udziałów w oknie PPLNS oraz
`strikes` — jeśli to nie zero, Twoja koparka liczy niepoprawnie i warto ją
uruchomić z `--precision fp64`.

---

## Czego pula nie robi

- **Nie jest odporna na nieuczciwego operatora.** Trzyma cudze środki na jednym
  kluczu.
- **Nie ma trudności zmiennej per górnik** (vardiff). Jedna trudność udziału dla
  wszystkich, więc bardzo słaby i bardzo mocny sprzęt nie są obsłużone równie
  wygodnie.
- **Nie ma kont, haseł ani panelu operatora.** Adres portfela jest jedyną
  tożsamością.
- **Nie przeszła testu na sieci publicznej.** Cykl przetestowano end-to-end
  lokalnie: 690 zweryfikowanych udziałów, 9 bloków, poprawny podział 44,10 COG
  przy 2% prowizji i wypłaty na łańcuchu. To nie to samo co tygodnie pracy pod
  realnym obciążeniem — zanim postawisz ją przy prawdziwych pieniądzach,
  uruchom ją najpierw na testnecie.
