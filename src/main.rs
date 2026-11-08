//! Event-driven SPL Token Swap 3.0.0 client (legacy SPL Token, constant product).
//! No discovery, no Pump.fun ABI, no implicit live sending.
#![allow(deprecated)] // Compatibility re-exports in the explicitly pinned Solana 2.x SDK.

use std::{
    borrow::Cow,
    collections::{HashSet, VecDeque},
    env, fmt,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use serde::{
    de::{SeqAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use serde_json::{json, value::RawValue};
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcSendTransactionConfig, RpcSimulateTransactionConfig},
};
use solana_sdk::{
    account::Account,
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::Message as SolanaMessage,
    program_option::COption,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signature, Signer},
    system_instruction, system_program,
    transaction::Transaction,
};
use spl_token::state::{Account as TokenAccount, AccountState, Mint};
use tokio::{
    fs,
    io::AsyncWriteExt,
    sync::watch,
    time::{interval, sleep, timeout, MissedTickBehavior},
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
};

const MAX_WS_MESSAGE: usize = 256 * 1024;
const BLOCKHASH_MAX_AGE: Duration = Duration::from_secs(2);
const DEDUP_CAPACITY: usize = 4096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    rpc_http_url: String,
    rpc_ws_url: String,
    keypair_path: PathBuf,
    expected_genesis_hash: String,
    swap_program_id: String,
    pool_address: String,
    input_mint: String,
    output_mint: String,
    source_token_account: String,
    destination_token_account: String,
    amount_in: u64,
    absolute_minimum_out: u64,
    min_input_reserve: u64,
    min_output_reserve: u64,
    slippage_bps: u16,
    max_input_reserve_bps: u16,
    compute_unit_limit: u32,
    compute_unit_price_micro_lamports: u64,
    max_priority_fee_lamports: u64,
    max_network_fee_lamports: u64,
    min_sol_balance_lamports: u64,
    skip_preflight: bool,
    max_submissions: u64,
    max_total_input: u64,
    cooldown_ms: u64,
    max_event_age_ms: u64,
    rpc_timeout_ms: u64,
    confirmation_timeout_secs: u64,
    pending_path: PathBuf,
    jito: Option<JitoConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JitoConfig {
    bundles_url: String,
    tip_lamports: u64,
    auth_env: Option<String>,
}

#[derive(Clone, Copy)]
struct Addresses {
    program: Pubkey,
    pool: Pubkey,
    input_mint: Pubkey,
    output_mint: Pubkey,
    source: Pubkey,
    destination: Pubkey,
}

impl Config {
    fn addresses(&self) -> Result<Addresses> {
        fn key(value: &str, name: &str) -> Result<Pubkey> {
            Pubkey::from_str(value).with_context(|| format!("Invalid public key: {name}"))
        }
        ensure!(
            self.rpc_http_url.starts_with("https://")
                || self.rpc_http_url.starts_with("http://127.0.0.1:"),
            "Use HTTPS RPC or local validator"
        );
        ensure!(
            self.rpc_ws_url.starts_with("wss://") || self.rpc_ws_url.starts_with("ws://127.0.0.1:"),
            "Use WSS or local validator"
        );
        Hash::from_str(&self.expected_genesis_hash).context("Invalid expected_genesis_hash")?;
        ensure!(
            self.amount_in > 0 && self.absolute_minimum_out > 0,
            "Trade amounts must be positive"
        );
        ensure!(
            self.min_input_reserve > 0 && self.min_output_reserve > 0,
            "Reserve thresholds must be positive"
        );
        ensure!(self.slippage_bps < 10_000, "slippage_bps must be < 10000");
        ensure!(
            (1..=10_000).contains(&self.max_input_reserve_bps),
            "Invalid reserve impact limit"
        );
        ensure!(
            (1..=1_400_000).contains(&self.compute_unit_limit),
            "Invalid CU limit"
        );
        let priority = (u128::from(self.compute_unit_limit)
            * u128::from(self.compute_unit_price_micro_lamports))
        .div_ceil(1_000_000);
        ensure!(
            priority <= u128::from(self.max_priority_fee_lamports),
            "Priority fee exceeds cap"
        );
        ensure!(
            self.max_network_fee_lamports >= self.max_priority_fee_lamports,
            "Invalid network fee cap"
        );
        ensure!(
            self.max_submissions > 0 && self.max_total_input >= self.amount_in,
            "Invalid spending limits"
        );
        ensure!(
            self.rpc_timeout_ms > 0
                && self.max_event_age_ms > 0
                && self.confirmation_timeout_secs > 0,
            "Timeouts must be positive"
        );
        ensure!(
            !self.pending_path.as_os_str().is_empty(),
            "Empty pending_path"
        );
        if let Some(jito) = &self.jito {
            ensure!(
                jito.bundles_url.starts_with("https://"),
                "Jito requires HTTPS"
            );
            ensure!(
                jito.tip_lamports >= 1000,
                "Block Engine bundle requires at least 1000 lamports tip"
            );
        }
        let a = Addresses {
            program: key(&self.swap_program_id, "swap_program_id")?,
            pool: key(&self.pool_address, "pool_address")?,
            input_mint: key(&self.input_mint, "input_mint")?,
            output_mint: key(&self.output_mint, "output_mint")?,
            source: key(&self.source_token_account, "source_token_account")?,
            destination: key(&self.destination_token_account, "destination_token_account")?,
        };
        let unique: HashSet<_> = [
            a.program,
            a.pool,
            a.input_mint,
            a.output_mint,
            a.source,
            a.destination,
        ]
        .into_iter()
        .collect();
        ensure!(unique.len() == 6, "Configured accounts must be distinct");
        ensure!(
            a.program != spl_token::id() && a.program != system_program::id(),
            "Invalid swap program"
        );
        Ok(a)
    }
}

/// Wire layout of SwapVersion::SwapV1 in spl-token-swap 3.0.0.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pool {
    bump: u8,
    token_program: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
    lp_mint: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    fee_account: Pubkey,
    fees: [u64; 8],
}

