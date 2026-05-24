use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;
use kaspa_consensus_core::tx::TransactionOutpoint;
use kaspa_rpc_core::{RpcAddress, RpcTransaction, api::rpc::RpcApi};
use kaspa_wrpc_client::{
    KaspaRpcClient, Resolver, WrpcEncoding,
    client::{ConnectOptions, ConnectStrategy},
    prelude::{NetworkId, NetworkType},
};
use serde::Serialize;
use silverscript_lang::doom_tn10 as doom;
use tokio::time::sleep;

#[derive(Debug, Parser)]
#[command(
    name = "doom-tn10-submitter",
    about = "Construct, locally validate, and optionally submit one DoomState TN10 covenant transition",
    next_line_help = true
)]
struct Cli {
    /// Optional explicit TN10 wRPC endpoint, for example ws://127.0.0.1:17210.
    #[arg(long)]
    url: Option<String>,

    /// Connection timeout in milliseconds.
    #[arg(long, default_value_t = 5_000)]
    timeout_ms: u64,

    /// Current on-chain DoomState UTXO transaction id. Omit for local synthetic dry-run.
    #[arg(long = "input-txid")]
    input_txid: Option<String>,

    /// Current on-chain DoomState UTXO output index.
    #[arg(long = "input-index", default_value_t = 0)]
    input_index: u32,

    /// Current DoomState UTXO value in sompi.
    #[arg(long = "utxo-value", default_value_t = 100_000_000)]
    utxo_value: u64,

    /// Fee to pay from each DoomState tic transition in sompi.
    #[arg(long = "fee", default_value_t = 20_000_000)]
    fee: u64,

    /// Current DoomState covenant id. Required for a real on-chain UTXO.
    #[arg(long = "covenant-id")]
    covenant_id: Option<String>,

    /// Wallet address bound to the game session when writing a bridge resume state file.
    #[arg(long = "wallet-address")]
    wallet_address: Option<String>,

    /// Tick currently represented by the input DoomState UTXO.
    #[arg(long = "prev-tick", default_value_t = 0)]
    prev_tick: u32,

    /// Exact 8-byte ticcmd committed in the current DoomState UTXO, encoded as 16 hex chars.
    #[arg(long = "prev-ticcmd-hex")]
    prev_ticcmd_hex: Option<String>,

    /// Exact 32-byte state hash committed in the current DoomState UTXO, encoded as 64 hex chars.
    #[arg(long = "prev-state-hash-hex")]
    prev_state_hash_hex: Option<String>,

    /// Number of consecutive DoomState tic transitions to build and optionally submit.
    #[arg(long, default_value_t = 1)]
    ticks: u32,

    /// Optional exact 8-byte Doom ticcmd for a single successor tic, encoded as 16 lowercase or uppercase hex chars.
    #[arg(long = "next-ticcmd-hex")]
    next_ticcmd_hex: Option<String>,

    /// Optional compact Doom state bytes for a single successor tic, encoded as hex and committed by hash.
    #[arg(long = "next-state-hex")]
    next_state_hex: Option<String>,

    /// Optional pacing target for chained submissions. Set 10 for TN10's desired 10 tics/sec target.
    #[arg(long = "target-tps", default_value_t = 0.0)]
    target_tps: f64,

    /// Submit to TN10 after local validation. Without this flag the command is dry-run only.
    #[arg(long)]
    submit: bool,

    /// Whether RPC should allow orphan transactions.
    #[arg(long = "allow-orphan", default_value_t = false)]
    allow_orphan: bool,

    /// Before live submit, verify that the starting DoomState input UTXO is visible by script address.
    #[arg(long = "preflight-input", default_value_t = true, action = clap::ArgAction::Set)]
    preflight_input: bool,

    /// Keep polling preflight visibility until the starting input appears or the timeout expires.
    #[arg(long = "wait-preflight", default_value_t = false)]
    wait_preflight: bool,

    /// Maximum time to wait for the starting input to appear during preflight. Use 0 to wait indefinitely.
    #[arg(long = "preflight-timeout-ms", default_value_t = 0)]
    preflight_timeout_ms: u64,

    /// Poll interval while waiting for the starting input to appear during preflight.
    #[arg(long = "preflight-poll-ms", default_value_t = 10_000)]
    preflight_poll_ms: u64,

    /// Poll the node after submit until the transaction is visible in the mempool or the timeout expires.
    #[arg(long = "track-mempool", default_value_t = true, action = clap::ArgAction::Set)]
    track_mempool: bool,

    /// Maximum time to wait for mempool visibility after each accepted RPC submit.
    #[arg(long = "mempool-timeout-ms", default_value_t = 2_000)]
    mempool_timeout_ms: u64,

