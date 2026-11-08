# Solana Event Trader (Rust)

Kompletny klient transakcyjny: Tokio, WSS, filtry kont, rzeczywisty swap exact-in,
podpis Ed25519, priority fee, opcjonalny Jito `sendBundle` i rozliczanie podpisu.
Domyślnie wykonuje wyłącznie symulację bez przekazywania podpisu do RPC.
Wysłanie wymaga `--live`.

**Gotowy test bez faucetu/GitHuba:** w PowerShell uruchom `./start-local.ps1`
lub `./start.ps1 -Local`. Wszystkie adresy są przygotowywane automatycznie
w osobnym `config.local.toml`. Jest to lokalna Solana z fikcyjnymi saldami,
programem SPL i rzeczywistymi powiadomieniami WSS. Bot odpowiada symulacją.
Wersja w tle: `./start-local.ps1 -Background`; zatrzymanie: `./stop-local.ps1`.
Logi w tle: `Get-Content .tools/local-demo.log -Tail 20 -Wait`.

## Zakres adaptera i strategii

Kod obsługuje **SPL Token Swap 3.0.0**, stan `SwapVersion::SwapV1` o długości
324 bajtów, krzywą ConstantProduct i dziesięciokontową instrukcję `Swap`.
Obydwa tokeny muszą używać klasycznego programu SPL Token.
W konfiguracji podajesz sprawdzony program, jedną pulę i kierunek wymiany.
Nie ma przykładowych, zmyślonych adresów wdrożeń.

Strategia jest celowo prosta i jawna: po udanej transakcji, która wspomina
wybraną pulę i wywołuje wskazany program, bot próbuje wymienić `amount_in`
tokenu wejściowego na token wyjściowy, jeżeli filtry pozwolą. Nie ocenia
rentowności. Domyślnie wysyła najwyżej jedną próbę na uruchomienie.
To klient wskazanej puli, a nie automatyczne wykrywanie nowych tokenów.

Pump.fun, Raydium, inne wersje SPL Token Swap i Token-2022 wymagają własnego
adaptera instrukcji, dekodera stanu oraz wyceny. Zmiana samego program ID
nie zapewnia kompatybilności. Sprawdzenie długości konta nie potwierdza ABI
wykonywalnego programu — konfiguruj wyłącznie zweryfikowane wdrożenie.

## Budowanie i uruchomienie

W tym środowisku Windows przygotowano launcher WSL: `./start.ps1` uruchamia
symulację, `./start.ps1 -BuildOnly` buduje release, a `./start.ps1 -Build`
przebudowuje program i uruchamia symulację. Trwały plik wykonywalny trafia do
`dist/linux-x86_64/solana-event-trader`. Wymagane wartości konfiguracji opisano
w [CO_UZUPELNIC.md](CO_UZUPELNIC.md). Launcher nie wymaga Rust w Windows PATH.

Do pierwszego testu bez ręcznego wyszukiwania adresów służy `./setup-devnet.ps1`.
Konfigurator używa wyłącznie devnet, tworzy osobny portfel, tokeny oraz pulę
i zapisuje `config.toml` po udanym swapie sprawdzającym zgodność programu.
Faucet może wymagać ręcznego zasilenia podanego publicznego adresu testowym SOL.
Skrypt jest przygotowany; jego pełny test sieciowy wymaga dostępnego faucetu.

Potrzebny jest stabilny Rust; projekt sprawdzany toolchainem 1.92.0.
Linux/WSL: `build-essential`, `pkg-config`, `libssl-dev`, `ca-certificates`.
Windows natywnie: toolchain MSVC i Visual Studio C++ Build Tools.

```bash
cargo build --release --locked
cargo test --locked
```

`Cargo.toml` przypina bezpośrednie zależności przez `=wersja`.
`Cargo.lock` utrwala też zależności przechodnie i powinien być wersjonowany.

Skopiuj konfigurację i uzupełnij rzeczywiste adresy:

```powershell
Copy-Item config.example.toml config.toml
```