impl Pool {
    fn parse(data: &[u8]) -> Result<Self> {
        ensure!(
            data.len() == 324,
            "Unsupported pool layout; expected 324 bytes"
        );
        ensure!(
            data[0] == 1 && data[1] == 1,
            "Pool version/state is unsupported"
        );
        ensure!(
            data[291] == 0 && data[292..].iter().all(|x| *x == 0),
            "Only ConstantProduct curve is supported"
        );
        fn key(data: &[u8], offset: usize) -> Result<Pubkey> {
            let raw: [u8; 32] = data
                .get(offset..offset + 32)
                .context("Truncated pool key")?
                .try_into()?;
            Ok(Pubkey::new_from_array(raw))
        }
        let mut fees = [0; 8];
        for (i, fee) in fees.iter_mut().enumerate() {
            let offset = 227 + 8 * i;
            *fee = u64::from_le_bytes(data[offset..offset + 8].try_into()?);
        }
        for fee in fees.chunks_exact(2) {
            ensure!(
                fee[0] == 0 || (fee[1] > 0 && fee[0] < fee[1]),
                "Invalid pool fee fraction"
            );
        }
        let pool = Self {
            bump: data[2],
            token_program: key(data, 3)?,
            vault_a: key(data, 35)?,
            vault_b: key(data, 67)?,
            lp_mint: key(data, 99)?,
            mint_a: key(data, 131)?,
            mint_b: key(data, 163)?,
            fee_account: key(data, 195)?,
            fees,
        };
        ensure!(
            pool.token_program == spl_token::id(),
            "Token-2022 and custom token programs are unsupported"
        );
        let unique: HashSet<_> = [
            pool.vault_a,
            pool.vault_b,
            pool.lp_mint,
            pool.mint_a,
            pool.mint_b,
            pool.fee_account,
        ]
        .into_iter()
        .collect();
        ensure!(unique.len() == 6, "Aliased pool accounts");
        Ok(pool)
    }

    fn vaults(&self, a: Addresses) -> Result<(Pubkey, Pubkey)> {
        if self.mint_a == a.input_mint && self.mint_b == a.output_mint {
            Ok((self.vault_a, self.vault_b))
        } else if self.mint_b == a.input_mint && self.mint_a == a.output_mint {
            Ok((self.vault_b, self.vault_a))
        } else {
            bail!("Pool mints do not match configured pair")
        }
    }

    fn authority(&self, a: Addresses) -> Result<Pubkey> {
        Pubkey::create_program_address(&[a.pool.as_ref(), &[self.bump]], &a.program)
            .context("Invalid pool authority PDA")
    }
}

fn token_account(account: &Account) -> Result<TokenAccount> {
    ensure!(
        account.owner == spl_token::id() && !account.executable,
        "Unexpected token account owner"
    );
    let token = TokenAccount::unpack(&account.data).context("Invalid legacy token account")?;
    ensure!(
        token.state == AccountState::Initialized,
        "Token account is frozen or uninitialized"
    );
    Ok(token)
}

fn mint_account(account: &Account) -> Result<Mint> {
    ensure!(
        account.owner == spl_token::id() && !account.executable,
        "Unexpected mint owner; legacy SPL Token required"
    );
    Mint::unpack(&account.data).context("Invalid mint account")
}

fn fee(amount: u64, numerator: u64, denominator: u64) -> Result<u64> {
    if numerator == 0 {
        return Ok(0);
    }
    ensure!(denominator > numerator, "Invalid fee ratio");
    Ok(u64::try_from(
        (u128::from(amount) * u128::from(numerator) / u128::from(denominator)).max(1),
    )?)
}

fn quote(
    amount: u64,
    input: u64,
    output: u64,
    fees: &[u64; 8],
    slippage: u16,
    floor: u64,
) -> Result<(u64, u64)> {
    ensure!(
        amount > 0 && input > 0 && output > 0 && slippage < 10_000,
        "Invalid quote parameters"
    );
    let trade_fee = fee(amount, fees[0], fees[1])?;
    let owner_fee = fee(amount, fees[2], fees[3])?;
    let net = amount
        .checked_sub(trade_fee)
        .and_then(|n| n.checked_sub(owner_fee))
        .context("Pool fees consume input")?;
    ensure!(net > 0, "Zero input after fees");
    // Same output rounding as constant-product checked_ceil_div in SPL Token Swap.
    let expected = u64::try_from(
        u128::from(output) * u128::from(net) / (u128::from(input) + u128::from(net)),
    )?;
    let minimum =
        u64::try_from(u128::from(expected) * u128::from(10_000 - slippage) / 10_000)?.max(floor);
    ensure!(
        minimum > 0 && expected >= minimum,
        "Quote is below absolute_minimum_out"
    );
    Ok((expected, minimum))
}

/// Real exact-in Swap instruction, ten-account ABI from SPL Token Swap 3.0.0.
fn swap_instruction(
    a: Addresses,
    pool: &Pool,
    payer: Pubkey,
    amount: u64,
    minimum: u64,
) -> Result<Instruction> {
    let (input_vault, output_vault) = pool.vaults(a)?;
    let mut data = Vec::with_capacity(17);
    data.push(1);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&minimum.to_le_bytes());
    Ok(Instruction {
        program_id: a.program,
        accounts: vec![
            AccountMeta::new_readonly(a.pool, false),
            AccountMeta::new_readonly(pool.authority(a)?, false),
            AccountMeta::new_readonly(payer, true),
            AccountMeta::new(a.source, false),
            AccountMeta::new(input_vault, false),
            AccountMeta::new(output_vault, false),
            AccountMeta::new(a.destination, false),
            AccountMeta::new(pool.lp_mint, false),
            AccountMeta::new(pool.fee_account, false),
            AccountMeta::new_readonly(spl_token::id(), false),
        ],
        data,
    })
}