    /// Poll interval for post-submit mempool visibility checks.
    #[arg(long = "mempool-poll-ms", default_value_t = 100)]
    mempool_poll_ms: u64,

    /// Poll the virtual chain after submit until the transaction appears in accepted transaction ids.
    #[arg(long = "track-inclusion", default_value_t = false, action = clap::ArgAction::Set)]
    track_inclusion: bool,

    /// Maximum time to wait for accepted-transaction visibility after each accepted RPC submit.
    #[arg(long = "inclusion-timeout-ms", default_value_t = 10_000)]
    inclusion_timeout_ms: u64,

    /// Poll interval for accepted-transaction visibility checks.
    #[arg(long = "inclusion-poll-ms", default_value_t = 250)]
    inclusion_poll_ms: u64,

    /// Write a doom_tn10_bridge-compatible state file for the final accepted/dry-run successor.
    #[arg(long = "write-bridge-state")]
    write_bridge_state: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct BridgeStateFile {
    input_txid: String,
    input_index: u32,
    utxo_value: u64,
    prev_tick: u32,
    started: bool,
    wallet_address: Option<String>,
    prev_ticcmd_hex: Option<String>,
    prev_state_hash_hex: Option<String>,
    prev_state_hex: Option<String>,
    covenant_id: Option<String>,
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let cli = Cli::parse();
    if cli.ticks == 0 {
        return Err("--ticks must be at least 1".to_string());
    }
    if cli.target_tps.is_sign_negative() {
        return Err("--target-tps cannot be negative".to_string());
    }
    if cli.next_ticcmd_hex.is_some() && cli.ticks != 1 {
        return Err("--next-ticcmd-hex is only supported with --ticks 1 so every Doom tic is explicitly bound".to_string());
    }
    if cli.next_state_hex.is_some() && cli.ticks != 1 {
        return Err("--next-state-hex is only supported with --ticks 1 so every Doom state snapshot is explicitly bound".to_string());
    }
    if cli.submit && cli.input_txid.is_none() {
        return Err("--submit requires --input-txid so placeholder transactions are never broadcast".to_string());
    }
    if cli.submit && cli.covenant_id.is_none() {
        return Err("--submit requires --covenant-id so the successor keeps the live DoomState covenant id".to_string());
    }
    if cli.write_bridge_state.is_some() && cli.wallet_address.as_deref().unwrap_or_default().is_empty() {
        return Err("--write-bridge-state requires --wallet-address so resumed bridge sessions stay wallet-bound".to_string());
    }

    let mut prev_tick = cli.prev_tick;
    let mut utxo_value = cli.utxo_value;
    let mut input_outpoint = match cli.input_txid.as_deref() {
        Some(txid) => TransactionOutpoint::new(doom::parse_txid(txid)?, cli.input_index),
        None => doom::synthetic_outpoint(prev_tick, cli.input_index),
    };
    let covenant_id = match cli.covenant_id.as_deref() {
        Some(covenant_id) => doom::parse_hash(covenant_id, "--covenant-id")?,
        None => doom::COV_DOOM,
    };
    let mut current_state =
        doom::CurrentState::from_cli(cli.prev_tick, cli.prev_ticcmd_hex.as_deref(), cli.prev_state_hash_hex.as_deref())?;
    let next_ticcmd_override = cli.next_ticcmd_hex.as_deref().map(doom::parse_ticcmd_hex).transpose()?;
    let next_state_override = cli.next_state_hex.as_deref().map(|hex| doom::parse_hex(hex, "--next-state-hex")).transpose()?;
    if let Some(state_bytes) = next_state_override.as_deref() {
        let next_tick = cli.prev_tick.checked_add(1).ok_or("--prev-tick plus --ticks is too large")?;
        let next_ticcmd = next_ticcmd_override.clone().unwrap_or_else(|| ticcmd_for_tick(next_tick));
        doom::validate_kds4_state_snapshot(state_bytes, next_tick, &next_ticcmd)?;
    }

    let client = if cli.submit {
        let client = connect_tn10(cli.url.as_deref(), cli.timeout_ms).await?;
        assert_toccata_endpoint(&client).await?;
        Some(client)
    } else {
        None
    };
    let chain_started = Instant::now();
    let pace = (cli.target_tps > 0.0).then(|| Duration::from_secs_f64(1.0 / cli.target_tps));
    let mut accepted = 0u32;
    let mut last_submit_started: Option<Instant> = None;
    let mut last_ticcmd: Option<Vec<u8>> = None;
    let mut last_state_hash: Option<Vec<u8>> = None;
    let mut last_state_chunk: Option<Vec<u8>> = None;

    println!("mode={}", if cli.submit { "tn10-chain-submit" } else { "chain-dry-run" });
    println!("network_id=testnet-10");
    println!("ticks_requested={}", cli.ticks);
    println!("start_prev_tick={}", cli.prev_tick);
    println!("target_tps={:.4}", cli.target_tps);
    println!("start_input_outpoint=({}, {})", input_outpoint.transaction_id, input_outpoint.index);
    println!("covenant_id={covenant_id}");

    if let Some(client) = client.as_ref()
        && cli.preflight_input
    {
        wait_for_starting_input(
            client,
            &current_state,
            input_outpoint,
            cli.wait_preflight,
            Duration::from_millis(cli.preflight_timeout_ms),
            Duration::from_millis(cli.preflight_poll_ms),
        )
        .await?;
    }

    for step in 0..cli.ticks {
        if let (Some(interval), Some(previous_started)) = (pace, last_submit_started) {
            let elapsed = previous_started.elapsed();
            if elapsed < interval {
                sleep(interval - elapsed).await;
            }
        }
        let submit_started = Instant::now();
        last_submit_started = Some(submit_started);

        let built = doom::build_transition(
            &current_state,
            input_outpoint,
            covenant_id,
            utxo_value,
            cli.fee,
            next_ticcmd_override.clone(),
            next_state_override.clone(),
        )?;
        doom::execute_input_with_covenants(built.tx.clone(), built.entries.clone(), 0)
            .map_err(|err| format!("local DoomState covenant validation failed before submit at tick {}: {err}", built.next_tick))?;

        println!("tick_step={step}");
        println!("prev_tick={prev_tick}");
        println!("next_tick={}", built.next_tick);
        println!("input_outpoint=({}, {})", input_outpoint.transaction_id, input_outpoint.index);
        println!("successor_outpoint=({}, 0)", built.tx.id());
        println!("tx_id={}", built.tx.id());
        println!("fee_sompi={}", built.fee);
        println!("successor_utxo_value_sompi={}", built.successor_utxo_value);
        println!("next_ticcmd_hex={}", doom::bytes_to_hex(&built.next_ticcmd));
        println!("next_state_len={}", built.next_state_chunk_len);
        println!("next_state_hash={}", doom::bytes_to_hex(&built.next_state_hash));
        println!("next_state_hex={}", doom::bytes_to_hex(&built.next_state_chunk));
        println!("script_len={}", built.script_len);
        println!("instruction_count={}", built.instruction_count);
        println!("charged_op_count={}", built.charged_op_count);
        println!("sigscript_len={}", built.tx.inputs[0].signature_script.len());
        println!("local_validation=ok");

        if let Some(client) = client.as_ref() {
            let inclusion_start_sink = if cli.track_inclusion {
                match client.get_sink().await {
                    Ok(response) => {
                        println!("inclusion_start_sink={}", response.sink);
                        Some(response.sink)
                    }
                    Err(err) => {
                        println!("inclusion_start_sink=unavailable");
                        println!("inclusion_start_sink_error={err}");
                        None
                    }
                }
            } else {
                None
            };
            let rpc_tx = RpcTransaction::from(&built.tx);
            let rpc_started = Instant::now();
            match client.submit_transaction(rpc_tx, cli.allow_orphan).await {
                Ok(submitted_id) => {
                    let elapsed_ms = rpc_started.elapsed().as_secs_f64() * 1_000.0;
                    println!("rpc_submit=ok");
                    println!("submitted_tx_id={submitted_id}");
                    println!("submit_elapsed_ms={elapsed_ms:.2}");
                    if cli.track_mempool {
                        track_mempool_visibility(
                            client,
                            submitted_id,
                            Duration::from_millis(cli.mempool_timeout_ms),
                            Duration::from_millis(cli.mempool_poll_ms),
                        )
                        .await;
                    } else {
                        println!("mempool_tracking=skipped");
                    }
                    if let Some(start_sink) = inclusion_start_sink {
                        track_inclusion(
                            client,
                            submitted_id,
                            start_sink,
                            Duration::from_millis(cli.inclusion_timeout_ms),
                            Duration::from_millis(cli.inclusion_poll_ms),
                        )
                        .await;
                    } else if cli.track_inclusion {
                        println!("inclusion_tracking=skipped");
                    }
                    accepted += 1;
                }
                Err(err) => {
                    let message = err.to_string();
                    println!("rpc_submit=rejected");
                    println!("rpc_rejection_class={}", doom::classify_rpc_rejection(&message));
                    println!("rpc_rejection_detail={message}");
                    return Err(format!("TN10 submit_transaction rejected {} at tick {}: {message}", built.tx.id(), built.next_tick));
                }
            }
        } else {
            println!("rpc_submit=skipped");
            accepted += 1;
        }

        prev_tick = built.next_tick;
        last_ticcmd = Some(built.next_ticcmd.clone());
        last_state_hash = Some(built.next_state_hash.clone());
        last_state_chunk = Some(built.next_state_chunk.clone());
        current_state = doom::CurrentState::from_parts(built.next_tick, built.next_ticcmd.clone(), built.next_state_hash.clone());
        input_outpoint = TransactionOutpoint::new(built.tx.id(), 0);
        utxo_value = built.successor_utxo_value;
    }

    let elapsed = chain_started.elapsed();
    let elapsed_secs = elapsed.as_secs_f64();
    println!("ticks_attempted={}", cli.ticks);
    println!("ticks_accepted={accepted}");
    println!("chain_elapsed_ms={:.2}", elapsed_secs * 1_000.0);
    if elapsed_secs > 0.0 {
        println!("observed_submit_tps={:.4}", f64::from(accepted) / elapsed_secs);
    }
    println!("final_tick={prev_tick}");
    println!("final_successor_outpoint=({}, {})", input_outpoint.transaction_id, input_outpoint.index);
    if let Some(path) = cli.write_bridge_state.as_ref() {
        let state = BridgeStateFile {
            input_txid: input_outpoint.transaction_id.to_string(),
            input_index: input_outpoint.index,
            utxo_value,
            prev_tick,
            started: true,
            wallet_address: cli.wallet_address.clone(),
            prev_ticcmd_hex: last_ticcmd.as_deref().map(doom::bytes_to_hex),
            prev_state_hash_hex: last_state_hash.as_deref().map(doom::bytes_to_hex),
            prev_state_hex: last_state_chunk.as_deref().map(doom::bytes_to_hex),
            covenant_id: cli.covenant_id.clone(),
        };
        let body = serde_json::to_string_pretty(&state).map_err(|err| format!("failed to encode bridge state file: {err}"))?;
        std::fs::write(path, format!("{body}\n"))
            .map_err(|err| format!("failed to write bridge state file {}: {err}", path.display()))?;
        println!("bridge_state_written=true");
        println!("bridge_state_file={}", path.display());
    }
    if cli.submit {
        println!(
            "next_required_step=track block inclusion for final_successor_outpoint and resume the browser client from the latest accepted DoomState UTXO"
        );
    } else {
        println!(
            "next_required_step=pass a real current DoomState UTXO with --input-txid and --submit after deploying the genesis game UTXO"
        );
    }

    if let Some(client) = client {
        client.disconnect().await.map_err(|err| format!("disconnect failed: {err}"))?;
    }
    Ok(())
}

async fn connect_tn10(url: Option<&str>, timeout_ms: u64) -> Result<KaspaRpcClient, String> {
    let selected_network = Some(NetworkId::with_suffix(NetworkType::Testnet, 10));
    let resolver = url.is_none().then(Resolver::default);
    let client = KaspaRpcClient::new(WrpcEncoding::Borsh, url, resolver, selected_network, None)
        .map_err(|err| format!("failed to create TN10 wRPC client: {err}"))?;
    let options = ConnectOptions {
        block_async_connect: true,
        connect_timeout: Some(Duration::from_millis(timeout_ms)),
        strategy: ConnectStrategy::Fallback,
        ..Default::default()
    };
    client.connect(Some(options)).await.map_err(|err| format!("failed to connect to TN10 wRPC: {err}"))?;
    Ok(client)
}

async fn assert_toccata_endpoint(client: &KaspaRpcClient) -> Result<(), String> {
    let server_info = client.get_server_info().await.map_err(|err| format!("get_server_info failed: {err}"))?;
    let server_version = server_info.server_version.to_ascii_lowercase();
    println!("server_version={}", server_info.server_version);
    if !server_version.contains("toc") {
        return Err(format!(
            "refusing live DoomState submit through non-Toccata endpoint {}; pass an upgraded TN10 Toccata wRPC URL such as ws://10.0.3.26:17210",
            server_info.server_version
        ));
    }
    Ok(())
}

struct PreflightResult {
    rpc_address: RpcAddress,
    utxo_count: usize,
    visible: bool,
}

async fn wait_for_starting_input(
    client: &KaspaRpcClient,
    current_state: &doom::CurrentState,
    input_outpoint: TransactionOutpoint,
    wait: bool,
    timeout: Duration,
    poll: Duration,
) -> Result<(), String> {
    let started = Instant::now();
    let mut attempt = 0u64;
    loop {
        attempt += 1;
        let result = preflight_starting_input(client, current_state, input_outpoint).await?;
        println!("preflight_attempt={attempt}");
        println!("preflight_state_address={}", result.rpc_address);
        println!("preflight_state_utxo_count={}", result.utxo_count);
        println!("preflight_input_visible={}", result.visible);
        if result.visible {
            return Ok(());
        }
        if !wait || (timeout != Duration::ZERO && started.elapsed() >= timeout) {
            return Err(format!(
                "preflight input ({}, {}) is not visible at {}; wait for node sync or deploy a fresh Toccata-visible DoomState genesis",
                input_outpoint.transaction_id, input_outpoint.index, result.rpc_address
            ));
        }
        println!("preflight_wait_elapsed_ms={:.2}", started.elapsed().as_secs_f64() * 1_000.0);
        sleep(poll).await;
    }
}

async fn preflight_starting_input(
    client: &KaspaRpcClient,
    current_state: &doom::CurrentState,
    input_outpoint: TransactionOutpoint,
) -> Result<PreflightResult, String> {
    let address = doom::doom_state_address(current_state)?;
    let rpc_address = RpcAddress::try_from(address.to_string().as_str())
        .map_err(|err| format!("invalid computed DoomState address {address}: {err}"))?;
    let utxos = client
        .get_utxos_by_addresses(vec![rpc_address.clone()])
        .await
        .map_err(|err| format!("preflight get_utxos_by_addresses failed for {rpc_address}: {err}"))?;
    let visible = utxos.iter().any(|entry| {
        entry.outpoint.transaction_id.to_string() == input_outpoint.transaction_id.to_string()
            && entry.outpoint.index == input_outpoint.index
    });
    println!("preflight_input=true");
    Ok(PreflightResult { rpc_address, utxo_count: utxos.len(), visible })
}

async fn track_mempool_visibility(client: &KaspaRpcClient, txid: kaspa_rpc_core::RpcTransactionId, timeout: Duration, poll: Duration) {
    let started = Instant::now();
    loop {
        match client.get_mempool_entry(txid, true, false).await {
            Ok(entry) => {
                println!("mempool_seen=true");
                println!("mempool_is_orphan={}", entry.is_orphan);
                println!("mempool_fee_sompi={}", entry.fee);
                println!("mempool_seen_elapsed_ms={:.2}", started.elapsed().as_secs_f64() * 1_000.0);
                return;
            }
            Err(err) if started.elapsed() >= timeout => {
                println!("mempool_seen=false");
                println!("mempool_wait_elapsed_ms={:.2}", started.elapsed().as_secs_f64() * 1_000.0);
                println!("mempool_last_error={err}");
                return;
            }
            Err(_) => sleep(poll).await,
        }
    }
}

async fn track_inclusion(
    client: &KaspaRpcClient,
    txid: kaspa_rpc_core::RpcTransactionId,
    start_sink: kaspa_rpc_core::RpcHash,
    timeout: Duration,
    poll: Duration,
) {
    let started = Instant::now();
    loop {
        match client.get_virtual_chain_from_block(start_sink, true, None).await {
            Ok(response) => {
                for accepted in &response.accepted_transaction_ids {
                    if accepted.accepted_transaction_ids.iter().any(|accepted_txid| accepted_txid == &txid) {
                        println!("inclusion_seen=true");
                        println!("accepting_block_hash={}", accepted.accepting_block_hash);
                        println!("inclusion_seen_elapsed_ms={:.2}", started.elapsed().as_secs_f64() * 1_000.0);
                        return;
                    }
                }

                if started.elapsed() >= timeout {
                    println!("inclusion_seen=false");
                    println!("inclusion_wait_elapsed_ms={:.2}", started.elapsed().as_secs_f64() * 1_000.0);
                    println!("inclusion_added_blocks={}", response.added_chain_block_hashes.len());
                    println!("inclusion_accepted_blocks={}", response.accepted_transaction_ids.len());
                    return;
                }
            }
            Err(err) if started.elapsed() >= timeout => {
                println!("inclusion_seen=false");
                println!("inclusion_wait_elapsed_ms={:.2}", started.elapsed().as_secs_f64() * 1_000.0);
                println!("inclusion_last_error={err}");
                return;
            }
            Err(_) => {}
        }
        sleep(poll).await;
    }
}

fn ticcmd_for_tick(tick: u32) -> Vec<u8> {
    let mut ticcmd = vec![0u8; 8];
    ticcmd[0] = (tick & 0xff) as u8;
    ticcmd[1] = ((tick >> 8) & 0xff) as u8;
    ticcmd
}
