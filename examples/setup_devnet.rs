//! Creates isolated demo accounts on devnet and fills config.toml. Never targets mainnet.
#![allow(deprecated)]

use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::Deserialize;
use serde_json::json;
use solana_client::{
    nonblocking::rpc_client::RpcClient, rpc_config::RpcSimulateTransactionConfig,
    rpc_request::RpcRequest,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};
use spl_token::{
    instruction::{self as token, AuthorityType},
    state::{Account as TokenAccount, Mint},
};
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    str::FromStr,
    time::{Duration, Instant},
};
use tokio::time::sleep;

const RPC: &str = "https://api.devnet.solana.com";
const GENESIS: &str = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG";
const PROGRAM: &str = "SwapsVeCiPHMUAtzQWZw7RjsKjgCjhwU55QGu4U1Szw";
const KEY_DIR: &str = "keys/devnet-demo";
const CONFIG: &str = "config.toml";
const MIN_SETUP_BALANCE: u64 = 50_000_000;

fn key(name: &str) -> Result<Keypair> {
    let dir = Path::new(KEY_DIR);
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let path = dir.join(format!("{name}.json"));
    if path.exists() {
        return read_keypair_file(&path)
            .map_err(|_| anyhow!("Nie mozna odczytac lokalnego klucza testowego: {name}"));
    }
    let generated = Keypair::new();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    file.write_all(&serde_json::to_vec(&generated.to_bytes().as_slice())?)?;
    file.sync_all()?;
    Ok(generated)
}

async fn submit(
    rpc: &RpcClient,
    payer: &Keypair,
    extra: &[&Keypair],
    instructions: &[Instruction],
    label: &str,
) -> Result<()> {
    let hash = rpc.get_latest_blockhash().await?;
    let mut transaction = Transaction::new_with_payer(instructions, Some(&payer.pubkey()));
    let mut signers: Vec<&Keypair> = Vec::with_capacity(extra.len() + 1);
    signers.push(payer);
    signers.extend_from_slice(extra);
    transaction.try_sign(&signers, hash)?;
    let signature = transaction
        .signatures
        .first()
        .context("Brak podpisu transakcji testowej")?;
    println!("Devnet: {label}; podpis={signature}");
    // The SDK retries the same signed transaction, never a newly signed purchase.
    rpc.send_and_confirm_transaction(&transaction).await.with_context(|| format!("Nie udalo sie: {label}; podpis={signature}. Uruchom ponownie setup, aby sprawdzic istniejace konta."))?;
    Ok(())
}

async fn create_mint(
    rpc: &RpcClient,
    payer: &Keypair,
    mint: &Keypair,
    authority: Pubkey,
) -> Result<()> {
    if let Some(account) = rpc
        .get_account_with_commitment(&mint.pubkey(), CommitmentConfig::confirmed())
        .await?
        .value
    {
        ensure!(
            account.owner == spl_token::id(),
            "Mint testowy ma innego wlasciciela"
        );
        Mint::unpack(&account.data)?;
        return Ok(());
    }
    let rent = rpc
        .get_minimum_balance_for_rent_exemption(Mint::LEN)
        .await?;
    submit(
        rpc,
        payer,
        &[mint],
        &[
            system_instruction::create_account(
                &payer.pubkey(),
                &mint.pubkey(),
                rent,
                Mint::LEN as u64,
                &spl_token::id(),
            ),
            token::initialize_mint2(&spl_token::id(), &mint.pubkey(), &authority, None, 6)?,
        ],
        "utworzenie testowego mintu",
    )
    .await
}