async fn checked_quote(
    rpc: &RpcClient,
    c: &Config,
    a: Addresses,
    pool: &Pool,
    payer: Pubkey,
    slot: u64,
) -> Result<(u64, u64, u64)> {
    let (input_vault, output_vault) = pool.vaults(a)?;
    ensure!(
        ![input_vault, output_vault, pool.fee_account].contains(&a.source),
        "Source aliases pool account"
    );
    ensure!(
        ![input_vault, output_vault, pool.fee_account].contains(&a.destination),
        "Destination aliases pool account"
    );
    let keys = [
        a.pool,
        a.input_mint,
        a.output_mint,
        input_vault,
        output_vault,
        a.source,
        a.destination,
        pool.lp_mint,
        pool.fee_account,
        payer,
    ];
    let response = rpc
        .get_multiple_accounts_with_config(
            &keys,
            RpcAccountInfoConfig {
                commitment: Some(CommitmentConfig::processed()),
                min_context_slot: Some(slot),
                ..Default::default()
            },
        )
        .await
        .context("Cannot read coherent pool snapshot")?;
    ensure!(response.context.slot >= slot, "RPC returned an older bank");
    let accounts: Vec<_> = response
        .value
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .context("A required account does not exist")?;
    ensure!(
        accounts.len() == keys.len(),
        "RPC returned wrong account count"
    );
    ensure!(
        accounts[0].owner == a.program && !accounts[0].executable,
        "Wrong pool account owner"
    );
    ensure!(
        &Pool::parse(&accounts[0].data)? == pool,
        "Pool configuration changed; restart and inspect"
    );
    let input_mint = mint_account(&accounts[1])?;
    let output_mint = mint_account(&accounts[2])?;
    ensure!(
        input_mint.freeze_authority == COption::None,
        "Input mint has freeze authority"
    );
    ensure!(
        output_mint.mint_authority == COption::None,
        "Output mint can still be minted"
    );
    ensure!(
        output_mint.freeze_authority == COption::None && output_mint.supply > 0,
        "Unsafe output mint"
    );
    let input = token_account(&accounts[3])?;
    let output = token_account(&accounts[4])?;
    let authority = pool.authority(a)?;
    for (vault, mint) in [(&input, a.input_mint), (&output, a.output_mint)] {
        ensure!(
            vault.mint == mint && vault.owner == authority,
            "Vault mint/authority mismatch"
        );
        ensure!(
            vault.delegate == COption::None && vault.close_authority == COption::None,
            "Vault has external authority"
        );
    }
    ensure!(
        input.amount >= c.min_input_reserve && output.amount >= c.min_output_reserve,
        "Liquidity below threshold"
    );
    ensure!(
        u128::from(c.amount_in) * 10_000
            <= u128::from(input.amount) * u128::from(c.max_input_reserve_bps),
        "Trade exceeds reserve impact limit"
    );
    for (index, mint) in [(5, a.input_mint), (6, a.output_mint)] {
        let user = token_account(&accounts[index])?;
        ensure!(
            user.mint == mint && user.owner == payer,
            "Wallet token account mismatch"
        );
        ensure!(
            user.delegate == COption::None && user.close_authority == COption::None,
            "Wallet token account has external authority"
        );
        if index == 5 {
            ensure!(
                user.amount >= c.amount_in,
                "Insufficient source token balance"
            );
        }
    }
    let lp = mint_account(&accounts[7])?;
    ensure!(
        lp.mint_authority == COption::Some(authority) && lp.freeze_authority == COption::None,
        "Invalid LP mint authority"
    );
    ensure!(
        token_account(&accounts[8])?.mint == pool.lp_mint,
        "Invalid pool fee account"
    );
    let payer_account = &accounts[9];
    ensure!(
        payer_account.owner == system_program::id() && payer_account.data.is_empty(),
        "Payer must be a system account"
    );
    let required_sol = c
        .min_sol_balance_lamports
        .checked_add(c.max_network_fee_lamports)
        .and_then(|x| x.checked_add(c.jito.as_ref().map_or(0, |j| j.tip_lamports)))
        .context("SOL balance limit overflow")?;
    ensure!(
        payer_account.lamports >= required_sol,
        "Insufficient SOL for configured fee caps and reserve"
    );
    let (expected, minimum) = quote(
        c.amount_in,
        input.amount,
        output.amount,
        &pool.fees,
        c.slippage_bps,
        c.absolute_minimum_out,
    )?;
    Ok((expected, minimum, response.context.slot))
}

#[derive(Clone, Copy)]
struct BlockhashSnapshot {
    hash: Hash,
    last_valid_block_height: u64,
    fetched_at: Instant,
}

async fn fetch_blockhash(rpc: &RpcClient) -> Result<BlockhashSnapshot> {
    // confirmed blockhash reduces fork-related BlockhashNotFound; data path stays processed.
    let (hash, last_valid_block_height) = rpc
        .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
        .await?;
    Ok(BlockhashSnapshot {
        hash,
        last_valid_block_height,
        fetched_at: Instant::now(),
    })
}

async fn refresh_blockhash(
    rpc: Arc<RpcClient>,
    sender: watch::Sender<BlockhashSnapshot>,
    mut stop: watch::Receiver<bool>,
) {
    let mut tick = interval(Duration::from_millis(400));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = stop.changed() => return,
            _ = tick.tick() => match fetch_blockhash(&rpc).await {
                Ok(snapshot) => { sender.send_replace(snapshot); }
                Err(_) => warn!("Blockhash refresh failed; stale cache will block sends"),
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Event {
    signature: Signature,
    slot: u64,
    received_at: Instant,
}

#[derive(Deserialize)]
struct Envelope<'a> {
    id: Option<u64>,
    result: Option<u64>,
    #[serde(borrow)]
    error: Option<&'a RawValue>,
    #[serde(borrow)]
    method: Option<&'a str>,
    #[serde(borrow)]
    params: Option<LogParams<'a>>,
}
#[derive(Deserialize)]
struct LogParams<'a> {
    subscription: u64,
    #[serde(borrow)]
    result: LogResult<'a>,
}
#[derive(Deserialize)]
struct LogResult<'a> {
    context: SlotContext,
    #[serde(borrow)]
    value: LogValue<'a>,
}
#[derive(Deserialize)]
struct SlotContext {
    slot: u64,
}
#[derive(Deserialize)]
struct LogValue<'a> {
    #[serde(borrow)]
    signature: Cow<'a, str>,
    #[serde(borrow)]
    err: Option<&'a RawValue>,
    #[serde(borrow)]
    logs: &'a RawValue,
}
#[derive(Deserialize)]
#[serde(transparent)]
struct LogLine<'a>(#[serde(borrow)] Cow<'a, str>);

fn invokes_program(logs: &str, prefix: &str) -> Result<bool> {
    struct Scan<'a>(&'a str);
    impl<'de> Visitor<'de> for Scan<'_> {
        type Value = bool;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("an array of log strings")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<bool, A::Error> {
            let mut found = false;
            while let Some(line) = seq.next_element::<LogLine<'de>>()? {
                found |= line.0.starts_with(self.0);
            }
            Ok(found)
        }
    }
    Ok((&mut serde_json::Deserializer::from_str(logs)).deserialize_seq(Scan(prefix))?)
}

struct Dedup {
    set: HashSet<Signature>,
    order: VecDeque<Signature>,
}
impl Dedup {
    fn new() -> Self {
        Self {
            set: HashSet::with_capacity(DEDUP_CAPACITY),
            order: VecDeque::with_capacity(DEDUP_CAPACITY),
        }
    }
    fn insert(&mut self, signature: Signature) -> bool {
        if self.set.contains(&signature) {
            return false;
        }
        if self.order.len() == DEDUP_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        self.set.insert(signature);
        self.order.push_back(signature);
        true
    }
}

