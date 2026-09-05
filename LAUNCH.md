# Uruchomienie sieci — runbook

Kod jest gotowy i przetestowany. Zostały cztery rzeczy, w tej kolejności.
Kolejność ma znaczenie: krok 1 zmienia hash genesis, a krok 3 rozdaje ten hash
ludziom, więc odwrócenie ich oznacza, że wszyscy pobiorą nieaktualny plik.

```
1. genesis_time  ──► zmienia hash genesis
2. węzeł seed    ──► daje publiczne IP
3. wydanie       ──► rozdaje binarki + genesis
4. ogłoszenie    ──► ludzie kopią
```

---

## 1. Ustaw moment startu

Plik `genesis/genesis.mainnet.json` ma `genesis_time = 1893456000`, czyli
1 stycznia 2030. To celowa blokada: blok 1 musi mieć znacznik czasu większy od
genesis i nie większy od „teraz", więc **do tej daty nie powstanie ani jeden
blok**. Sieć wygląda wtedy na zawieszoną.

**Start natychmiast po uruchomieniu węzła** — przegeneruj plik, `genesis-template`
wstawia bieżący czas:

```bash
cd cognitivechain
./node/target/release/cog-node genesis-template \
  --out genesis/genesis.mainnet.json \
  --chain-id cognitivechain-1 \
  --founders  cog523fe4ffffb34e4dd244b2e2cc5a543e812ac802 \
  --ecosystem cogfac8b9ec001fe09c3ef42356b085bfce8ceeba7a \
  --liquidity cog231925e3b8bbdc093f98f8e7e34a6a5da862dc71 \
  --initial-difficulty 5000000 --block-time 30
```

**Start zaplanowany na konkretną godzinę** — podmień samo pole. Sieć wystartuje
sama, gdy nadejdzie ta chwila; węzły mogą stać uruchomione wcześniej:

```bash
# przykład: 1 marca 2027, 18:00 UTC
python -c "import json,collections; p='genesis/genesis.mainnet.json'; \
d=json.load(open(p),object_pairs_hook=collections.OrderedDict); \
d['genesis_time']=1804615200; json.dump(d,open(p,'w'),indent=2)"
```

Sprawdź wynik — pierwsza linia nie może mówić „in the FUTURE", jeśli sieć ma
ruszyć od razu:

```bash
./node/target/release/cog-node inspect-genesis --genesis genesis/genesis.mainnet.json
```

Skopiuj plik dla Dockera, zacommituj i **zapisz nowy hash genesis** — to on
identyfikuje sieć i tylko węzły z tym samym hashem się połączą:

```bash
cp genesis/genesis.mainnet.json docker/config/genesis.json
git add genesis/ docker/config/ && git commit -m "genesis: set launch time" && git push
```

### O `initial_difficulty`

`5000000` znaczy „około 5 mln prób skrótu na blok". Jedna koparka na CPU robi
~1,8 mln prób na sekundę, więc pierwsze bloki polecą co ~3 sekundy zamiast co 30.
To nie jest problem: po 60 blokach retarget podnosi trudność (maks. ×4 na okno),
więc po kilkunastu minutach sieć sama trafi w docelowe 30 s. Wolno raczej zacząć
za nisko niż za wysoko — przy zbyt wysokiej wartości nikt nie znajdzie bloku
i retarget nigdy nie zadziała.

---

## 2. Postaw węzeł seed

Potrzebujesz VPS z **publicznym IP**. Bez tego nikt się nie połączy — to jest
prawdziwy warunek konieczny całej reszty. Wystarczy najtańsza maszyna: 2 rdzenie,
2 GB RAM, 20 GB dysku (Hetzner CX22, DigitalOcean, OVH — rząd wielkości 5 €/mies.).

Na świeżym Debianie/Ubuntu jedna komenda:

```bash
curl -fsSL https://raw.githubusercontent.com/xeniak123/cognitivechain/main/scripts/install-node.sh | sudo bash
```

Skrypt instaluje Dockera, klonuje repozytorium, kopiuje genesis, startuje węzeł,
otwiera porty 26656/26657, czeka na `/health` i na koniec wypisuje gotową komendę
dla górników. Jest idempotentny — ponowne uruchomienie aktualizuje kod bez
kasowania łańcucha.

Drugi węzeł, wskazujący na pierwszy:

```bash
curl -fsSL https://raw.githubusercontent.com/xeniak123/cognitivechain/main/scripts/install-node.sh \
  | sudo bash -s -- --peer <IP_PIERWSZEGO>:26656
```

Postaw **co najmniej dwa** w różnych lokalizacjach. Jeden węzeł to jeden punkt
awarii: gdy padnie, sieć się zatrzymuje, bo górnicy nie mają gdzie wysyłać
rozwiązań. Sprawdź, że oba widzą to samo:

```bash
for ip in <IP_1> <IP_2>; do
  curl -s -X POST http://$ip:26657 -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"cog_status","params":{}}' \
    | python -c "import sys,json; d=json.load(sys.stdin)['result']; print(d['height'], d['tip_hash'][:16])"
done
```

