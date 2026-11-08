# Co trzeba uzupełnić przed uruchomieniem nasłuchu

## Gotowy lokalny test bez faucetu

Jeśli faucet odrzuca konto GitHub, uruchom `./start-local.ps1` albo
`./start.ps1 -Local`. Tryb ten startuje lokalny walidator Solany w WSL,
przygotowuje fikcyjne salda i pulę, zapisuje kompletny `config.local.toml`
i uruchamia bota w trybie symulacji. Nie trzeba wpisywać żadnych adresów.
`config.toml` dla publicznej sieci devnet pozostaje osobnym plikiem.

Wersja uruchamiana w tle: `./start-local.ps1 -Background`.
Logi: `Get-Content .tools/local-demo.log -Tail 20 -Wait`.
Zatrzymanie: `./stop-local.ps1`. Po kilku sekundach od startu skrypt wysyła
jedno lokalne zdarzenie, na które bot odpowiada symulacją; później czeka na
kolejne zdarzenia.

To rzeczywisty lokalny walidator i wykonanie programu SPL, z kontami
przygotowanymi w genesis. Salda nie są środkami devnet ani mainnet.
Test nie ocenia rentowności i nie handluje prawdziwymi tokenami.
Porty RPC/WSS: `127.0.0.1:18899` i `127.0.0.1:18900`.
Pierwszy start pobiera oficjalny Agave 4.4.1 z kontrolą sumy SHA-256
i kopię programu SPL z publicznego devnet. Kolejne starty używają lokalnej kopii.

## Najprostszy test: automatyczna konfiguracja

W PowerShell uruchom `./setup-devnet.ps1`. Konfigurator tworzy osobny portfel
wyłącznie do testów, dwa tokeny i pulę devnet, sprawdza wykonanie swapu, a dopiero
po sukcesie wpisuje komplet adresów do `config.toml`. Oryginał zachowuje w
`config.before-demo.toml`; istniejących plików kluczy nie nadpisuje.

Jeśli automatyczny faucet odmówi testowego SOL, konfigurator poda publiczny
adres. Zasil ten adres przez [oficjalny faucet devnet](https://faucet.solana.com/)
i uruchom `./setup-devnet.ps1` ponownie. Nie wpłacaj na niego prawdziwych środków.

Po ukończeniu setupu uruchom `./start.ps1`. Aby wywołać zdarzenie podczas testu,
w drugim terminalu uruchom `./setup-devnet.ps1 -Trigger`: wykona mały swap
testowych tokenów na devnet. Bot domyślnie reaguje tylko symulacją.

## Ręczna konfiguracja

Przy wartościach `REPLACE_...` launcher podaje listę brakujących pól i kieruje
do `setup-devnet.ps1`. Takie wartości nie są adresami Solany i nie przejdą
walidacji Base58 w samym programie. Do ukończenia konfiguracji nasłuch
i transakcje nie są uruchomione.

Konfiguracja użytkownika: `config.toml`. Jest ignorowana przez Git.
Ustawiona sieć to **devnet**. Jej `expected_genesis_hash` odczytano z publicznego
RPC devnet. Wszystkie konta i pula muszą należeć do tej samej sieci.

| Pole w `config.toml` | Co wpisać |
| --- | --- |
| `swap_program_id` | Zweryfikowany program zgodny z instrukcją Swap z SPL Token Swap 3.0.0; nie ID Pump.fun ani Raydium. |
| `pool_address` | Konto konkretnej puli tego programu, w formacie SwapV1 ConstantProduct. |
| `input_mint` | Mint tokena, którym płacisz. Dla SOL: mint WSOL i istniejące zasilone konto WSOL. |
| `output_mint` | Mint tokena, który chcesz kupować. |
| `source_token_account` | Twoje istniejące, zasilone konto tokena wejściowego. To adres konta tokenowego, a nie adres portfela. |
| `destination_token_account` | Twoje istniejące konto tokena wyjściowego, należące do tego samego portfela. |
| `keypair_path` | Ścieżka do lokalnego pliku keypair Solana CLI. Domyślnie `keys/trader.json`; tego pliku jeszcze brakuje. Nie wklejaj klucza prywatnego do czatu. |

Przy uruchamianiu przez WSL używaj ścieżki względnej do katalogu projektu,
np. `keys/trader.json`, albo ścieżki Linux `/mnt/c/...`; zapis `C:\...` nie jest
ścieżką Linux. Nie zmieniaj nazwy rozszerzenia dowolnego eksportu portfela na
`.json`: potrzebny jest faktyczny format keypair Solana CLI.

Dopasuj też `amount_in`, `max_total_input`, `absolute_minimum_out` oraz minimalne
rezerwy do liczby miejsc dziesiętnych i wyceny wybranej pary. Kwoty są w surowych
jednostkach tokenów. `absolute_minimum_out = 1` jest przykładem, nie sensowną
ochroną kursu. Portfel musi mieć także SOL na opłaty i wymaganą rezerwę.

Zmiana na mainnet wymaga wymiany obu adresów RPC, oczekiwanego genesis hash
oraz wszystkich adresów puli i tokenów. Sama zmiana URL nie przenosi kont
devnet na mainnet.

Ten adapter celowo odrzuca Token-2022, mint wyjściowy z mint/freeze authority
oraz mint wejściowy z freeze authority. Jeżeli celem jest Pump.fun, potrzebny
jest nowy adapter protokołu; podmiana `swap_program_id` nie wystarczy.

Uruchamianie z PowerShella, z katalogu projektu:

```powershell
.\start.ps1             # symulacja
.\start.ps1 -Build      # przebudowa i symulacja
.\start.ps1 -BuildOnly  # sama kompilacja
```

Plik wykonywalny `dist/linux-x86_64/solana-event-trader` jest przeznaczony dla
Linux/WSL, a `start.ps1` uruchamia go w Ubuntu WSL. Launcher działa też bez Rust
w Windows PATH, korzystając z przygotowanego toolchainu WSL.

Tryb wysyłania transakcji włącza jawny parametr `-Live`. Do czasu uzupełnienia
adresów i portfela program przerwie walidację konfiguracji i nie uruchomi
nasłuchu ani zakupów.