async fn websocket_session(
    c: &Config,
    a: Addresses,
    sender: &watch::Sender<Option<Event>>,
    dedup: &mut Dedup,
    stop: &mut watch::Receiver<bool>,
) -> Result<()> {
    let ws_config = WebSocketConfig::default()
        .read_buffer_size(64 * 1024)
        .write_buffer_size(0)
        .max_message_size(Some(MAX_WS_MESSAGE))
        .max_frame_size(Some(MAX_WS_MESSAGE));
    let (mut socket, _) = timeout(
        Duration::from_secs(10),
        connect_async_with_config(&c.rpc_ws_url, Some(ws_config), true),
    )
    .await
    .context("WebSocket connect timeout")?
    .map_err(|_| anyhow!("WebSocket connect failed"))?;
    let subscribe = json!({"jsonrpc":"2.0","id":1,"method":"logsSubscribe",
        "params":[{"mentions":[a.pool.to_string()]},{"commitment":"processed"}]})
    .to_string();
    timeout(
        Duration::from_secs(5),
        socket.send(Message::Text(subscribe.into())),
    )
    .await??;
    let prefix = format!("Program {} invoke [", a.program);
    let mut subscription = None;
    let connected_at = Instant::now();
    let mut last_receive = Instant::now();
    let mut heartbeat = interval(Duration::from_secs(5));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = stop.changed() => return Ok(()),
            _ = heartbeat.tick() => {
                ensure!(last_receive.elapsed() < Duration::from_secs(20), "WebSocket heartbeat timeout");
                ensure!(subscription.is_some() || connected_at.elapsed() < Duration::from_secs(10), "logsSubscribe acknowledgment timeout");
                timeout(Duration::from_secs(3), socket.send(Message::Ping(Vec::new().into()))).await??;
            }
            frame = socket.next() => {
                let frame = frame.context("WebSocket stream ended")?.map_err(|_| anyhow!("WebSocket read failed"))?;
                last_receive = Instant::now();
                match frame {
                    Message::Text(text) => {
                        let envelope: Envelope<'_> = serde_json::from_str(text.as_str()).context("Invalid WebSocket JSON")?;
                        ensure!(envelope.error.is_none(), "WebSocket JSON-RPC error");
                        if envelope.id == Some(1) {
                            subscription = Some(envelope.result.context("Missing subscription id")?);
                            info!("Subscribed to pool={} commitment=processed", a.pool);
                            continue;
                        }
                        if envelope.method != Some("logsNotification") { continue; }
                        let params = envelope.params.context("Missing log params")?;
                        if Some(params.subscription) != subscription || params.result.value.err.is_some() { continue; }
                        if !invokes_program(params.result.value.logs.get(), &prefix)? { continue; }
                        let signature = Signature::from_str(&params.result.value.signature).context("Invalid event signature")?;
                        if dedup.insert(signature) {
                            // A single coalescing slot: no unbounded queue and no per-event task spawn.
                            sender.send_replace(Some(Event { signature, slot: params.result.context.slot, received_at: Instant::now() }));
                        }
                    }
                    Message::Ping(payload) => { timeout(Duration::from_secs(3), socket.send(Message::Pong(payload))).await??; }
                    Message::Close(_) => bail!("Peer closed WebSocket"),
                    _ => {}
                }
            }
        }
    }
}

async fn listener(
    c: Arc<Config>,
    a: Addresses,
    sender: watch::Sender<Option<Event>>,
    mut stop: watch::Receiver<bool>,
) {
    let mut dedup = Dedup::new();
    let mut backoff_ms = 250_u64;
    loop {
        let started = Instant::now();
        let outcome = websocket_session(&c, a, &sender, &mut dedup, &mut stop).await;
        if *stop.borrow() {
            return;
        }
        if let Err(reason) = outcome {
            warn!("WebSocket disconnected: {reason}; resubscribing");
        }
        if started.elapsed() > Duration::from_secs(30) {
            backoff_ms = 250;
        }
        // Jitter without RNG allocation; not used for any security decision.
        let jitter = u64::from(std::process::id()) % 137;
        tokio::select! {
            _ = stop.changed() => return,
            _ = sleep(Duration::from_millis(backoff_ms + jitter)) => {}
        }
        backoff_ms = (backoff_ms * 2).min(10_000);
    }
}

struct Jito {
    client: reqwest::Client,
    url: String,
    tips: Vec<Pubkey>,
    tip_lamports: u64,
    auth: Option<String>,
}

#[derive(Deserialize)]
struct RpcReply<T> {
    result: Option<T>,
    error: Option<serde_json::Value>,
}

impl Jito {
    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T> {
        let mut request = self
            .client
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}));
        if let Some(auth) = &self.auth {
            request = request.header("x-jito-auth", auth);
        }
        let response = request
            .send()
            .await
            .map_err(|e| anyhow!(e.without_url()))?
            .error_for_status()
            .map_err(|e| anyhow!(e.without_url()))?;
        let body: RpcReply<T> = response
            .json()
            .await
            .map_err(|e| anyhow!(e.without_url()))?;
        ensure!(body.error.is_none(), "Jito JSON-RPC rejected request");
        body.result.context("Missing Jito result")
    }

    async fn connect(c: &JitoConfig, timeout_ms: u64) -> Result<Self> {
        let mut jito = Self {
            client: reqwest::Client::builder()
                .tcp_nodelay(true)
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_millis(timeout_ms))
                .pool_idle_timeout(Duration::from_secs(60))
                .build()?,
            url: c.bundles_url.clone(),
            tips: Vec::new(),
            tip_lamports: c.tip_lamports,
            auth: c
                .auth_env
                .as_ref()
                .map(|name| env::var(name).with_context(|| format!("Missing env variable {name}")))
                .transpose()?,
        };
        let tips: Vec<String> = jito.call("getTipAccounts", json!([])).await?;
        jito.tips = tips
            .iter()
            .map(|key| Pubkey::from_str(key).context("Invalid Jito tip address"))
            .collect::<Result<_>>()?;
        ensure!(!jito.tips.is_empty(), "No Jito tip accounts");
        Ok(jito)
    }

    async fn send(&self, wire: &[u8]) -> Result<String> {
        self.call(
            "sendBundle",
            json!([[BASE64.encode(wire)], {"encoding":"base64"}]),
        )
        .await
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    genesis_hash: String,
    signature: String,
    last_valid_block_height: u64,
    input_amount: u64,
    signed_transaction_base64: String,
}

async fn persist_pending(path: &Path, pending: &Pending) -> Result<()> {
    // create_new doubles as a cross-process guard. No send is allowed before sync_all.
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .await
        .context("Cannot create pending journal; another trade may be unresolved")?;
    file.write_all(&serde_json::to_vec(pending)?).await?;
    file.sync_all().await?;
    // Persist directory entry too, where the platform supports directory fsync.
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::File::open(parent).await?.sync_all().await?;
    }
    Ok(())
}

async fn reconcile(rpc: &RpcClient, c: &Config, signature: Signature) -> Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(c.confirmation_timeout_secs);
    loop {
        // History search also handles restarting after the recent-status cache expired.
        match rpc.get_signature_statuses_with_history(&[signature]).await {
            Ok(response) => {
                if let Some(Some(status)) = response.value.first() {
                    if status.satisfies_commitment(CommitmentConfig::finalized()) {
                        fs::remove_file(&c.pending_path)
                            .await
                            .context("Cannot clear resolved journal")?;
                        if let Some(reason) = &status.err {
                            error!("FINALIZED failure signature={signature}: {reason:?}");
                            return Ok(false);
                        }
                        info!(
                            "FINALIZED success signature={signature} slot={}",
                            status.slot
                        );
                        return Ok(true);
                    }
                }
            }
            Err(_) => warn!("Cannot query signature status; journal retained"),
        }
        ensure!(
            Instant::now() < deadline,
            "Outcome unresolved for {signature}; journal retained; new buys stopped"
        );
        sleep(Duration::from_millis(500)).await;
    }
}