Ustaw HTTP/WSS tego samego klastra, `expected_genesis_hash`, ścieżkę do pliku
keypair w formacie Solana CLI, zweryfikowany program i pulę, oba minty,
istniejące konta tokenowe portfela oraz limity kwot. Wartości `REPLACE...`
są celowo niepoprawne: konfiguracja nie wyśle przypadkowego transferu.
Genesis hash sprawdź niezależnie, np. `solana genesis-hash --url <RPC>`.
Klucz trzymaj poza repozytorium; na Unix ogranicz dostęp do pliku przez
`chmod 600`. Na Windows użyj uprawnień NTFS.

Konto wejściowe musi być zasilone odpowiednim tokenem. Dla SOL użyj istniejącego,
zsynchronizowanego konta **WSOL**; ten adapter nie opakowuje SOL automatycznie.
Konto wyjściowe musi już istnieć i należeć do tego samego portfela.

Symulacja, Linux/WSL:

```bash
RUST_LOG=info ./target/release/solana-event-trader config.toml
```

Symulacja, PowerShell:

```powershell
$env:RUST_LOG = "info"
.\target\release\solana-event-trader.exe config.toml
```

Jawne wysyłanie transakcji, Linux/WSL:

```bash
RUST_LOG=info ./target/release/solana-event-trader config.toml --live
```

Jawne wysyłanie transakcji, PowerShell:

```powershell
.\target\release\solana-event-trader.exe config.toml --live
```

Nie uruchamiaj wielu instancji dla tego samego portfela. Limity
`max_submissions` i `max_total_input` obowiązują w ramach jednego uruchomienia;
restart procesu rozpoczyna nowe limity. Nie ma limitu strat ani strategii
sprzedaży. `absolute_minimum_out` jest niezależnym dolnym limitem kursu:
ustaw go na podstawie własnej wyceny; przykładowe `1` nie chroni ekonomicznie.

## Filtry i kontrola kosztów

- `getMultipleAccounts` pobiera stan puli i kont z jednego banku; `minContextSlot`
  nie dopuszcza odpowiedzi starszej niż lokalnie odebrany sygnał.
- Bot weryfikuje właścicieli kont, parę mintów, PDA, mint LP i konto opłat.
- Mint wyjściowy musi mieć wyłączone mint authority i freeze authority.
  Mint wejściowy również nie może mieć freeze authority; to restrykcyjny filtr,
  który odrzuci także część popularnych stablecoinów.
- Odrzuca zamrożone konta, obcych delegatów oraz zewnętrzne close authority.
- Sprawdza rezerwy w jednostkach tokenów, saldo portfela i udział wejścia
  w rezerwie. To nie jest wycena płynności w USD ani dowód zablokowania LP.
- Wycena uwzględnia obie opłaty swapu i zaokrąglenia całkowitoliczbowe `u128`.
  `minimum_amount_out` jest egzekwowane przez program podczas wykonania.
- `compute_unit_price_micro_lamports` oznacza cenę **jednego CU**, nie całkowitą
  opłatę. Limit priority fee sprawdzany jest przed startem. Rzeczywista opłata
  sieciowa dla wiadomości jest dodatkowo pobierana przez `getFeeForMessage`.
- `skip_preflight=false` jest ustawieniem domyślnym; można jawnie włączyć `true`.
  Jito Block Engine nie daje analogicznej gwarancji preflight RPC.

Mint bez uprawnień i duże rezerwy nie wykluczają wycofania LP, koncentracji
posiadaczy, manipulacji ceną ani złośliwego/upgradowalnego programu.
Filtry to ograniczone kontrole stanu, nie certyfikat bezpieczeństwa tokena.

## Jito

Odkomentuj sekcję `[jito]`. Podaj endpoint właściwego klastra i regionu,
`tip_lamports` oraz opcjonalną nazwę zmiennej środowiskowej UUID.
Adresy tip są pobierane przez `getTipAccounts`, a tip jest instrukcją
w tej samej transakcji co swap. Wysyłany jest jednoelementowy bundle base64.
Nie ma równoległej wysyłki przez zwykły RPC.

Odpowiedź z bundle ID oznacza przyjęcie żądania przez relay. Bot sprawdza
ostateczny wynik podpisu przez Solana RPC. Priority fee i tip nie zapewniają
pierwszego miejsca w bloku ani włączenia transakcji.

## Błędy, restart i opóźnienia

