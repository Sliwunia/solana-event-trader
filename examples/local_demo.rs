//! Isolated localhost fixtures and trigger for the real Solana test validator.
#![allow(deprecated)]

use anyhow::{anyhow, bail, ensure, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::json;
use solana_client::{nonblocking::rpc_client::RpcClient, rpc_config::RpcSimulateTransactionConfig};
use solana_sdk::{
    account::Account,
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    program_option::COption,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    system_program,
    transaction::Transaction,
};
use spl_token::state::{Account as TokenAccount, AccountState, Mint};
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    str::FromStr,
    time::{Duration, Instant},
};
use tokio::time::sleep;

const RPC: &str = "http://127.0.0.1:18899";
const WS: &str = "ws://127.0.0.1:18900";
const PROGRAM: &str = "SwapsVeCiPHMUAtzQWZw7RjsKjgCjhwU55QGu4U1Szw";
const KEY_DIR: &str = "keys/local-demo";
const FIXTURES: &str = ".tools/local-fixtures/accounts";
const CONFIG: &str = "config.local.toml";

fn key(name: &str) -> Result<Keypair> {
    fs::create_dir_all(KEY_DIR)?;
    let path = Path::new(KEY_DIR).join(format!("{name}.json"));
    if path.exists() {
        return read_keypair_file(path)
            .map_err(|_| anyhow!("Nie mozna odczytac lokalnego klucza testowego: {name}"));
    }
    let keypair = Keypair::new();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        fs::set_permissions(KEY_DIR, fs::Permissions::from_mode(0o700))?;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(&serde_json::to_vec(&keypair.to_bytes().as_slice())?)?;
    file.sync_all()?;
    Ok(keypair)
}

struct Demo {
    wallet: Keypair,
    pool: Keypair,
    mint_a: Keypair,
    mint_b: Keypair,
    lp_mint: Keypair,
    vault_a: Keypair,
    vault_b: Keypair,
    source: Keypair,
    destination: Keypair,
    fee: Keypair,
    lp_owner: Keypair,
}

impl Demo {
    fn load() -> Result<Self> {
        Ok(Self {
            wallet: key("wallet")?,
            pool: key("pool")?,
            mint_a: key("mint-a")?,
            mint_b: key("mint-b")?,
            lp_mint: key("lp-mint")?,
            vault_a: key("vault-a")?,
            vault_b: key("vault-b")?,
            source: key("source")?,
            destination: key("destination")?,
            fee: key("fee")?,
            lp_owner: key("lp-owner")?,
        })
    }

    fn authority(&self) -> Result<(Pubkey, u8)> {
        Ok(Pubkey::find_program_address(
            &[self.pool.pubkey().as_ref()],
            &Pubkey::from_str(PROGRAM)?,
        ))
    }