async fn create_account(
    rpc: &RpcClient,
    payer: &Keypair,
    account: &Keypair,
    mint: Pubkey,
    owner: Pubkey,
) -> Result<()> {
    if let Some(existing) = rpc
        .get_account_with_commitment(&account.pubkey(), CommitmentConfig::confirmed())
        .await?
        .value
    {
        ensure!(
            existing.owner == spl_token::id(),
            "Konto tokenowe ma innego wlasciciela"
        );
        let token = TokenAccount::unpack(&existing.data)?;
        ensure!(
            token.mint == mint && token.owner == owner,
            "Konto testowe ma nieoczekiwany mint/owner"
        );
        return Ok(());
    }
    let rent = rpc
        .get_minimum_balance_for_rent_exemption(TokenAccount::LEN)
        .await?;
    submit(
        rpc,
        payer,
        &[account],
        &[
            system_instruction::create_account(
                &payer.pubkey(),
                &account.pubkey(),
                rent,
                TokenAccount::LEN as u64,
                &spl_token::id(),
            ),
            token::initialize_account3(&spl_token::id(), &account.pubkey(), &mint, &owner)?,
        ],
        "utworzenie testowego konta tokenowego",
    )
    .await
}

async fn top_up_tokens(
    rpc: &RpcClient,
    payer: &Keypair,
    mint: Pubkey,
    destination: Pubkey,
    target: u64,
) -> Result<()> {
    let current = TokenAccount::unpack(&rpc.get_account(&destination).await?.data)?.amount;
    if current >= target {
        return Ok(());
    }
    submit(
        rpc,
        payer,
        &[],
        &[token::mint_to(
            &spl_token::id(),
            &mint,
            &destination,
            &payer.pubkey(),
            &[],
            target - current,
        )?],
        "zasilenie konta testowymi tokenami",
    )
    .await
}

async fn revoke_mint(rpc: &RpcClient, payer: &Keypair, mint: Pubkey) -> Result<()> {
    let current = Mint::unpack(&rpc.get_account(&mint).await?.data)?;
    if current.mint_authority.is_none() {
        return Ok(());
    }
    submit(
        rpc,
        payer,
        &[],
        &[token::set_authority(
            &spl_token::id(),
            &mint,
            None,
            AuthorityType::MintTokens,
            &payer.pubkey(),
            &[],
        )?],
        "zamkniecie mint authority testowego tokena",
    )
    .await
}

#[derive(Deserialize)]
struct AccountData {
    data: (String, String),
}
#[derive(Deserialize)]
struct ProgramAccount {
    account: AccountData,
}

async fn fee_owner(rpc: &RpcClient, program: Pubkey) -> Result<Pubkey> {
    // Reuse the fee-account owner from an existing pool to honor production build constraints.
    let pools: Vec<ProgramAccount> = rpc
        .send(
            RpcRequest::GetProgramAccounts,
            json!([program.to_string(), {
                "encoding":"base64", "commitment":"confirmed", "filters":[{"dataSize":324}],
                "dataSlice":{"offset":195,"length":32}
            }]),
        )
        .await?;
    for candidate in pools.iter().take(8) {
        let raw = BASE64.decode(&candidate.account.data.0)?;
        let Ok(bytes): std::result::Result<[u8; 32], _> = raw.try_into() else {
            continue;
        };
        let address = Pubkey::new_from_array(bytes);
        if let Some(account) = rpc
            .get_account_with_commitment(&address, CommitmentConfig::confirmed())
            .await?
            .value
        {
            if account.owner == spl_token::id() {
                if let Ok(token) = TokenAccount::unpack(&account.data) {
                    return Ok(token.owner);
                }
            }
        }
    }
    bail!("Nie znaleziono poprawnego konta oplat programu devnet")
}

struct Demo {
    payer: Keypair,
    pool: Keypair,
    mint_a: Keypair,
    mint_b: Keypair,
    lp_mint: Keypair,
    vault_a: Keypair,
    vault_b: Keypair,
    source: Keypair,
    destination: Keypair,
    lp_fee: Keypair,
    lp_owner: Keypair,
}

impl Demo {
    fn load() -> Result<Self> {
        Ok(Self {
            payer: key("wallet")?,
            pool: key("pool")?,
            mint_a: key("mint-a")?,
            mint_b: key("mint-b")?,
            lp_mint: key("lp-mint")?,
            vault_a: key("vault-a")?,
            vault_b: key("vault-b")?,
            source: key("source")?,
            destination: key("destination")?,
            lp_fee: key("lp-fee")?,
            lp_owner: key("lp-owner")?,
        })
    }