fn sign(
    c: &Config,
    a: Addresses,
    pool: &Pool,
    payer: &Keypair,
    minimum: u64,
    bh: BlockhashSnapshot,
    tip: Option<(Pubkey, u64)>,
) -> Result<(Transaction, Vec<u8>)> {
    ensure!(
        bh.fetched_at.elapsed() <= BLOCKHASH_MAX_AGE,
        "Blockhash cache is stale"
    );
    let mut instructions = Vec::with_capacity(4);
    instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
        c.compute_unit_limit,
    ));
    instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
        c.compute_unit_price_micro_lamports,
    ));
    instructions.push(swap_instruction(
        a,
        pool,
        payer.pubkey(),
        c.amount_in,
        minimum,
    )?);
    if let Some((address, lamports)) = tip {
        instructions.push(system_instruction::transfer(
            &payer.pubkey(),
            &address,
            lamports,
        ));
    }
    let message = SolanaMessage::new(&instructions, Some(&payer.pubkey()));
    let mut transaction = Transaction::new_unsigned(message);
    transaction
        .try_sign(&[payer], bh.hash)
        .context("Signing failed")?;
    let wire = bincode::serialize(&transaction)?;
    ensure!(
        wire.len() <= 1232,
        "Transaction exceeds Solana packet limit"
    );
    Ok((transaction, wire))
}

fn simulation_transaction(mut transaction: Transaction) -> Transaction {
    // A remote simulation service must never receive a broadcastable signature in dry-run mode.
    transaction.signatures.fill(Signature::default());
    transaction
}

struct ExecutionContext {
    c: Arc<Config>,
    a: Addresses,
    pool: Pool,
    payer: Keypair,
    rpc: Arc<RpcClient>,
    live: bool,
    jito: Option<Jito>,
}