    fn prepare(&self) -> Result<()> {
        fs::create_dir_all(FIXTURES)?;
        let (authority, bump) = self.authority()?;
        let mut pool = vec![0; 324];
        pool[0] = 1;
        pool[1] = 1;
        pool[2] = bump;
        for (offset, address) in [
            (3, spl_token::id()),
            (35, self.vault_a.pubkey()),
            (67, self.vault_b.pubkey()),
            (99, self.lp_mint.pubkey()),
            (131, self.mint_a.pubkey()),
            (163, self.mint_b.pubkey()),
            (195, self.fee.pubkey()),
        ] {
            pool[offset..offset + 32].copy_from_slice(address.as_ref());
        }
        for (i, value) in [25_u64, 10_000, 5, 10_000, 0, 0, 20, 100]
            .iter()
            .enumerate()
        {
            pool[227 + 8 * i..235 + 8 * i].copy_from_slice(&value.to_le_bytes());
        }
        save(
            "pool",
            self.pool.pubkey(),
            Account {
                lamports: 10_000_000,
                data: pool,
                owner: Pubkey::from_str(PROGRAM)?,
                executable: false,
                rent_epoch: 0,
            },
        )?;
        let mint = Mint {
            mint_authority: COption::None,
            supply: 1_100_000_000,
            decimals: 6,
            is_initialized: true,
            freeze_authority: COption::None,
        };
        save("mint-a", self.mint_a.pubkey(), packed(mint)?)?;
        save(
            "mint-b",
            self.mint_b.pubkey(),
            packed(Mint {
                supply: 2_000_000_000,
                ..mint
            })?,
        )?;
        save(
            "lp-mint",
            self.lp_mint.pubkey(),
            packed(Mint {
                mint_authority: COption::Some(authority),
                supply: 1_000_000_000,
                ..mint
            })?,
        )?;
        for (name, address, mint, owner, amount) in [
            (
                "vault-a",
                self.vault_a.pubkey(),
                self.mint_a.pubkey(),
                authority,
                1_000_000_000,
            ),
            (
                "vault-b",
                self.vault_b.pubkey(),
                self.mint_b.pubkey(),
                authority,
                2_000_000_000,
            ),
            (
                "source",
                self.source.pubkey(),
                self.mint_a.pubkey(),
                self.wallet.pubkey(),
                100_000_000,
            ),
            (
                "destination",
                self.destination.pubkey(),
                self.mint_b.pubkey(),
                self.wallet.pubkey(),
                0,
            ),
            (
                "fee",
                self.fee.pubkey(),
                self.lp_mint.pubkey(),
                self.wallet.pubkey(),
                0,
            ),
            (
                "lp-owner",
                self.lp_owner.pubkey(),
                self.lp_mint.pubkey(),
                self.wallet.pubkey(),
                1_000_000_000,
            ),
        ] {
            save(
                name,
                address,
                packed(TokenAccount {
                    mint,
                    owner,
                    amount,
                    delegate: COption::None,
                    state: AccountState::Initialized,
                    is_native: COption::None,
                    delegated_amount: 0,
                    close_authority: COption::None,
                })?,
            )?;
        }
        save(
            "wallet",
            self.wallet.pubkey(),
            Account {
                lamports: 100_000_000_000,
                owner: system_program::id(),
                ..Account::default()
            },
        )?;
        println!("Przygotowano lokalne konto i pule. To fikcyjne salda, tylko do lokalnego testu.");
        Ok(())
    }

    fn instruction(&self) -> Result<Instruction> {
        let mut data = vec![1];
        data.extend_from_slice(&10_000_u64.to_le_bytes());
        data.extend_from_slice(&1_u64.to_le_bytes());
        Ok(Instruction {
            program_id: Pubkey::from_str(PROGRAM)?,
            data,
            accounts: vec![
                AccountMeta::new_readonly(self.pool.pubkey(), false),
                AccountMeta::new_readonly(self.authority()?.0, false),
                AccountMeta::new_readonly(self.wallet.pubkey(), true),
                AccountMeta::new(self.source.pubkey(), false),
                AccountMeta::new(self.vault_a.pubkey(), false),
                AccountMeta::new(self.vault_b.pubkey(), false),
                AccountMeta::new(self.destination.pubkey(), false),
                AccountMeta::new(self.lp_mint.pubkey(), false),
                AccountMeta::new(self.fee.pubkey(), false),
                AccountMeta::new_readonly(spl_token::id(), false),
            ],
        })
    }

    async fn configure(&self, rpc: &RpcClient) -> Result<()> {
        let genesis = rpc.get_genesis_hash().await?.to_string();
        let account = rpc.get_account(&self.pool.pubkey()).await?;
        ensure!(
            account.owner == Pubkey::from_str(PROGRAM)? && account.data.len() == 324,
            "Pod portem testowym nie ma naszej lokalnej puli"
        );
        let simulation =
            Transaction::new_with_payer(&[self.instruction()?], Some(&self.wallet.pubkey()));
        let result = rpc
            .simulate_transaction_with_config(
                &simulation,
                RpcSimulateTransactionConfig {
                    sig_verify: false,
                    replace_recent_blockhash: true,
                    commitment: Some(CommitmentConfig::processed()),
                    ..Default::default()
                },
            )
            .await?;
        ensure!(
            result.value.err.is_none(),
            "Lokalna symulacja swapu nie przeszla: {:?}; logi={:?}",
            result.value.err,
            result.value.logs
        );
        let mut config: toml::Value = toml::from_str(&fs::read_to_string("config.example.toml")?)?;
        for (name, value) in [
            ("rpc_http_url", RPC.to_owned()),
            ("rpc_ws_url", WS.to_owned()),
            ("expected_genesis_hash", genesis),
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
            ("pending_path", "local-demo.pending.json".to_owned()),
        ] {
            config[name] = toml::Value::String(value);
        }
        config["min_input_reserve"] = toml::Value::Integer(500_000_000);
        config["min_output_reserve"] = toml::Value::Integer(500_000_000);
        config["absolute_minimum_out"] = toml::Value::Integer(1_500_000);
        config["max_event_age_ms"] = toml::Value::Integer(10_000);
        config["rpc_timeout_ms"] = toml::Value::Integer(8000);
        fs::write(
            CONFIG,
            format!(
                "# LOKALNY TEST. Salda i tokeny nie maja wartosci.\n{}",
                toml::to_string_pretty(&config)?
            ),
        )?;
        println!("Lokalna symulacja swapu: OK. Utworzono config.local.toml z kompletem adresow.");
        Ok(())
    }