    fn initialize(&self, program: Pubkey, authority: Pubkey) -> Instruction {
        // SPL Token Swap v3.0.0 Initialize: tag, eight u64 fee fields, ConstantProduct + 32 zero bytes.
        let mut data = Vec::with_capacity(98);
        data.push(0);
        for value in [25_u64, 10_000, 5, 10_000, 0, 0, 20, 100] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.extend_from_slice(&[0; 33]);
        Instruction {
            program_id: program,
            data,
            accounts: vec![
                AccountMeta::new(self.pool.pubkey(), true),
                AccountMeta::new_readonly(authority, false),
                AccountMeta::new_readonly(self.vault_a.pubkey(), false),
                AccountMeta::new_readonly(self.vault_b.pubkey(), false),
                AccountMeta::new(self.lp_mint.pubkey(), false),
                AccountMeta::new_readonly(self.lp_fee.pubkey(), false),
                AccountMeta::new(self.lp_owner.pubkey(), false),
                AccountMeta::new_readonly(spl_token::id(), false),
            ],
        }
    }

    fn trigger(&self, program: Pubkey, authority: Pubkey) -> Instruction {
        let mut data = vec![1];
        data.extend_from_slice(&10_000_u64.to_le_bytes());
        data.extend_from_slice(&1_u64.to_le_bytes());
        Instruction {
            program_id: program,
            data,
            accounts: vec![
                AccountMeta::new_readonly(self.pool.pubkey(), false),
                AccountMeta::new_readonly(authority, false),
                AccountMeta::new_readonly(self.payer.pubkey(), true),
                AccountMeta::new(self.source.pubkey(), false),
                AccountMeta::new(self.vault_a.pubkey(), false),
                AccountMeta::new(self.vault_b.pubkey(), false),
                AccountMeta::new(self.destination.pubkey(), false),
                AccountMeta::new(self.lp_mint.pubkey(), false),
                AccountMeta::new(self.lp_fee.pubkey(), false),
                AccountMeta::new_readonly(spl_token::id(), false),
            ],
        }
    }

    fn write_config(&self) -> Result<()> {
        let source_path = if Path::new(CONFIG).exists() {
            CONFIG
        } else {
            "config.example.toml"
        };
        let original = fs::read_to_string(source_path)?;
        let mut config: toml::Value = toml::from_str(&original)?;
        let fields = [
            ("rpc_http_url", RPC.to_owned()),
            ("rpc_ws_url", "wss://api.devnet.solana.com".to_owned()),
            ("expected_genesis_hash", GENESIS.to_owned()),
            ("swap_program_id", PROGRAM.to_owned()),
            ("pool_address", self.pool.pubkey().to_string()),
            ("input_mint", self.mint_a.pubkey().to_string()),
            ("output_mint", self.mint_b.pubkey().to_string()),
            ("source_token_account", self.source.pubkey().to_string()),
            (
                "destination_token_account",
                self.destination.pubkey().to_string(),
            ),
            ("keypair_path", format!("{KEY_DIR}/wallet.json")),
        ];
        for (name, value) in fields {
            config[name] = toml::Value::String(value);
        }
        config["rpc_timeout_ms"] = toml::Value::Integer(8000);
        config["max_event_age_ms"] = toml::Value::Integer(10_000);
        config["absolute_minimum_out"] = toml::Value::Integer(1_500_000);
        config["amount_in"] = toml::Value::Integer(1_000_000);
        config["max_total_input"] = toml::Value::Integer(1_000_000);
        config["max_submissions"] = toml::Value::Integer(1);
        config["min_input_reserve"] = toml::Value::Integer(500_000_000);
        config["min_output_reserve"] = toml::Value::Integer(500_000_000);
        config["skip_preflight"] = toml::Value::Boolean(false);
        config
            .as_table_mut()
            .context("Niepoprawna konfiguracja")?
            .remove("jito");
        if !Path::new("config.before-demo.toml").exists() {
            let mut backup = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open("config.before-demo.toml")?;
            backup.write_all(original.as_bytes())?;
            backup.sync_all()?;
        }
        let temp_path = "config.setup-devnet.tmp";
        fs::write(temp_path, toml::to_string_pretty(&config)?)?;
        fs::rename(temp_path, CONFIG)?;
        println!(
            "Uzupelniono config.toml: portfel={}, pula={}",
            self.payer.pubkey(),
            self.pool.pubkey()
        );
        Ok(())
    }
}