Dwie identyczne linie = sieć spójna.

> **Otwarty RPC = otwarte kopanie.** Węzeł nie pobiera prowizji — kto wyśle
> zwycięskie rozwiązanie, ten dostaje całą nagrodę. Port 26657 wystawiony
> publicznie nie ma limitowania zapytań, więc na produkcji postaw przed nim
> nginx albo Caddy z rate limitem i TLS.

---

## 3. Opublikuj wydanie

Binarki są już zbudowane — tag `v1.0.0` przeszedł oba buildy i czeka jako
**szkic** wydania:

**https://github.com/xeniak123/cognitivechain/releases**

Zanim klikniesz „Publish release":

- [ ] `genesis_time` z kroku 1 jest już w repozytorium
- [ ] plik `genesis.json` dołączony do wydania ma **nowy** hash — jeśli tagowałeś
      przed zmianą genesis, skasuj tag i zrób go jeszcze raz:
      ```bash
      git tag -d v1.0.0 && git push origin :refs/tags/v1.0.0
      git tag -a v1.0.0 -m "CognitiveChain mainnet" && git push origin v1.0.0
      ```
- [ ] w opisie wydania jest hash genesis, żeby ludzie mogli sprawdzić, do jakiej
      sieci się podłączają
- [ ] sumy z `SHA256SUMS-*.txt` zgadzają się z plikami

Wydanie daje ludziom cztery pliki na system: `cog-miner`, `cog-node`,
`genesis.json` i sumy kontrolne.

---

## 4. Powiedz ludziom, jak zacząć

Ten fragment wklej do ogłoszenia — jest napisany dla kogoś, kto nie zna projektu.

> **Jak kopać COG**
>
> **1. Załóż portfel.** Pobierz `cog-node` z
> [Releases](https://github.com/xeniak123/cognitivechain/releases) i uruchom:
> ```
> cog-node keygen --out wallet.json
> ```
> Zapisz wypisany adres `cog...` i zrób kopię `wallet.json` — bez tego pliku
> stracisz dostęp do wykopanych monet. Nikt nigdy nie poprosi Cię o jego treść.
>
> **2. Kop.** Linux:
> ```
> curl -fsSL https://raw.githubusercontent.com/xeniak123/cognitivechain/main/scripts/mine.sh \
>   | bash -s -- --wallet cog<TWÓJ_ADRES> --pool <IP_WĘZŁA>
> ```
> Windows (PowerShell):
> ```
> irm https://raw.githubusercontent.com/xeniak123/cognitivechain/main/scripts/mine.ps1 -OutFile mine.ps1
> .\mine.ps1 -Wallet cog<TWÓJ_ADRES> -Pool <IP_WĘZŁA>
> ```
> Skrypty pobierają koparkę, **sprawdzają sumę kontrolną** i odmawiają
> uruchomienia pliku, który się nie zgadza.
>
> **3. Masz kartę NVIDIA?** Doinstaluj PyTorch z CUDA —
> [pytorch.org](https://pytorch.org/get-started/locally/) — koparka wykryje ją
> sama i przełączy się na GPU.
>
> **4. Saldo:**
> ```
> cog-node balance --address cog<TWÓJ_ADRES> --rpc <IP_WĘZŁA>:26657
> ```

---

## Czego pilnować w pierwszej dobie

| Co | Jak sprawdzić | Co znaczy problem |
|---|---|---|
| Bloki powstają | `cog_status`, rosnąca `height` | stoi na 0 → `genesis_time` w przyszłości albo nikt nie kopie |
| Trudność się dostraja | `difficulty` po 60, 120, 180 blokach | nie rośnie → za mało górników na okno retargetu |
| Nagrody się wypłacają | `tasks_completed` rośnie razem z `height` | rośnie wolniej → górnicy nie zdążają z otwarciem zobowiązań |
| Węzły są zgodne | `tip_hash` na każdym węźle | rozjazd → rozłączony P2P albo różny genesis |
| Podaż | `cog_getSupply` | `minted` musi rosnąć o 45 COG na zadanie, nigdy inaczej |

Rozjazd między `height` a `tasks_completed` to najciekawszy sygnał: znaczy, że
górnicy zdobywają bloki, ale nie otwierają zobowiązań, więc **tracą nagrody**.
Najczęstsza przyczyna to bardzo wysokie opóźnienie sieciowe do węzła.

---

## Czego jeszcze nie ma

Rzeczy, o które ludzie zapytają, a których w tym repozytorium nie znajdziesz:

- **Eksplorator bloków.** Jest RPC `cog_getBlock`, ale nie ma interfejsu.
- **Portfel graficzny.** Tylko CLI.
- **Pula wydobywcza z podziałem nagród.** Każdy górnik dostaje całą nagrodę za
  swój blok albo nic — nie ma mechanizmu dzielenia się między uczestnikami.
- **Giełda.** Notowanie COG to osobny proces, niezależny od kodu.
- **Audyt zewnętrzny.** Kod jest przetestowany, ale nieaudytowany. Przed
  jakąkolwiek realną wartością to krok obowiązkowy.