    async fn trigger(&self, rpc: &RpcClient) -> Result<()> {
        let config: toml::Value = toml::from_str(&fs::read_to_string(CONFIG)?)?;
        ensure!(
            config["rpc_http_url"].as_str() == Some(RPC),
            "Trigger dziala tylko na lokalnym RPC"
        );
        ensure!(
            config["expected_genesis_hash"].as_str()
                == Some(&rpc.get_genesis_hash().await?.to_string()),
            "Lokalny genesis nie zgadza sie z konfiguracja"
        );
        let mut tx =
            Transaction::new_with_payer(&[self.instruction()?], Some(&self.wallet.pubkey()));
        tx.try_sign(&[&self.wallet], rpc.get_latest_blockhash().await?)?;
        let signature = rpc.send_and_confirm_transaction(&tx).await?;
        println!("Zdarzenie lokalnej puli wyslane: {signature}");
        Ok(())
    }
}

fn packed<T: Pack>(value: T) -> Result<Account> {
    let mut data = vec![0; T::LEN];
    T::pack(value, &mut data)?;
    Ok(Account {
        lamports: 10_000_000,
        data,
        owner: spl_token::id(),
        executable: false,
        rent_epoch: 0,
    })
}

fn save(name: &str, address: Pubkey, account: Account) -> Result<()> {
    fs::write(
        Path::new(FIXTURES).join(format!("{name}.json")),
        serde_json::to_vec(&json!({"pubkey":address.to_string(), "account":{
            "lamports":account.lamports, "data":[BASE64.encode(account.data),"base64"], "owner":account.owner.to_string(), "executable":account.executable, "rentEpoch":account.rent_epoch
        }}))?,
    )?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let command = env::args().nth(1).unwrap_or_else(|| "help".to_owned());
    if command == "help" {
        println!("local-demo prepare|configure|trigger (tylko lokalna Solana)");
        return Ok(());
    }
    let demo = Demo::load()?;
    if command == "prepare" {
        return demo.prepare();
    }
    let rpc = RpcClient::new_with_timeout_and_commitment(
        RPC.to_owned(),
        Duration::from_secs(5),
        CommitmentConfig::confirmed(),
    );
    match command.as_str() {
        "configure" => {
            let deadline = Instant::now() + Duration::from_secs(90);
            while rpc.get_health().await.is_err() {
                ensure!(
                    Instant::now() < deadline,
                    "Lokalny walidator nie wystartowal. Sprawdz .tools/local-validator.log"
                );
                sleep(Duration::from_secs(1)).await;
            }
            demo.configure(&rpc).await
        }
        "trigger" => demo.trigger(&rpc).await,
        _ => bail!("Nieznana komenda lokalnego testu"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn genesis_accounts_are_valid_legacy_token_states() -> Result<()> {
        let authority = Pubkey::new_unique();
        let mint = Mint {
            mint_authority: COption::None,
            supply: 1000,
            decimals: 6,
            is_initialized: true,
            freeze_authority: COption::None,
        };
        let account = packed(mint)?;
        ensure!(
            Mint::unpack(&account.data)? == mint,
            "Mint serialization mismatch"
        );
        let wallet = packed(TokenAccount {
            mint: Pubkey::new_unique(),
            owner: authority,
            amount: 50,
            delegate: COption::None,
            state: AccountState::Initialized,
            is_native: COption::None,
            delegated_amount: 0,
            close_authority: COption::None,
        })?;
        ensure!(
            TokenAccount::unpack(&wallet.data)?.owner == authority,
            "Token owner mismatch"
        );
        Ok(())
    }
}