async fn verify_swap(
    rpc: &RpcClient,
    demo: &Demo,
    program: Pubkey,
    authority: Pubkey,
) -> Result<()> {
    let transaction = Transaction::new_with_payer(
        &[demo.trigger(program, authority)],
        Some(&demo.payer.pubkey()),
    );
    let simulation = rpc
        .simulate_transaction_with_config(
            &transaction,
            RpcSimulateTransactionConfig {
                sig_verify: false,
                replace_recent_blockhash: true,
                commitment: Some(CommitmentConfig::confirmed()),
                ..Default::default()
            },
        )
        .await?;
    ensure!(
        simulation.value.err.is_none(),
        "Program devnet nie przeszedl symulacji swapu v3: {:?}; logi={:?}",
        simulation.value.err,
        simulation.value.logs
    );
    println!(
        "Symulacja swapu v3 przeszla. Jednostki CU: {:?}",
        simulation.value.units_consumed
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let trigger = env::args().skip(1).any(|arg| arg == "--trigger");
    let rpc = RpcClient::new_with_timeout_and_commitment(
        RPC.into(),
        Duration::from_secs(15),
        CommitmentConfig::confirmed(),
    );
    ensure!(
        rpc.get_genesis_hash().await?.to_string() == GENESIS,
        "Setup dziala wylacznie na zweryfikowanym devnet"
    );
    let program = Pubkey::from_str(PROGRAM)?;
    ensure!(
        rpc.get_account(&program).await?.executable,
        "Brak programu SPL Token Swap na devnet"
    );
    let demo = Demo::load()?;
    let (authority, _) = Pubkey::find_program_address(&[demo.pool.pubkey().as_ref()], &program);
    println!("Portfel TYLKO DO TESTOW devnet: {}", demo.payer.pubkey());
    if trigger {
        submit(
            &rpc,
            &demo.payer,
            &[],
            &[demo.trigger(program, authority)],
            "testowy swap wywolujacy zdarzenie WSS",
        )
        .await?;
        return Ok(());
    }
    let existing_pool = rpc
        .get_account_with_commitment(&demo.pool.pubkey(), CommitmentConfig::confirmed())
        .await?
        .value;
    if let Some(pool) = existing_pool {
        ensure!(
            pool.owner == program
                && pool.data.len() == 324
                && pool.data[0] == 1
                && pool.data[1] == 1,
            "Istniejaca pula testowa ma niepoprawny stan"
        );
        verify_swap(&rpc, &demo, program, authority).await?;
        demo.write_config()?;
        println!("Pula testowa juz istnieje. Uruchom .\\start.ps1");
        return Ok(());
    }
    if rpc.get_balance(&demo.payer.pubkey()).await? < MIN_SETUP_BALANCE {
        println!("Pobieranie 0.1 testowego SOL z devnet...");
        match rpc.request_airdrop(&demo.payer.pubkey(), 100_000_000).await {
            Ok(signature) => println!("Airdrop wyslany: {signature}"),
            Err(_) => bail!("Faucet RPC odmowil testowego SOL. Otworz https://faucet.solana.com/ i wklej PUBLICZNY adres {}. Po zasileniu uruchom ponownie .\\setup-devnet.ps1. Klucz prywatny jest juz zapisany lokalnie.", demo.payer.pubkey()),
        }
        let deadline = Instant::now() + Duration::from_secs(45);
        while rpc.get_balance(&demo.payer.pubkey()).await? < MIN_SETUP_BALANCE {
            ensure!(
                Instant::now() < deadline,
                "Testowe SOL jeszcze nie dotarly. Uruchom setup ponownie za chwile."
            );
            sleep(Duration::from_secs(1)).await;
        }
    }
    let fee_owner = fee_owner(&rpc, program).await?;
    create_mint(&rpc, &demo.payer, &demo.mint_a, demo.payer.pubkey()).await?;
    create_mint(&rpc, &demo.payer, &demo.mint_b, demo.payer.pubkey()).await?;
    create_mint(&rpc, &demo.payer, &demo.lp_mint, authority).await?;
    for (account, mint, owner) in [
        (&demo.vault_a, demo.mint_a.pubkey(), authority),
        (&demo.vault_b, demo.mint_b.pubkey(), authority),
        (&demo.source, demo.mint_a.pubkey(), demo.payer.pubkey()),
        (&demo.destination, demo.mint_b.pubkey(), demo.payer.pubkey()),
        (&demo.lp_fee, demo.lp_mint.pubkey(), fee_owner),
        (&demo.lp_owner, demo.lp_mint.pubkey(), demo.payer.pubkey()),
    ] {
        create_account(&rpc, &demo.payer, account, mint, owner).await?;
    }
    top_up_tokens(
        &rpc,
        &demo.payer,
        demo.mint_a.pubkey(),
        demo.vault_a.pubkey(),
        1_000_000_000,
    )
    .await?;
    top_up_tokens(
        &rpc,
        &demo.payer,
        demo.mint_b.pubkey(),
        demo.vault_b.pubkey(),
        2_000_000_000,
    )
    .await?;
    top_up_tokens(
        &rpc,
        &demo.payer,
        demo.mint_a.pubkey(),
        demo.source.pubkey(),
        100_000_000,
    )
    .await?;
    revoke_mint(&rpc, &demo.payer, demo.mint_a.pubkey()).await?;
    revoke_mint(&rpc, &demo.payer, demo.mint_b.pubkey()).await?;
    let rent = rpc.get_minimum_balance_for_rent_exemption(324).await?;
    submit(
        &rpc,
        &demo.payer,
        &[&demo.pool],
        &[
            system_instruction::create_account(
                &demo.payer.pubkey(),
                &demo.pool.pubkey(),
                rent,
                324,
                &program,
            ),
            demo.initialize(program, authority),
        ],
        "utworzenie puli testowej",
    )
    .await?;
    // Verify the exact swap ABI on-chain before publishing a runnable configuration.
    submit(
        &rpc,
        &demo.payer,
        &[],
        &[demo.trigger(program, authority)],
        "sprawdzenie zgodnosci swapu testowego",
    )
    .await?;
    demo.write_config()?;
    println!("Test gotowy. Uruchom .\\start.ps1; w drugim terminalu .\\setup-devnet.ps1 -Trigger");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn initialize_abi_matches_v3() -> Result<()> {
        let demo = Demo {
            payer: Keypair::new(),
            pool: Keypair::new(),
            mint_a: Keypair::new(),
            mint_b: Keypair::new(),
            lp_mint: Keypair::new(),
            vault_a: Keypair::new(),
            vault_b: Keypair::new(),
            source: Keypair::new(),
            destination: Keypair::new(),
            lp_fee: Keypair::new(),
            lp_owner: Keypair::new(),
        };
        let program = Pubkey::from_str(PROGRAM)?;
        let (authority, _) = Pubkey::find_program_address(&[demo.pool.pubkey().as_ref()], &program);
        let initialize = demo.initialize(program, authority);
        ensure!(
            initialize.data.len() == 98
                && initialize.data[0] == 0
                && initialize.data[65..].iter().all(|x| *x == 0),
            "Incorrect Initialize wire format"
        );
        ensure!(
            initialize.accounts.len() == 8
                && initialize.accounts[0].is_signer
                && initialize.accounts[0].pubkey == demo.pool.pubkey(),
            "Incorrect Initialize account layout"
        );
        ensure!(
            demo.trigger(program, authority).accounts.len() == 10,
            "Incorrect v3 Swap layout"
        );
        Ok(())
    }
}