Logi rozróżniają `READY`, `SIMULATION`, `ACCEPTED` i `FINALIZED`.
Przed wysłaniem bot zapisuje i synchronizuje dziennik `pending_path` zawierający
podpis i podpisaną transakcję. Dziennik jest usuwany dopiero po stwierdzeniu
wyniku `finalized`, także gdy transakcja zakończyła się błędem.

Timeout wysyłania nie oznacza, że transakcja nie trafiła do sieci.
Bot nigdy automatycznie nie podpisuje ponownego zakupu po takim błędzie.
Brak rozstrzygnięcia zatrzymuje proces i pozostawia dziennik. Przy ponownym
uruchomieniu sprawdzany jest tylko zapisany podpis, bez nowych zakupów.
Nie usuwaj dziennika na podstawie pojedynczej odpowiedzi „not found” — sprawdź
historię i wygaśnięcie blockhash w zaufanym RPC. Uszkodzony dziennik również
blokuje działanie. Dziennik zawiera transakcję możliwą do rozgłoszenia do chwili
jej wygaśnięcia; nie udostępniaj go publicznie.

Listener ma reconnect z backoff, resubskrypcję, ping/pong i timeout potwierdzenia
subskrypcji. Po przerwie WSS nie odtwarza historycznych sygnałów zakupowych.
Zdarzenia mogą zostać pominięte. Jedna współdzielona pozycja `watch` zachowuje
najnowszy sygnał zamiast tworzyć kolejkę starych zakupów; deduplikacja ma
ograniczoną pamięć i przetrwa reconnect, ale nie restart procesu.

Parsowanie korzysta z typowanych struktur `serde`, pożyczonych ciągów i
`RawValue`. Logi są skanowane bez tworzenia `Vec<String>`; escape'y JSON mogą
wymagać alokacji. Nie jest to obietnica całkowicie bezalokacyjnego programu:
RPC, TLS, budowa i serializacja transakcji nadal alokują pamięć.

Przed symulacją w trybie domyślnym podpisy są zerowane, a `sigVerify=false`.
RPC symulacji nie otrzymuje transakcji, którą mógłby poprawnie rozgłosić.
Podpisywanie jest osobno sprawdzane lokalnie w testach.

Blockhash jest odświeżany w tle co 400 ms, z maksymalnym lokalnym wiekiem 2 s.
Pobierany jest z `confirmed`, aby zmniejszyć ryzyko odrzuconego blockhash z forka;
nasłuch, odczyty filtrów i preflight korzystają z `processed`.
Na drodze do wysyłki pozostają odczyt stanu, odczyt opłaty i trwały zapis.
O wyborze tych kontroli decyduje ochrona przed niezamierzonym wydaniem środków.
Log `prepare_us` obejmuje lokalny czas od odbioru sygnału do przygotowania
i kontroli opłaty; nie jest pomiarem czasu włączenia do bloku.
`max_event_age_ms` mierzy wiek od lokalnego odbioru, nie od powstania zdarzenia
u dostawcy. `processed` może zostać cofnięte na forku.

W repozytorium są testy matematyki, ABI, filtrów, reconnect, kodowania Jito
i wyłączności dziennika. Nie zastępują testu konkretnego wdrożenia na walidatorze
ani audytu. Kod nie ma deklaracji gotowości produkcyjnej ani pomiarów latencji
mainnet bez takich testów. Żadna transakcja na żywym klastrze nie była wysyłana
podczas tworzenia projektu.

## Źródła API

- [Solana logsSubscribe](https://solana.com/docs/rpc/websocket/logssubscribe)
- [Asynchroniczny RpcClient 2.3.13](https://docs.rs/solana-rpc-client/2.3.13/solana_rpc_client/nonblocking/rpc_client/struct.RpcClient.html)
- [SPL Token Swap 3.0.0 — swap ABI](https://docs.rs/spl-token-swap/3.0.0/spl_token_swap/instruction/fn.swap.html)
- [SPL Token Swap 3.0.0 — stan](https://docs.rs/spl-token-swap/3.0.0/spl_token_swap/state/index.html)
- [Opłaty Solany](https://solana.com/docs/core/fees)
- [Jito Block Engine](https://docs.jito.wtf/lowlatencytxnsend/)