async fn executor(
    context: ExecutionContext,
    hashes: watch::Receiver<BlockhashSnapshot>,
    mut events: watch::Receiver<Option<Event>>,
) -> Result<()> {
    let ExecutionContext {
        c,
        a,
        pool,
        payer,
        rpc,
        live,
        jito,
    } = context;
    let mut submissions = 0_u64;
    let mut reserved_input = 0_u64;
    let mut last_attempt: Option<Instant> = None;
    let mut own_signatures = Dedup::new();
    loop {
        events.changed().await.context("Event stream stopped")?;
        let event = match *events.borrow_and_update() {
            Some(e) => e,
            None => continue,
        };
        if own_signatures.set.contains(&event.signature) {
            continue;
        }
        if event.received_at.elapsed() > Duration::from_millis(c.max_event_age_ms) {
            continue;
        }
        if last_attempt.is_some_and(|t| t.elapsed() < Duration::from_millis(c.cooldown_ms)) {
            continue;
        }
        last_attempt = Some(Instant::now());
        let (expected, minimum, snapshot_slot) =
            match checked_quote(&rpc, &c, a, &pool, payer.pubkey(), event.slot).await {
                Ok(quote) => quote,
                Err(reason) => {
                    warn!("Pre-execution filter rejected event: {reason}");
                    continue;
                }
            };
        let bh = *hashes.borrow();
        let tip = jito.as_ref().map(|j| {
            let seed = event.signature.as_ref()[0] as usize + submissions as usize;
            (j.tips[seed % j.tips.len()], j.tip_lamports)
        });
        let (transaction, wire) = match sign(&c, a, &pool, &payer, minimum, bh, tip) {
            Ok(value) => value,
            Err(reason) => {
                warn!("Transaction preparation rejected: {reason}");
                continue;
            }
        };
        let signature = *transaction
            .signatures
            .first()
            .context("Missing signed signature")?;
        // The RPC calculates the actual network fee, including priority fee, for this message.
        let network_fee = match rpc.get_fee_for_message(&transaction.message).await {
            Ok(fee) => fee,
            Err(_) => {
                warn!("Cannot calculate network fee; skipping event");
                continue;
            }
        };
        ensure!(
            network_fee <= c.max_network_fee_lamports,
            "Network fee exceeds configured cap"
        );
        if event.received_at.elapsed() > Duration::from_millis(c.max_event_age_ms)
            || bh.fetched_at.elapsed() > BLOCKHASH_MAX_AGE
        {
            debug!("Event or blockhash became stale during validation");
            continue;
        }
        info!("READY live={live} trigger={} snapshot_slot={snapshot_slot} expected_out={expected} min_out={minimum} network_fee={network_fee} prepare_us={}", event.signature, event.received_at.elapsed().as_micros());
        if !live {
            let simulation = simulation_transaction(transaction);
            match rpc
                .simulate_transaction_with_config(
                    &simulation,
                    RpcSimulateTransactionConfig {
                        sig_verify: false,
                        replace_recent_blockhash: true,
                        commitment: Some(CommitmentConfig::processed()),
                        min_context_slot: Some(snapshot_slot),
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(sim) => info!(
                    "SIMULATION err={:?} units={:?}; nothing submitted",
                    sim.value.err, sim.value.units_consumed
                ),
                Err(_) => warn!("Simulation RPC failed; nothing submitted"),
            }
            continue;
        }
        let next_reserved = reserved_input
            .checked_add(c.amount_in)
            .context("Input budget overflow")?;
        ensure!(
            submissions < c.max_submissions && next_reserved <= c.max_total_input,
            "Submission/input budget reached"
        );
        let pending = Pending {
            genesis_hash: c.expected_genesis_hash.clone(),
            signature: signature.to_string(),
            last_valid_block_height: bh.last_valid_block_height,
            input_amount: c.amount_in,
            signed_transaction_base64: BASE64.encode(&wire),
        };
        persist_pending(&c.pending_path, &pending).await?;
        // After this point any error is treated as potentially submitted. Never re-sign/re-buy.
        submissions += 1;
        reserved_input = next_reserved;
        own_signatures.insert(signature);
        if let Some(jito) = &jito {
            match jito.send(&wire).await {
                Ok(bundle) => info!(
                    "Jito ACCEPTED bundle={bundle} signature={signature}; awaiting chain status"
                ),
                Err(reason) => {
                    warn!("Jito delivery ambiguous ({reason}); checking signature={signature}")
                }
            }
        } else {
            match rpc.send_transaction_with_config(&transaction, RpcSendTransactionConfig {
                skip_preflight: c.skip_preflight,
                preflight_commitment: Some(CommitmentConfig::processed().commitment),
                max_retries: Some(0), min_context_slot: Some(snapshot_slot), ..Default::default()
            }).await {
                Ok(returned) if returned == signature => info!("RPC ACCEPTED signature={signature}; awaiting chain status"),
                Ok(_) => warn!("RPC returned unexpected signature; checking locally signed signature={signature}"),
                Err(_) => warn!("RPC delivery ambiguous; checking signature={signature}"),
            }
        }
        ensure!(
            reconcile(&rpc, &c, signature).await?,
            "Transaction finalized with an error"
        );
        if submissions >= c.max_submissions
            || c.max_total_input.saturating_sub(reserved_input) < c.amount_in
        {
            info!("Run budget reached: submissions={submissions} reserved_input={reserved_input}");
            return Ok(());
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .write_style(env_logger::WriteStyle::Auto)
        .format_timestamp_millis()
        .try_init()?;
    let mut live = false;
    let mut config_path = PathBuf::from("config.toml");
    let mut path_set = false;
    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--live" => live = true,
            "--help" | "-h" => {
                println!("solana-event-trader [config.toml] [--live]\nDefault: unsigned simulation only. Live mode submits real SPL Token Swap trades.");
                return Ok(());
            }
            _ if !arg.starts_with('-') && !path_set => {
                config_path = arg.into();
                path_set = true;
            }
            _ => bail!("Unknown argument; use --help"),
        }
    }
    let c: Config = toml::from_str(
        &fs::read_to_string(config_path)
            .await
            .context("Cannot read config file")?,
    )
    .context("Invalid TOML config")?;
    let a = c.addresses()?;
    let rpc = Arc::new(RpcClient::new_with_timeout_and_commitment(
        c.rpc_http_url.clone(),
        Duration::from_millis(c.rpc_timeout_ms),
        CommitmentConfig::processed(),
    ));
    ensure!(
        rpc.get_genesis_hash().await?.to_string() == c.expected_genesis_hash,
        "RPC cluster genesis mismatch"
    );
    if fs::try_exists(&c.pending_path).await? {
        let pending: Pending = serde_json::from_slice(&fs::read(&c.pending_path).await?)
            .context("Invalid pending journal; inspect before trading")?;
        ensure!(
            pending.genesis_hash == c.expected_genesis_hash,
            "Journal belongs to another cluster"
        );
        let signature = Signature::from_str(&pending.signature)?;
        warn!(
            "Existing journal: reconciling {signature}; this invocation will not start new trades"
        );
        ensure!(
            reconcile(&rpc, &c, signature).await?,
            "Recorded transaction finalized with an error"
        );
        return Ok(());
    }
    let payer =
        read_keypair_file(&c.keypair_path).map_err(|_| anyhow!("Cannot load keypair file"))?;
    let program = rpc
        .get_account(&a.program)
        .await
        .context("Cannot load swap program")?;
    ensure!(
        program.executable,
        "Configured swap program is not executable"
    );
    let pool_account = rpc.get_account(&a.pool).await.context("Cannot load pool")?;
    ensure!(
        pool_account.owner == a.program && !pool_account.executable,
        "Pool is not owned by configured program"
    );
    let pool = Pool::parse(&pool_account.data)?;
    pool.vaults(a)?;
    pool.authority(a)?;
    checked_quote(&rpc, &c, a, &pool, payer.pubkey(), 0)
        .await
        .context("Startup account validation failed")?;
    let jito = match &c.jito {
        Some(config) => Some(Jito::connect(config, c.rpc_timeout_ms).await?),
        None => None,
    };
    let initial_hash = fetch_blockhash(&rpc).await?;
    let (hash_tx, hash_rx) = watch::channel(initial_hash);
    let (events_tx, events_rx) = watch::channel(None);
    let (stop_tx, stop_rx) = watch::channel(false);
    let c = Arc::new(c);
    info!(
        "Starting payer={} live={live} pool={} adapter=SPL-Token-Swap-3.0.0",
        payer.pubkey(),
        a.pool
    );
    let mut background = tokio::task::JoinSet::new();
    background.spawn(refresh_blockhash(rpc.clone(), hash_tx, stop_rx.clone()));
    background.spawn(listener(c.clone(), a, events_tx, stop_rx));
    let result = tokio::select! {
        outcome = executor(ExecutionContext { c, a, pool, payer, rpc, live, jito }, hash_rx, events_rx) => outcome,
        signal = tokio::signal::ctrl_c() => { signal?; info!("Shutdown; any unresolved transaction remains in the journal"); Ok(()) },
        task = background.join_next() => { bail!("Background task stopped unexpectedly: {task:?}") },
    };
    stop_tx.send_replace(true);
    // Cancellation never deletes the pending journal.
    background.shutdown().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_client::rpc_request::RpcRequest;
    use tokio::{io::AsyncReadExt, net::TcpListener};

    #[test]
    fn quote_rounding_fees_and_floor() -> Result<()> {
        let fees = [25, 10_000, 5, 10_000, 0, 0, 0, 0];
        assert_eq!(
            quote(10_000, 1_000_000, 2_000_000, &fees, 100, 1)?,
            (19_743, 19_545)
        );
        assert_eq!(fee(1, 25, 10_000)?, 1);
        assert!(quote(2, 1000, 1000, &fees, 100, 1).is_err());
        assert!(quote(10_000, 1_000_000, 2_000_000, &fees, 100, 20_000).is_err());
        assert!(quote(1, 1, 0, &[0; 8], 100, 1).is_err());
        assert!(quote(u64::MAX, u64::MAX, u64::MAX, &[0; 8], 0, 1).is_ok());
        Ok(())
    }

    fn fixture() -> (Addresses, Vec<u8>) {
        let a = Addresses {
            program: Pubkey::new_unique(),
            pool: Pubkey::new_unique(),
            input_mint: Pubkey::new_unique(),
            output_mint: Pubkey::new_unique(),
            source: Pubkey::new_unique(),
            destination: Pubkey::new_unique(),
        };
        let (_, bump) = Pubkey::find_program_address(&[a.pool.as_ref()], &a.program);
        let mut bytes = vec![0; 324];
        bytes[0] = 1;
        bytes[1] = 1;
        bytes[2] = bump;
        for (offset, key) in [
            (3, spl_token::id()),
            (35, Pubkey::new_unique()),
            (67, Pubkey::new_unique()),
            (99, Pubkey::new_unique()),
            (131, a.input_mint),
            (163, a.output_mint),
            (195, Pubkey::new_unique()),
        ] {
            bytes[offset..offset + 32].copy_from_slice(key.as_ref());
        }
        (a, bytes)
    }

    #[test]
    fn layout_and_swap_abi_both_directions() -> Result<()> {
        let (mut a, bytes) = fixture();
        let pool = Pool::parse(&bytes)?;
        let payer = Pubkey::new_unique();
        let ix = swap_instruction(a, &pool, payer, 42, 17)?;
        assert_eq!(ix.accounts.len(), 10);
        assert_eq!(
            ix.data,
            [
                vec![1],
                42_u64.to_le_bytes().to_vec(),
                17_u64.to_le_bytes().to_vec()
            ]
            .concat()
        );
        assert_eq!(ix.accounts[4].pubkey, pool.vault_a);
        assert_eq!(ix.accounts[5].pubkey, pool.vault_b);
        assert!(ix.accounts[2].is_signer);
        assert!(!ix.accounts[0].is_writable);
        std::mem::swap(&mut a.input_mint, &mut a.output_mint);
        let reverse = swap_instruction(a, &pool, payer, 42, 17)?;
        assert_eq!(reverse.accounts[4].pubkey, pool.vault_b);
        assert_eq!(reverse.accounts[5].pubkey, pool.vault_a);
        Ok(())
    }

    #[test]
    fn rejects_incompatible_pool_and_token2022() {
        let (_, mut bytes) = fixture();
        assert!(Pool::parse(&bytes[..323]).is_err());
        bytes[291] = 2;
        assert!(Pool::parse(&bytes).is_err());
        bytes[291] = 0;
        bytes[3..35].fill(0);
        assert!(Pool::parse(&bytes).is_err());
    }

    #[test]
    fn logs_borrow_strings_and_cannot_spoof_invocation() -> Result<()> {
        let line: LogLine<'_> = serde_json::from_str("\"Program abc invoke [1]\"")?;
        assert!(matches!(line.0, Cow::Borrowed(_)));
        assert!(invokes_program(
            r#"["Program abc invoke [1]","Program log: message"]"#,
            "Program abc invoke ["
        )?);
        assert!(!invokes_program(
            r#"["Program log: Program abc invoke [1]"]"#,
            "Program abc invoke ["
        )?);
        assert!(!invokes_program(
            r#"["Program abcd invoke [1]"]"#,
            "Program abc invoke ["
        )?);
        let envelope: Envelope<'_> = serde_json::from_str(
            r#"{"method":"logsNotification","params":{"subscription":3,"result":{"context":{"slot":99},"value":{"signature":"abc","err":null,"logs":[]}}}}"#,
        )?;
        let params = envelope.params.context("Missing params")?;
        assert!(params.result.value.err.is_none());
        assert!(matches!(params.result.value.signature, Cow::Borrowed(_)));
        Ok(())
    }

    #[test]
    fn dedup_is_bounded_and_retains_recent() {
        let mut dedup = Dedup::new();
        let first = Signature::new_unique();
        assert!(dedup.insert(first));
        assert!(!dedup.insert(first));
        for _ in 0..DEDUP_CAPACITY {
            dedup.insert(Signature::new_unique());
        }
        assert_eq!(dedup.set.len(), DEDUP_CAPACITY);
        assert_eq!(dedup.order.len(), DEDUP_CAPACITY);
        assert!(dedup.insert(first));
    }

    fn test_config(a: Addresses) -> Result<Config> {
        let mut c: Config = toml::from_str(include_str!("../config.example.toml"))?;
        c.swap_program_id = a.program.to_string();
        c.pool_address = a.pool.to_string();
        c.input_mint = a.input_mint.to_string();
        c.output_mint = a.output_mint.to_string();
        c.source_token_account = a.source.to_string();
        c.destination_token_account = a.destination.to_string();
        c.expected_genesis_hash = Hash::new_unique().to_string();
        Ok(c)
    }

    fn packed<T: Pack>(value: T, owner: Pubkey) -> Result<Account> {
        let mut data = vec![0; T::LEN];
        T::pack(value, &mut data)?;
        Ok(Account {
            data,
            owner,
            lamports: 10_000_000,
            executable: false,
            rent_epoch: 0,
        })
    }

    fn mock_snapshot(accounts: &[Account], slot: u64) -> RpcClient {
        let value: Vec<_> = accounts
            .iter()
            .map(|a| {
                json!({
                    "data":[BASE64.encode(&a.data),"base64"], "owner":a.owner.to_string(),
                    "lamports":a.lamports, "executable":a.executable, "rentEpoch":a.rent_epoch,
                })
            })
            .collect();
        RpcClient::new_mock_with_mocks(
            "succeeds".into(),
            [(
                RpcRequest::GetMultipleAccounts,
                json!({"context":{"slot":slot},"value":value}),
            )]
            .into_iter()
            .collect(),
        )
    }

    fn account_fixture(
        a: Addresses,
        pool: &Pool,
        bytes: Vec<u8>,
        payer: Pubkey,
    ) -> Result<Vec<Account>> {
        let authority = pool.authority(a)?;
        let mint = Mint {
            mint_authority: COption::None,
            supply: 10_000_000_000,
            decimals: 6,
            is_initialized: true,
            freeze_authority: COption::None,
        };
        let token = |mint, owner, amount| TokenAccount {
            mint,
            owner,
            amount,
            delegate: COption::None,
            state: AccountState::Initialized,
            is_native: COption::None,
            delegated_amount: 0,
            close_authority: COption::None,
        };
        Ok(vec![
            Account {
                data: bytes,
                owner: a.program,
                lamports: 10_000_000,
                executable: false,
                rent_epoch: 0,
            },
            packed(mint, spl_token::id())?,
            packed(mint, spl_token::id())?,
            packed(
                token(a.input_mint, authority, 2_000_000_000),
                spl_token::id(),
            )?,
            packed(
                token(a.output_mint, authority, 4_000_000_000),
                spl_token::id(),
            )?,
            packed(token(a.input_mint, payer, 100_000_000), spl_token::id())?,
            packed(token(a.output_mint, payer, 0), spl_token::id())?,
            packed(
                Mint {
                    mint_authority: COption::Some(authority),
                    ..mint
                },
                spl_token::id(),
            )?,
            packed(
                token(pool.lp_mint, Pubkey::new_unique(), 0),
                spl_token::id(),
            )?,
            Account {
                lamports: 1_000_000_000,
                owner: system_program::id(),
                ..Account::default()
            },
        ])
    }

    #[tokio::test]
    async fn filters_reject_mint_authority_frozen_vault_and_low_liquidity() -> Result<()> {
        let (a, bytes) = fixture();
        let pool = Pool::parse(&bytes)?;
        let payer = Pubkey::new_unique();
        let c = test_config(a)?;
        c.addresses()?;
        let accounts = account_fixture(a, &pool, bytes, payer)?;
        assert!(
            checked_quote(&mock_snapshot(&accounts, 100), &c, a, &pool, payer, 99)
                .await
                .is_ok()
        );
        assert!(
            checked_quote(&mock_snapshot(&accounts, 98), &c, a, &pool, payer, 99)
                .await
                .is_err()
        );
        let mut unsafe_accounts = accounts.clone();
        let mut mint = Mint::unpack(&unsafe_accounts[2].data)?;
        mint.mint_authority = COption::Some(Pubkey::new_unique());
        Mint::pack(mint, &mut unsafe_accounts[2].data)?;
        assert!(checked_quote(
            &mock_snapshot(&unsafe_accounts, 100),
            &c,
            a,
            &pool,
            payer,
            99
        )
        .await
        .is_err());
        let mut unsafe_accounts = accounts.clone();
        let mut vault = TokenAccount::unpack(&unsafe_accounts[4].data)?;
        vault.state = AccountState::Frozen;
        TokenAccount::pack(vault, &mut unsafe_accounts[4].data)?;
        assert!(checked_quote(
            &mock_snapshot(&unsafe_accounts, 100),
            &c,
            a,
            &pool,
            payer,
            99
        )
        .await
        .is_err());
        let mut unsafe_accounts = accounts.clone();
        vault.state = AccountState::Initialized;
        vault.amount = c.min_output_reserve - 1;
        TokenAccount::pack(vault, &mut unsafe_accounts[4].data)?;
        assert!(checked_quote(
            &mock_snapshot(&unsafe_accounts, 100),
            &c,
            a,
            &pool,
            payer,
            99
        )
        .await
        .is_err());
        let mut unsafe_accounts = accounts;
        unsafe_accounts[3].owner = Pubkey::new_unique();
        assert!(checked_quote(
            &mock_snapshot(&unsafe_accounts, 100),
            &c,
            a,
            &pool,
            payer,
            99
        )
        .await
        .is_err());
        Ok(())
    }

    #[test]
    fn signatures_packet_size_and_stale_blockhash() -> Result<()> {
        let (a, bytes) = fixture();
        let pool = Pool::parse(&bytes)?;
        let c = test_config(a)?;
        let payer = Keypair::new();
        let bh = BlockhashSnapshot {
            hash: Hash::new_unique(),
            last_valid_block_height: 123,
            fetched_at: Instant::now(),
        };
        let (tx, bytes) = sign(
            &c,
            a,
            &pool,
            &payer,
            1234,
            bh,
            Some((Pubkey::new_unique(), 1000)),
        )?;
        tx.verify()?;
        assert!(bytes.len() <= 1232);
        let roundtrip: Transaction = bincode::deserialize(&bytes)?;
        assert_eq!(roundtrip, tx);
        assert_eq!(tx.message.instructions.len(), 4);
        let simulation = simulation_transaction(tx.clone());
        assert_eq!(simulation.message, tx.message);
        assert!(simulation.verify().is_err());
        let stale = BlockhashSnapshot {
            fetched_at: Instant::now() - Duration::from_secs(3),
            ..bh
        };
        assert!(sign(&c, a, &pool, &payer, 1234, stale, None).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn journal_refuses_overwrite() -> Result<()> {
        let path = env::temp_dir().join(format!(
            "trader-test-{}.pending.json",
            Signature::new_unique()
        ));
        let pending = Pending {
            genesis_hash: Hash::new_unique().to_string(),
            signature: Signature::new_unique().to_string(),
            last_valid_block_height: 123,
            input_amount: 10,
            signed_transaction_base64: "AQID".into(),
        };
        persist_pending(&path, &pending).await?;
        let second = persist_pending(&path, &pending).await;
        let read: Pending = serde_json::from_slice(&fs::read(&path).await?)?;
        fs::remove_file(path).await?;
        assert!(second.is_err());
        assert_eq!(read.signature, pending.signature);
        Ok(())
    }

    #[tokio::test]
    async fn jito_encodes_real_bundle_rpc() -> Result<()> {
        let server = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/api/v1/bundles", server.local_addr()?);
        let mock = tokio::spawn(async move {
            let (mut socket, _) = server.accept().await?;
            let mut buffer = Vec::new();
            let mut chunk = [0_u8; 2048];
            let (header_end, length) = loop {
                let n = socket.read(&mut chunk).await?;
                ensure!(n > 0, "Early HTTP EOF");
                buffer.extend_from_slice(&chunk[..n]);
                if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&buffer[..end])?;
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|n| n.trim().to_owned())
                        })
                        .context("Missing content length")?
                        .parse()?;
                    break (end + 4, length);
                }
            };
            while buffer.len() < header_end + length {
                let n = socket.read(&mut chunk).await?;
                ensure!(n > 0, "Early body EOF");
                buffer.extend_from_slice(&chunk[..n]);
            }
            let request: serde_json::Value =
                serde_json::from_slice(&buffer[header_end..header_end + length])?;
            let body = r#"{"jsonrpc":"2.0","id":1,"result":"bundle-test"}"#;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
            Ok::<_, anyhow::Error>(request)
        });
        let jito = Jito {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()?,
            url,
            tips: vec![Pubkey::new_unique()],
            tip_lamports: 1000,
            auth: None,
        };
        assert_eq!(jito.send(&[1, 2, 3]).await?, "bundle-test");
        let request = timeout(Duration::from_secs(3), mock).await???;
        assert_eq!(request["method"], "sendBundle");
        assert_eq!(request["params"], json!([["AQID"], {"encoding":"base64"}]));
        Ok(())
    }

    #[tokio::test]
    async fn websocket_reconnects_and_resubscribes() -> Result<()> {
        let (a, _) = fixture();
        let server = TcpListener::bind("127.0.0.1:0").await?;
        let mut c = test_config(a)?;
        c.rpc_ws_url = format!("ws://{}", server.local_addr()?);
        let sig1 = Signature::new_unique();
        let sig2 = Signature::new_unique();
        let mock = tokio::spawn(async move {
            for signature in [sig1, sig2] {
                let (stream, _) = server.accept().await?;
                let mut ws = tokio_tungstenite::accept_async(stream).await?;
                let frame = ws.next().await.context("No subscribe request")??;
                let request: serde_json::Value = serde_json::from_str(frame.to_text()?)?;
                ensure!(
                    request["method"] == "logsSubscribe",
                    "Wrong subscribe method"
                );
                ensure!(
                    request["params"][0]["mentions"][0] == a.pool.to_string(),
                    "Wrong pool filter"
                );
                ensure!(
                    request["params"][1]["commitment"] == "processed",
                    "Wrong commitment"
                );
                ws.send(Message::Text(
                    r#"{"jsonrpc":"2.0","id":1,"result":42}"#.into(),
                ))
                .await?;
                let notification = json!({"jsonrpc":"2.0","method":"logsNotification","params":{"subscription":42,"result":{
                    "context":{"slot":100},"value":{"signature":signature.to_string(),"err":null,"logs":[format!("Program {} invoke [1]", a.program)]}}}});
                ws.send(Message::Text(notification.to_string().into()))
                    .await?;
                sleep(Duration::from_millis(50)).await;
                ws.close(None).await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        let (event_tx, mut events) = watch::channel(None);
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(listener(Arc::new(c), a, event_tx, stop_rx));
        let mut signatures = Vec::new();
        let receive = async {
            while signatures.len() < 2 {
                events.changed().await?;
                if let Some(event) = *events.borrow_and_update() {
                    signatures.push(event.signature);
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        let result = timeout(Duration::from_secs(5), receive).await;
        stop_tx.send_replace(true);
        timeout(Duration::from_secs(2), task).await??;
        result??;
        timeout(Duration::from_secs(2), mock).await???;
        assert_eq!(signatures, vec![sig1, sig2]);
        Ok(())
    }
}
