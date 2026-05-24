use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use blake2b_simd::Params as Blake2bParams;
use clap::{Parser, ValueEnum};
use kaspa_consensus_core::tx::TransactionOutpoint;
use kaspa_rpc_core::{RpcAddress, RpcTransaction, api::rpc::RpcApi};
use kaspa_wrpc_client::{
    KaspaRpcClient, Resolver, WrpcEncoding,
    client::{ConnectOptions, ConnectStrategy},
    prelude::{NetworkId, NetworkType},
};
use serde::{Deserialize, Serialize};
use silverscript_lang::doom_tn10 as doom;
use tokio::runtime::Runtime;
use tokio::time::sleep;

const DEFAULT_WALLET_ADDRESS: &str = "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz";
const KDS4_STATE_LEN: usize = 96;
const KDS4_TICCMD_OFFSET: usize = 88;

#[derive(Debug, Parser)]
#[command(
    name = "doom-tn10-bridge",
    about = "Local HTTP bridge from browser Doom tic payloads to doom_tn10_submitter",
    next_line_help = true
)]
struct Cli {
    /// Local HTTP listen address used by the Doom browser page.
    #[arg(long, default_value = "127.0.0.1:8787")]
    listen: String,

    /// Optional explicit TN10 wRPC endpoint passed to doom_tn10_submitter.
    #[arg(long)]
    url: Option<String>,

    /// Current on-chain DoomState UTXO transaction id.
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

    /// Current DoomState covenant id.
    #[arg(long = "covenant-id")]
    covenant_id: Option<String>,

    /// Wallet address bound to the current Doom game session when resuming from an explicit tuple.
    #[arg(long = "wallet-address")]
    wallet_address: Option<String>,

    /// Tick currently represented by the input DoomState UTXO.
    #[arg(long = "prev-tick", default_value_t = 0)]
    prev_tick: u32,

    /// Exact ticcmd committed in the current DoomState UTXO, required when resuming after a real browser tic.
    #[arg(long = "prev-ticcmd-hex")]
    prev_ticcmd_hex: Option<String>,

    /// Exact state hash committed in the current DoomState UTXO, required when resuming after a real browser tic.
    #[arg(long = "prev-state-hash-hex")]
    prev_state_hash_hex: Option<String>,

    /// Exact KDS4 state bytes committed in the current DoomState UTXO, used to hydrate browser/report resume state.
    #[arg(long = "prev-state-hex")]
    prev_state_hex: Option<String>,

    /// Whether RPC should allow orphan transactions.
    #[arg(long = "allow-orphan", default_value_t = true)]
    allow_orphan: bool,

    /// Whether the child submitter should verify the current DoomState input before broadcasting.
    #[arg(long = "preflight-input", default_value_t = true, action = clap::ArgAction::Set)]
    preflight_input: bool,

    /// Whether the child submitter should wait for preflight input visibility before broadcasting.
    #[arg(long = "wait-preflight", default_value_t = false)]
    wait_preflight: bool,

    /// Maximum time the child submitter waits for preflight input visibility. Use 0 to wait indefinitely.
    #[arg(long = "preflight-timeout-ms", default_value_t = 0)]
    preflight_timeout_ms: u64,

    /// Poll interval while the child submitter waits for preflight input visibility.
    #[arg(long = "preflight-poll-ms", default_value_t = 10_000)]
    preflight_poll_ms: u64,

    /// Poll mempool visibility in the child submitter.
    #[arg(long = "track-mempool", default_value_t = true, action = clap::ArgAction::Set)]
    track_mempool: bool,

    /// Maximum time to wait for mempool visibility after each accepted RPC submit.
    #[arg(long = "mempool-timeout-ms", default_value_t = 2_000)]
    mempool_timeout_ms: u64,

    /// Poll interval for post-submit mempool visibility checks.
    #[arg(long = "mempool-poll-ms", default_value_t = 100)]
    mempool_poll_ms: u64,

    /// Poll accepted transaction visibility in the child submitter.
    #[arg(long = "track-inclusion", default_value_t = false, action = clap::ArgAction::Set)]
    track_inclusion: bool,

    /// Maximum time to wait for accepted-transaction visibility after each accepted RPC submit.
    #[arg(long = "inclusion-timeout-ms", default_value_t = 10_000)]
    inclusion_timeout_ms: u64,

    /// Poll interval for accepted-transaction visibility checks.
    #[arg(long = "inclusion-poll-ms", default_value_t = 250)]
    inclusion_poll_ms: u64,

    /// Connection timeout in milliseconds for direct in-process live RPC submission.
    #[arg(long = "rpc-timeout-ms", default_value_t = 5_000)]
    rpc_timeout_ms: u64,

    /// Submit child transactions to TN10. Set false for local browser/bridge dry-runs.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    submit: bool,

    /// Backend used for tic transition construction. `child` preserves the legacy submitter subprocess path; `in-process` uses the shared library builder for dry-runs.
    #[arg(long = "submit-backend", value_enum, default_value_t = SubmitBackend::Child)]
    submit_backend: SubmitBackend,

    /// JSON file storing the latest canonical bridge state for restart/resume.
    #[arg(long = "state-file", default_value = ".doom-tn10-bridge-state.json")]
    state_file: PathBuf,

    /// Append-only JSONL event log of accepted bridge tics. Omit to disable.
    #[arg(long = "event-log", default_value = ".doom-tn10-bridge-events.jsonl")]
    event_log: Option<PathBuf>,

    /// Timeout for /ready TN10 status probes.
    #[arg(long = "ready-timeout-ms", default_value_t = 5_000)]
    ready_timeout_ms: u64,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum SubmitBackend {
    Child,
    InProcess,
}

#[derive(Debug, Serialize, Deserialize)]
struct BridgeState {
    input_txid: String,
    input_index: u32,
    #[serde(default)]
    utxo_value: Option<u64>,
    prev_tick: u32,
    #[serde(default)]
    started: bool,
    #[serde(default)]
    wallet_address: Option<String>,
    prev_ticcmd_hex: Option<String>,
    prev_state_hash_hex: Option<String>,
    #[serde(default)]
    prev_state_hex: Option<String>,
    #[serde(default)]
    covenant_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BridgeStateResponse<'a> {
    canonical_tick: u32,
    canonical_outpoint: String,
    input_txid: &'a str,
    input_index: u32,
    utxo_value: Option<u64>,
    started: bool,
    wallet_address: Option<&'a str>,
    ticcmd_hex: Option<&'a str>,
    state_hash: Option<&'a str>,
    state_bytes_hex: Option<&'a str>,
    covenant_id: Option<&'a str>,
    submit: bool,
    bridge_url: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TicPayload {
    tick: u32,
    wallet_address: String,
    canonical_outpoint: Option<String>,
    ticcmd: Vec<u8>,
    state_bytes: Option<Vec<u8>>,
    captured_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartPayload {
    wallet_address: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BridgeResponse {
    canonical_tick: u32,
    successor_outpoint: String,
    tx_id: String,
    fee_sompi: Option<u64>,
    successor_utxo_value_sompi: Option<u64>,
    ticcmd_hex: String,
    state_hash: String,
    state_bytes_hex: String,
    submit_elapsed_ms: f64,
    child_output: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StartResponse {
    canonical_tick: u32,
    initial_game_outpoint: String,
    input_txid: String,
    input_index: u32,
    utxo_value: u64,
    covenant_id: String,
    tx_id: String,
    rpc_submit: String,
    submit: bool,
    synthetic: bool,
    child_output: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadyResponse {
    ready: bool,
    probe_ok: bool,
    submit: bool,
    wallet_address: String,
    endpoint_url: Option<String>,
    server_version: Option<String>,
    is_toccata: Option<bool>,
    is_synced: Option<bool>,
    has_utxo_index: Option<bool>,
    virtual_daa_score: Option<u64>,
    wallet_funded: Option<bool>,
    wallet_balance_sompi: Option<u64>,
    wallet_balance_kas: Option<String>,
    wallet_utxo_count: Option<u64>,
    reference_wallet_funded: Option<bool>,
    reference_wallet_balance_kas: Option<String>,
    reference_wallet_utxo_count: Option<u64>,
    wallet_key_available: bool,
    wallet_key_matches: Option<bool>,
    wallet_key_path: Option<String>,
    doom_expected_genesis_visible: Option<bool>,
    doom_initial_state_utxo_count: Option<u64>,
    ready_to_submit_existing: bool,
    ready_to_deploy_fresh_genesis: bool,
    start_available: bool,
    start_mode: String,
    reason: String,
    next_required_step: String,
    child_output: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BridgeEvent<'a> {
    status: &'a str,
    browser_tick: u32,
    canonical_tick: Option<u32>,
    tx_id: Option<&'a str>,
    successor_outpoint: Option<&'a str>,
    fee_sompi: Option<u64>,
    successor_utxo_value_sompi: Option<u64>,
    ticcmd_hex: &'a str,
    state_hash: Option<&'a str>,
    state_bytes_hex: Option<&'a str>,
    covenant_id: Option<&'a str>,
    current_utxo_value_sompi: Option<u64>,
    submit_elapsed_ms: f64,
    captured_at: Option<&'a str>,
    wallet_address: &'a str,
    canonical_outpoint_before: &'a str,
    error: Option<&'a str>,
    rejection_class: Option<&'a str>,
    rpc_submit: Option<&'a str>,
    mempool_seen: Option<bool>,
    mempool_is_orphan: Option<bool>,
    mempool_seen_elapsed_ms: Option<f64>,
    inclusion_seen: Option<bool>,
    inclusion_seen_elapsed_ms: Option<f64>,
    accepting_block_hash: Option<&'a str>,
    child_output: &'a [&'a str],
}

#[derive(Default)]
struct ChildMetrics {
    rpc_submit: Option<String>,
    rpc_rejection_class: Option<String>,
    mempool_seen: Option<bool>,
    mempool_is_orphan: Option<bool>,
    mempool_seen_elapsed_ms: Option<f64>,
    inclusion_seen: Option<bool>,
    inclusion_seen_elapsed_ms: Option<f64>,
    accepting_block_hash: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorResponse<'a> {
    error: &'a str,
    canonical_tick: u32,
    canonical_outpoint: String,
    utxo_value: Option<u64>,
    wallet_address: Option<&'a str>,
    ticcmd_hex: Option<&'a str>,
    state_hash: Option<&'a str>,
    state_bytes_hex: Option<&'a str>,
    covenant_id: Option<&'a str>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    if cli.prev_ticcmd_hex.is_some() != cli.prev_state_hash_hex.is_some() {
        return Err("--prev-ticcmd-hex and --prev-state-hash-hex must be provided together".to_string());
    }
    if cli.prev_state_hex.is_some() && (cli.prev_ticcmd_hex.is_none() || cli.prev_state_hash_hex.is_none()) {
        return Err("--prev-state-hex requires --prev-ticcmd-hex and --prev-state-hash-hex".to_string());
    }
    let loaded_state = load_bridge_state(&cli)?;
    if cli.submit && cli.input_txid.is_none() && loaded_state.is_none() {
        return Err("--input-txid is required when --submit true unless --state-file already contains a canonical state".to_string());
    }
    if cli.submit && cli.covenant_id.is_none() && loaded_state.as_ref().and_then(|state| state.covenant_id.as_ref()).is_none() {
        return Err("--covenant-id is required when --submit true unless --state-file already contains it".to_string());
    }
    let submitter = submitter_path()?;
    let initial_state = loaded_state.unwrap_or_else(|| BridgeState {
        input_txid: cli.input_txid.clone().unwrap_or_else(|| synthetic_bridge_txid(cli.prev_tick)),
        input_index: cli.input_index,
        utxo_value: Some(cli.utxo_value),
        prev_tick: cli.prev_tick,
        started: cli.submit || cli.input_txid.is_some() || cli.covenant_id.is_some(),
        wallet_address: cli.wallet_address.clone(),
        prev_ticcmd_hex: cli.prev_ticcmd_hex.clone(),
        prev_state_hash_hex: cli.prev_state_hash_hex.clone(),
        prev_state_hex: cli.prev_state_hex.clone(),
        covenant_id: cli.covenant_id.clone(),
    });
    validate_cli_bridge_state(&initial_state)?;
    let state = Arc::new(Mutex::new(initial_state));
    let listener = TcpListener::bind(&cli.listen).map_err(|err| format!("failed to bind {}: {err}", cli.listen))?;

    println!("mode=doom-tn10-bridge");
    println!("listen={}", cli.listen);
    println!("submitter={}", submitter.display());
    println!("submit={}", cli.submit);
    println!("submit_backend={:?}", cli.submit_backend);
    println!("state_file={}", cli.state_file.display());
    if let Some(event_log) = &cli.event_log {
        println!("event_log={}", event_log.display());
    }
    {
        let locked = state.lock().map_err(|_| "bridge state lock poisoned".to_string())?;
        println!("start_outpoint=({}, {})", locked.input_txid, locked.input_index);
        println!("start_prev_tick={}", locked.prev_tick);
    }
    println!("preflight_input={}", cli.preflight_input);
    println!("wait_preflight={}", cli.wait_preflight);

    let cli = Arc::new(cli);
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let cli = Arc::clone(&cli);
                let state = Arc::clone(&state);
                let submitter = submitter.clone();
                if let Err(err) = handle_stream(stream, &cli, &state, &submitter) {
                    eprintln!("bridge request failed: {err}");
                }
            }
            Err(err) => eprintln!("bridge accept failed: {err}"),
        }
    }

    Ok(())
}

fn submitter_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|err| format!("failed to locate current executable: {err}"))?;
    let dir = exe.parent().ok_or_else(|| format!("failed to locate executable directory for {}", exe.display()))?;
    Ok(dir.join("doom_tn10_submitter"))
}

fn genesis_plan_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|err| format!("failed to locate current executable: {err}"))?;
    let dir = exe.parent().ok_or_else(|| format!("failed to locate executable directory for {}", exe.display()))?;
    Ok(dir.join("doom_tn10_genesis_plan"))
}

fn status_probe_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|err| format!("failed to locate current executable: {err}"))?;
    let dir = exe.parent().ok_or_else(|| format!("failed to locate executable directory for {}", exe.display()))?;
    Ok(dir.join("tn10_status_probe"))
}

fn wallet_key_check_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|err| format!("failed to locate current executable: {err}"))?;
    let dir = exe.parent().ok_or_else(|| format!("failed to locate executable directory for {}", exe.display()))?;
    Ok(dir.join("tn10_wallet_key_check"))
}

fn handle_stream(mut stream: TcpStream, cli: &Cli, state: &Arc<Mutex<BridgeState>>, submitter: &PathBuf) -> Result<(), String> {
    let mut request = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let read = stream.read(&mut buf).map_err(|err| format!("failed to read request: {err}"))?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buf[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            let content_len = content_length(&request)?;
            let header_len = header_len(&request).ok_or("failed to find request header terminator")?;
            if request.len() >= header_len + content_len {
                break;
            }
        }
    }

    let request_text = String::from_utf8_lossy(&request);
    let mut lines = request_text.lines();
    let request_line = lines.next().ok_or("empty HTTP request")?;
    let target = request_line.split_whitespace().nth(1).unwrap_or("/");
    if request_line.starts_with("OPTIONS ") {
        return write_response(&mut stream, 204, "No Content", "");
    }
    if request_line.starts_with("GET /ready") {
        let wallet_address = query_param(target, "walletAddress").unwrap_or_else(|| DEFAULT_WALLET_ADDRESS.to_string());
        let body = serde_json::to_string(&probe_ready(cli, state, &wallet_address)?)
            .map_err(|err| format!("failed to encode bridge ready response: {err}"))?;
        return write_response(&mut stream, 200, "OK", &body);
    }
    if request_line.starts_with("GET /state ") {
        let locked = state.lock().map_err(|_| "bridge state lock poisoned".to_string())?;
        let body = serde_json::to_string(&BridgeStateResponse {
            canonical_tick: locked.prev_tick,
            canonical_outpoint: format!("{}:{}", locked.input_txid, locked.input_index),
            input_txid: &locked.input_txid,
            input_index: locked.input_index,
            utxo_value: locked.utxo_value,
            started: locked.started,
            wallet_address: locked.wallet_address.as_deref(),
            ticcmd_hex: locked.prev_ticcmd_hex.as_deref(),
            state_hash: locked.prev_state_hash_hex.as_deref(),
            state_bytes_hex: locked.prev_state_hex.as_deref(),
            covenant_id: locked.covenant_id.as_deref(),
            submit: cli.submit,
            bridge_url: cli.url.as_deref(),
        })
        .map_err(|err| format!("failed to encode bridge state response: {err}"))?;
        return write_response(&mut stream, 200, "OK", &body);
    }
    if request_line.starts_with("POST /start ") {
        let header_len = header_len(&request).ok_or("failed to find request header terminator")?;
        let content_len = content_length(&request)?;
        let payload = if content_len > 0 {
            serde_json::from_slice(&request[header_len..]).map_err(|err| format!("invalid start payload JSON: {err}"))?
        } else {
            StartPayload { wallet_address: None }
        };
        match start_game(cli, state, payload) {
            Ok(response) => {
                let body = serde_json::to_string(&response).map_err(|err| format!("failed to encode start response: {err}"))?;
                write_response(&mut stream, 200, "OK", &body)
            }
            Err(err) => {
                let locked = state.lock().map_err(|_| "bridge state lock poisoned".to_string())?;
                let response = error_response(&err, &locked);
                let body = serde_json::to_string(&response)
                    .map_err(|json_err| format!("failed to encode bridge start error response: {json_err}"))?;
                write_response(&mut stream, 500, "Internal Server Error", &body)
            }
        }?;
        return Ok(());
    }
    if !request_line.starts_with("POST /tic ") {
        return write_response(
            &mut stream,
            404,
            "Not Found",
            "{\"error\":\"expected GET /ready, GET /state, POST /start, or POST /tic\"}",
        );
    }

    let header_len = header_len(&request).ok_or("failed to find request header terminator")?;
    let payload: TicPayload =
        serde_json::from_slice(&request[header_len..]).map_err(|err| format!("invalid tic payload JSON: {err}"))?;
    match submit_tic(cli, state, submitter, payload) {
        Ok(response) => {
            let body = serde_json::to_string(&response).map_err(|err| format!("failed to encode bridge response: {err}"))?;
            write_response(&mut stream, 200, "OK", &body)
        }
        Err(err) => {
            let locked = state.lock().map_err(|_| "bridge state lock poisoned".to_string())?;
            let response = error_response(&err, &locked);
            let body =
                serde_json::to_string(&response).map_err(|json_err| format!("failed to encode bridge error response: {json_err}"))?;
            write_response(&mut stream, 500, "Internal Server Error", &body)
        }
    }
}

fn error_response<'a>(error: &'a str, state: &'a BridgeState) -> ErrorResponse<'a> {
    ErrorResponse {
        error,
        canonical_tick: state.prev_tick,
        canonical_outpoint: format!("{}:{}", state.input_txid, state.input_index),
        utxo_value: state.utxo_value,
        wallet_address: state.wallet_address.as_deref(),
        ticcmd_hex: state.prev_ticcmd_hex.as_deref(),
        state_hash: state.prev_state_hash_hex.as_deref(),
        state_bytes_hex: state.prev_state_hex.as_deref(),
        covenant_id: state.covenant_id.as_deref(),
    }
}

fn bridge_can_resume_existing(cli: &Cli, state: &BridgeState) -> bool {
    state.started
        && state.covenant_id.is_some()
        && state.utxo_value.is_some()
        && (!cli.submit || cli.input_txid.is_some() || state.prev_tick > 0 || state.prev_state_hash_hex.is_some())
}

fn probe_ready(cli: &Cli, state: &Arc<Mutex<BridgeState>>, wallet_address: &str) -> Result<ReadyResponse, String> {
    {
        let locked = state.lock().map_err(|_| "bridge state lock poisoned".to_string())?;
        if bridge_can_resume_existing(cli, &locked) {
            return Ok(ReadyResponse {
                ready: true,
                probe_ok: true,
                submit: cli.submit,
                wallet_address: wallet_address.to_string(),
                endpoint_url: cli.url.clone(),
                server_version: None,
                is_toccata: None,
                is_synced: None,
                has_utxo_index: None,
                virtual_daa_score: None,
                wallet_funded: None,
                wallet_balance_sompi: None,
                wallet_balance_kas: None,
                wallet_utxo_count: None,
                reference_wallet_funded: None,
                reference_wallet_balance_kas: None,
                reference_wallet_utxo_count: None,
                wallet_key_available: std::env::var("KASPA_TN10_MNEMONIC").is_ok(),
                wallet_key_matches: None,
                wallet_key_path: None,
                doom_expected_genesis_visible: None,
                doom_initial_state_utxo_count: None,
                ready_to_submit_existing: true,
                ready_to_deploy_fresh_genesis: false,
                start_available: true,
                start_mode: "resume_existing".to_string(),
                reason: "ready".to_string(),
                next_required_step: "resume the current bridge state and submit the next Doom tic".to_string(),
                child_output: vec![
                    "bridge_resume_state=ready".to_string(),
                    "endpoint_probe=skipped_for_existing_bridge_state".to_string(),
                    format!("resume_tick={}", locked.prev_tick),
                    format!("resume_outpoint={}:{}", locked.input_txid, locked.input_index),
                ],
            });
        }
    }

    let status_probe = status_probe_path()?;
    let mut command = Command::new(&status_probe);
    command.args(["--wallet-address", wallet_address]).args(["--timeout-ms", &cli.ready_timeout_ms.to_string()]);
    command.arg("--compare-reference");
    if let Some(url) = &cli.url {
        command.args(["--url", url]);
    }

    let output = command.output().map_err(|err| format!("failed to run {}: {err}", status_probe.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let child_output = stdout.lines().chain(stderr.lines()).map(str::to_string).collect::<Vec<_>>();
    let server_version = parse_string_line(&stdout, "server_version=");
    let key_check = probe_wallet_key(wallet_address);
    let is_toccata = server_version.as_ref().map(|version| version.to_ascii_lowercase().contains("toc"));
    let is_synced = parse_bool_line(&stdout, "is_synced=");
    let has_utxo_index = parse_bool_line(&stdout, "has_utxo_index=");
    let wallet_funded = parse_bool_line(&stdout, "wallet_funded=");
    let reference_wallet_funded = parse_bool_line(&stdout, "reference_wallet_funded=");
    let reference_wallet_balance_kas = parse_string_line(&stdout, "reference_wallet_balance_kas=");
    let reference_wallet_utxo_count = parse_u64_line(&stdout, "reference_wallet_utxo_count=");
    let doom_expected_genesis_visible = parse_bool_line(&stdout, "doom_expected_genesis_visible=");
    let endpoint_ready =
        output.status.success() && is_toccata == Some(true) && is_synced == Some(true) && has_utxo_index == Some(true);
    let key_matches = key_check.matches == Some(true);
    let bridge_can_resume_existing = {
        let locked = state.lock().map_err(|_| "bridge state lock poisoned".to_string())?;
        bridge_can_resume_existing(cli, &locked)
    };
    let ready_to_submit_existing = endpoint_ready && (doom_expected_genesis_visible == Some(true) || bridge_can_resume_existing);
    let ready_to_deploy_fresh_genesis = endpoint_ready && wallet_funded == Some(true) && key_matches;
    let start_available = !cli.submit || ready_to_submit_existing || ready_to_deploy_fresh_genesis;
    let start_mode = if !cli.submit {
        "synthetic_dry_run"
    } else if ready_to_submit_existing {
        "resume_existing"
    } else if ready_to_deploy_fresh_genesis {
        "fresh_genesis"
    } else {
        "unavailable"
    }
    .to_string();
    let ready = output.status.success()
        && is_toccata == Some(true)
        && is_synced == Some(true)
        && has_utxo_index == Some(true)
        && (ready_to_submit_existing || ready_to_deploy_fresh_genesis);
    let reason = if ready {
        "ready"
    } else if !output.status.success() {
        "status_probe_failed"
    } else if is_toccata != Some(true) {
        "endpoint_missing_toccata"
    } else if is_synced != Some(true) {
        "endpoint_not_synced"
    } else if has_utxo_index != Some(true) {
        "endpoint_missing_utxo_index"
    } else if key_check.available && key_check.matches == Some(false) {
        "wallet_key_mismatch"
    } else if cli.submit && !ready_to_submit_existing && wallet_funded == Some(true) && key_check.matches != Some(true) {
        "wallet_key_unavailable"
    } else if wallet_funded != Some(true) && reference_wallet_funded == Some(true) {
        "wallet_unfunded_on_toccata_reference_funded"
    } else if wallet_funded != Some(true) {
        "wallet_unfunded"
    } else if doom_expected_genesis_visible != Some(true) && !ready_to_deploy_fresh_genesis {
        "genesis_not_visible"
    } else {
        "ready"
    }
    .to_string();
    let next_required_step = ready_next_required_step(
        ready_to_submit_existing,
        ready_to_deploy_fresh_genesis,
        key_check.available,
        key_check.matches,
        wallet_funded,
        reference_wallet_funded,
        doom_expected_genesis_visible,
    );

    Ok(ReadyResponse {
        ready,
        probe_ok: output.status.success(),
        submit: cli.submit,
        wallet_address: wallet_address.to_string(),
        endpoint_url: cli.url.clone().or_else(|| parse_string_line(&stdout, "pnn_resolved_url=")),
        server_version,
        is_toccata,
        is_synced,
        has_utxo_index,
        virtual_daa_score: parse_u64_line(&stdout, "virtual_daa_score="),
        wallet_funded,
        wallet_balance_sompi: parse_u64_line(&stdout, "wallet_balance_sompi="),
        wallet_balance_kas: parse_string_line(&stdout, "wallet_balance_kas="),
        wallet_utxo_count: parse_u64_line(&stdout, "wallet_utxo_count="),
        reference_wallet_funded,
        reference_wallet_balance_kas,
        reference_wallet_utxo_count,
        wallet_key_available: key_check.available,
        wallet_key_matches: key_check.matches,
        wallet_key_path: key_check.path,
        doom_expected_genesis_visible,
        doom_initial_state_utxo_count: parse_u64_line(&stdout, "doom_initial_state_utxo_count="),
        ready_to_submit_existing,
        ready_to_deploy_fresh_genesis,
        start_available,
        start_mode,
        reason,
        next_required_step,
        child_output,
    })
}

fn ready_next_required_step(
    ready_to_submit_existing: bool,
    ready_to_deploy_fresh_genesis: bool,
    wallet_key_available: bool,
    wallet_key_matches: Option<bool>,
    wallet_funded: Option<bool>,
    reference_wallet_funded: Option<bool>,
    doom_expected_genesis_visible: Option<bool>,
) -> String {
    if ready_to_submit_existing {
        "start from the visible DoomState genesis or resume the current bridge state".to_string()
    } else if ready_to_deploy_fresh_genesis {
        "start a fresh DoomState genesis from the funded Toccata wallet".to_string()
    } else if !wallet_key_available {
        "set KASPA_TN10_MNEMONIC in the local bridge environment so the target wallet can authorize genesis".to_string()
    } else if wallet_key_matches == Some(false) {
        "replace the local signing material; it does not derive the target TN10 wallet address".to_string()
    } else if wallet_funded != Some(true) && reference_wallet_funded == Some(true) {
        "wallet funds are visible on the reference TN10 endpoint but not on the selected Toccata endpoint; fund this wallet on the Toccata fork/view or switch the bridge to a synced Toccata node that sees the UTXO".to_string()
    } else if wallet_funded != Some(true) {
        "fund the target wallet on the selected Toccata endpoint, then refresh readiness".to_string()
    } else if doom_expected_genesis_visible != Some(true) {
        "start with fresh genesis once enabled, or wait for the known DoomState genesis to become visible".to_string()
    } else {
        "refresh readiness and retry start".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_next_step_explains_reference_funded_toccata_empty_split() {
        let next_step = ready_next_required_step(false, false, true, Some(true), Some(false), Some(true), Some(false));

        assert!(next_step.contains("reference TN10 endpoint but not on the selected Toccata endpoint"));
    }
}

struct WalletKeyCheck {
    available: bool,
    matches: Option<bool>,
    path: Option<String>,
}

fn probe_wallet_key(wallet_address: &str) -> WalletKeyCheck {
    if std::env::var("KASPA_TN10_MNEMONIC").is_err() {
        return WalletKeyCheck { available: false, matches: None, path: None };
    }
    let Ok(wallet_key_check) = wallet_key_check_path() else {
        return WalletKeyCheck { available: true, matches: Some(false), path: None };
    };
    let Ok(output) = Command::new(wallet_key_check).args(["--wallet-address", wallet_address]).output() else {
        return WalletKeyCheck { available: true, matches: Some(false), path: None };
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    WalletKeyCheck {
        available: true,
        matches: Some(output.status.success() && parse_bool_line(&stdout, "key_matches_wallet=") == Some(true)),
        path: parse_string_line(&stdout, "matched_key_path="),
    }
}

fn start_game(cli: &Cli, state: &Arc<Mutex<BridgeState>>, payload: StartPayload) -> Result<StartResponse, String> {
    let genesis_plan = genesis_plan_path()?;
    let wallet_address = payload.wallet_address.as_deref().filter(|value| !value.is_empty()).unwrap_or(DEFAULT_WALLET_ADDRESS);
    let mut command = Command::new(&genesis_plan);
    command
        .args(["--wallet-address", wallet_address])
        .args(["--game-value", &cli.utxo_value.to_string()])
        .args(["--fee", &cli.fee.to_string()]);
    if let Some(url) = &cli.url {
        command.args(["--url", url]);
    }
    if cli.submit {
        command.arg("--submit");
    }

    let output = command.output().map_err(|err| format!("failed to run {}: {err}", genesis_plan.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let child_output = stdout.lines().chain(stderr.lines()).map(str::to_string).collect::<Vec<_>>();
    if !output.status.success() {
        if !cli.submit {
            return synthetic_start(cli, state, wallet_address, child_output);
        }
        return Err(format!("genesis planner rejected start for wallet {wallet_address}: {}", child_output.join(" | ")));
    }

    let covenant_id =
        parse_string_line(&stdout, "doom_covenant_id=").ok_or("genesis planner output did not include doom_covenant_id")?;
    let initial_game_outpoint = parse_outpoint_line(&stdout, "initial_game_outpoint=")
        .ok_or("genesis planner output did not include initial_game_outpoint")?;
    let tx_id =
        parse_string_line(&stdout, "signed_deploy_tx_id=").ok_or("genesis planner output did not include signed_deploy_tx_id")?;
    let rpc_submit = parse_string_line(&stdout, "rpc_submit=").unwrap_or_else(|| "unknown".to_string());
    let (input_txid, input_index) = split_outpoint(&initial_game_outpoint)?;

    {
        let mut locked = state.lock().map_err(|_| "bridge state lock poisoned".to_string())?;
        locked.input_txid = input_txid.clone();
        locked.input_index = input_index;
        locked.utxo_value = Some(cli.utxo_value);
        locked.prev_tick = 0;
        locked.started = true;
        locked.prev_ticcmd_hex = None;
        locked.prev_state_hash_hex = None;
        locked.prev_state_hex = None;
        locked.covenant_id = Some(covenant_id.clone());
        locked.wallet_address = Some(wallet_address.to_string());
        persist_bridge_state(&cli.state_file, &locked)?;
    }

    println!(
        "game_start wallet={} canonical_tick=0 outpoint={} covenant_id={} tx_id={} rpc_submit={}",
        wallet_address, initial_game_outpoint, covenant_id, tx_id, rpc_submit
    );

    Ok(StartResponse {
        canonical_tick: 0,
        initial_game_outpoint,
        input_txid,
        input_index,
        utxo_value: cli.utxo_value,
        covenant_id,
        tx_id,
        rpc_submit,
        submit: cli.submit,
        synthetic: false,
        child_output,
    })
}

fn synthetic_start(
    cli: &Cli,
    state: &Arc<Mutex<BridgeState>>,
    wallet_address: &str,
    child_output: Vec<String>,
) -> Result<StartResponse, String> {
    let input_txid = synthetic_bridge_txid(0);
    let input_index = 0;
    let initial_game_outpoint = format!("{input_txid}:{input_index}");
    let covenant_id =
        cli.covenant_id.clone().unwrap_or_else(|| "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_string());

    {
        let mut locked = state.lock().map_err(|_| "bridge state lock poisoned".to_string())?;
        locked.input_txid = input_txid.clone();
        locked.input_index = input_index;
        locked.utxo_value = Some(cli.utxo_value);
        locked.prev_tick = 0;
        locked.started = true;
        locked.prev_ticcmd_hex = None;
        locked.prev_state_hash_hex = None;
        locked.prev_state_hex = None;
        locked.covenant_id = Some(covenant_id.clone());
        locked.wallet_address = Some(wallet_address.to_string());
        persist_bridge_state(&cli.state_file, &locked)?;
    }

    println!(
        "game_start wallet={} canonical_tick=0 outpoint={} covenant_id={} tx_id={} rpc_submit=synthetic_dry_run synthetic=true",
        wallet_address, initial_game_outpoint, covenant_id, input_txid
    );

    Ok(StartResponse {
        canonical_tick: 0,
        initial_game_outpoint,
        input_txid: input_txid.clone(),
        input_index,
        utxo_value: cli.utxo_value,
        covenant_id,
        tx_id: input_txid,
        rpc_submit: "synthetic_dry_run".to_string(),
        submit: false,
        synthetic: true,
        child_output,
    })
}

fn submit_tic(cli: &Cli, state: &Arc<Mutex<BridgeState>>, submitter: &PathBuf, payload: TicPayload) -> Result<BridgeResponse, String> {
    let mut locked = state.lock().map_err(|_| "bridge state lock poisoned".to_string())?;
    let started = Instant::now();
    let prev_tick = locked.prev_tick;
    let input_txid = locked.input_txid.clone();
    let input_index = locked.input_index;
    let prev_ticcmd_hex = locked.prev_ticcmd_hex.clone();
    let prev_state_hash_hex = locked.prev_state_hash_hex.clone();
    let covenant_id = locked.covenant_id.clone().or_else(|| cli.covenant_id.clone());
    let session_wallet_address = locked.wallet_address.clone();
    let expected_outpoint = format!("{input_txid}:{input_index}");
    if !locked.started {
        let error = "game session has not been started; call /start or resume a canonical bridge state before POST /tic".to_string();
        let submit_elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        let child_metrics = ChildMetrics::default();
        append_rejected_event(
            cli,
            &payload,
            prev_tick,
            &expected_outpoint,
            &bytes_to_hex(&payload.ticcmd),
            submit_elapsed_ms,
            &error,
            "session_not_started",
            &child_metrics,
            &[],
        )?;
        return Err(error);
    }
    if let Some(session_wallet_address) = session_wallet_address.as_deref()
        && payload.wallet_address != session_wallet_address
    {
        let error = format!("browser wallet {} does not match started game wallet {session_wallet_address}", payload.wallet_address);
        let submit_elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        let child_metrics = ChildMetrics::default();
        append_rejected_event(
            cli,
            &payload,
            prev_tick,
            &expected_outpoint,
            &bytes_to_hex(&payload.ticcmd),
            submit_elapsed_ms,
            &error,
            "wallet_mismatch",
            &child_metrics,
            &[],
        )?;
        return Err(error);
    }
    if payload.ticcmd.len() != 8 {
        let error = format!("ticcmd must contain exactly 8 bytes, got {}", payload.ticcmd.len());
        let submit_elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        let child_metrics = ChildMetrics::default();
        append_rejected_event(
            cli,
            &payload,
            prev_tick,
            &expected_outpoint,
            &bytes_to_hex(&payload.ticcmd),
            submit_elapsed_ms,
            &error,
            "invalid_ticcmd",
            &child_metrics,
            &[],
        )?;
        return Err(error);
    }
    let Some(state_bytes) = payload.state_bytes.as_deref() else {
        let error = "stateBytes is required and must contain a 96-byte KDS4 snapshot".to_string();
        let submit_elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        let child_metrics = ChildMetrics::default();
        append_rejected_event(
            cli,
            &payload,
            prev_tick,
            &expected_outpoint,
            &bytes_to_hex(&payload.ticcmd),
            submit_elapsed_ms,
            &error,
            "invalid_state_snapshot",
            &child_metrics,
            &[],
        )?;
        return Err(error);
    };
    let expected_browser_tick = prev_tick.checked_add(1).ok_or("bridge canonical tick overflow")?;
    if payload.tick != expected_browser_tick {
        let error =
            format!("browser canonical tick {} does not match bridge next canonical tick {expected_browser_tick}", payload.tick);
        let submit_elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        let child_metrics = ChildMetrics::default();
        append_rejected_event(
            cli,
            &payload,
            prev_tick,
            &expected_outpoint,
            &bytes_to_hex(&payload.ticcmd),
            submit_elapsed_ms,
            &error,
            "canonical_tick_mismatch",
            &child_metrics,
            &[],
        )?;
        return Err(error);
    }
    if let Err(error) = validate_kds4_state_snapshot(state_bytes, expected_browser_tick, &payload.ticcmd) {
        let submit_elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        let child_metrics = ChildMetrics::default();
        append_rejected_event(
            cli,
            &payload,
            prev_tick,
            &expected_outpoint,
            &bytes_to_hex(&payload.ticcmd),
            submit_elapsed_ms,
            &error,
            "invalid_state_snapshot",
            &child_metrics,
            &[],
        )?;
        return Err(error);
    }
    if let Some(browser_outpoint) = payload.canonical_outpoint.as_deref().filter(|value| !value.is_empty())
        && browser_outpoint != expected_outpoint
    {
        let error =
            format!("browser canonical outpoint {browser_outpoint} does not match bridge canonical outpoint {expected_outpoint}");
        let submit_elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        let child_metrics = ChildMetrics::default();
        append_rejected_event(
            cli,
            &payload,
            prev_tick,
            &expected_outpoint,
            "",
            submit_elapsed_ms,
            &error,
            "canonical_outpoint_mismatch",
            &child_metrics,
            &[],
        )?;
        return Err(error);
    }
    let next_ticcmd_hex = bytes_to_hex(&payload.ticcmd);
    let next_state_hex = bytes_to_hex(state_bytes);

    if cli.submit_backend == SubmitBackend::InProcess {
        let input_outpoint = TransactionOutpoint::new(doom::parse_txid(&input_txid)?, input_index);
        let covenant_hash =
            covenant_id.as_deref().map(|value| doom::parse_hash(value, "--covenant-id")).transpose()?.unwrap_or(doom::COV_DOOM);
        let current_state = doom::CurrentState::from_cli(prev_tick, prev_ticcmd_hex.as_deref(), prev_state_hash_hex.as_deref())?;
        let built = doom::build_transition(
            &current_state,
            input_outpoint,
            covenant_hash,
            locked.utxo_value.unwrap_or(cli.utxo_value),
            cli.fee,
            Some(payload.ticcmd.clone()),
            Some(state_bytes.to_vec()),
        )?;
        doom::execute_input_with_covenants(built.tx.clone(), built.entries.clone(), 0).map_err(|err| {
            format!("local DoomState covenant validation failed before bridge accept at tick {}: {err}", built.next_tick)
        })?;
        let tx_id = built.tx.id().to_string();
        let successor_outpoint = format!("{tx_id}:0");
        let committed_ticcmd_hex = doom::bytes_to_hex(&built.next_ticcmd);
        let committed_state_hash = doom::bytes_to_hex(&built.next_state_hash);
        let mut child_output = vec![
            "mode=bridge-in-process-dry-run".to_string(),
            format!("next_tick={}", built.next_tick),
            format!("tx_id={tx_id}"),
            format!("successor_outpoint=({}, 0)", built.tx.id()),
            format!("fee_sompi={}", built.fee),
            format!("successor_utxo_value_sompi={}", built.successor_utxo_value),
            format!("next_ticcmd_hex={committed_ticcmd_hex}"),
            format!("next_state_len={}", built.next_state_chunk_len),
            format!("next_state_hash={committed_state_hash}"),
            format!("script_len={}", built.script_len),
            format!("instruction_count={}", built.instruction_count),
            format!("charged_op_count={}", built.charged_op_count),
            format!("sigscript_len={}", built.tx.inputs[0].signature_script.len()),
            "local_validation=ok".to_string(),
        ];
        if cli.submit {
            child_output[0] = "mode=bridge-in-process-live".to_string();
            match submit_built_transition_direct(cli, &current_state, input_outpoint, &built) {
                Ok(rpc_output) => child_output.extend(rpc_output),
                Err(err) => {
                    child_output.extend(err.lines);
                    let submit_elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
                    let child_metrics = parse_child_metrics(&child_output);
                    let rejection_class = child_metrics
                        .rpc_rejection_class
                        .as_deref()
                        .map(str::to_string)
                        .or_else(|| classify_bridge_error(&err.message).map(str::to_string))
                        .unwrap_or_else(|| "in_process_rpc_rejected".to_string());
                    let child_refs = child_output.iter().map(String::as_str).collect::<Vec<_>>();
                    append_rejected_event(
                        cli,
                        &payload,
                        prev_tick,
                        &expected_outpoint,
                        &next_ticcmd_hex,
                        submit_elapsed_ms,
                        &err.message,
                        &rejection_class,
                        &child_metrics,
                        &child_refs,
                    )?;
                    return Err(err.message);
                }
            }
        } else {
            child_output.push("rpc_submit=skipped".to_string());
        }
        return accept_built_tic(
            cli,
            &mut locked,
            &payload,
            started,
            prev_tick,
            &expected_outpoint,
            built.next_tick,
            tx_id,
            successor_outpoint,
            committed_ticcmd_hex,
            committed_state_hash,
            next_state_hex,
            covenant_id,
            child_output,
        );
    }

    let mut command = Command::new(submitter);
    if let Some(url) = &cli.url {
        command.args(["--url", url]);
    }
    command
        .args(["--prev-tick", &prev_tick.to_string()])
        .args(["--ticks", "1"])
        .args(["--next-ticcmd-hex", &next_ticcmd_hex])
        .args(["--input-txid", &input_txid])
        .args(["--input-index", &input_index.to_string()])
        .args(["--utxo-value", &locked.utxo_value.unwrap_or(cli.utxo_value).to_string()])
        .args(["--fee", &cli.fee.to_string()])
        .args(["--track-mempool", bool_arg(cli.track_mempool)])
        .args(["--track-inclusion", bool_arg(cli.track_inclusion)]);
    command.args(["--next-state-hex", &next_state_hex]);
    if let Some(covenant_id) = &covenant_id {
        command.args(["--covenant-id", covenant_id]);
    }
    if cli.submit {
        command
            .arg("--submit")
            .args(["--preflight-input", bool_arg(cli.preflight_input)])
            .args(["--preflight-timeout-ms", &cli.preflight_timeout_ms.to_string()])
            .args(["--preflight-poll-ms", &cli.preflight_poll_ms.to_string()]);
    }
    if let (Some(prev_ticcmd_hex), Some(prev_state_hash_hex)) = (&prev_ticcmd_hex, &prev_state_hash_hex) {
        command.args(["--prev-ticcmd-hex", prev_ticcmd_hex]);
        command.args(["--prev-state-hash-hex", prev_state_hash_hex]);
    }
    if cli.allow_orphan {
        command.arg("--allow-orphan");
    }
    if cli.wait_preflight {
        command.arg("--wait-preflight");
    }

    let output = command.output().map_err(|err| format!("failed to run {}: {err}", submitter.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let child_output = stdout.lines().chain(stderr.lines()).map(str::to_string).collect::<Vec<_>>();
    if !output.status.success() {
        let error = format!(
            "submitter rejected browser tic {} for wallet {}: {}",
            payload.tick,
            payload.wallet_address,
            child_output.join(" | ")
        );
        let rejection_class = parse_string_line(&stdout, "rpc_rejection_class=")
            .or_else(|| classify_bridge_error(&error).map(str::to_string))
            .unwrap_or_else(|| "child_submitter_rejected".to_string());
        let submit_elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        let child_refs = child_output.iter().map(String::as_str).collect::<Vec<_>>();
        let child_metrics = parse_child_metrics(&child_output);
        append_rejected_event(
            cli,
            &payload,
            prev_tick,
            &expected_outpoint,
            &next_ticcmd_hex,
            submit_elapsed_ms,
            &error,
            &rejection_class,
            &child_metrics,
            &child_refs,
        )?;
        return Err(error);
    }

    let next_tick = parse_u32_line(&stdout, "next_tick=").ok_or("submitter output did not include next_tick")?;
    let tx_id = parse_string_line(&stdout, "tx_id=").ok_or("submitter output did not include tx_id")?;
    let committed_ticcmd_hex =
        parse_string_line(&stdout, "next_ticcmd_hex=").ok_or("submitter output did not include next_ticcmd_hex")?;
    let committed_state_hash =
        parse_string_line(&stdout, "next_state_hash=").ok_or("submitter output did not include next_state_hash")?;
    let successor_outpoint =
        parse_successor_outpoint(&stdout).ok_or("submitter output did not include successor_outpoint=(<txid>, <index>)")?;

    accept_built_tic(
        cli,
        &mut locked,
        &payload,
        started,
        prev_tick,
        &expected_outpoint,
        next_tick,
        tx_id,
        successor_outpoint,
        committed_ticcmd_hex,
        committed_state_hash,
        next_state_hex,
        covenant_id,
        child_output,
    )
}

#[allow(clippy::too_many_arguments)]
fn accept_built_tic(
    cli: &Cli,
    locked: &mut BridgeState,
    payload: &TicPayload,
    started: Instant,
    _prev_tick: u32,
    expected_outpoint: &str,
    next_tick: u32,
    tx_id: String,
    successor_outpoint: String,
    committed_ticcmd_hex: String,
    committed_state_hash: String,
    next_state_hex: String,
    covenant_id: Option<String>,
    child_output: Vec<String>,
) -> Result<BridgeResponse, String> {
    let current_utxo_value = locked.utxo_value;
    let fee_sompi = parse_child_metric_u64(&child_output, "fee_sompi=").or(Some(cli.fee));
    let successor_utxo_value = parse_child_metric_u64(&child_output, "successor_utxo_value_sompi=")
        .or_else(|| current_utxo_value.map(|value| value.saturating_sub(cli.fee)));
    locked.prev_tick = next_tick;
    locked.input_txid = tx_id.clone();
    locked.input_index = 0;
    locked.utxo_value = successor_utxo_value;
    locked.prev_ticcmd_hex = Some(committed_ticcmd_hex.clone());
    locked.prev_state_hash_hex = Some(committed_state_hash.clone());
    locked.prev_state_hex = Some(next_state_hex.clone());
    locked.covenant_id = covenant_id;
    locked.started = true;
    persist_bridge_state(&cli.state_file, locked)?;

    println!(
        "browser_tic={} canonical_tick={} tx_id={} successor_outpoint={} ticcmd={} state_hash={} captured_at={} browser_outpoint={}",
        payload.tick,
        next_tick,
        tx_id,
        successor_outpoint,
        committed_ticcmd_hex,
        committed_state_hash,
        payload.captured_at.as_deref().unwrap_or(""),
        payload.canonical_outpoint.as_deref().unwrap_or("")
    );
    let submit_elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    if let Some(event_log) = &cli.event_log {
        let child_metrics = parse_child_metrics(&child_output);
        let child_refs = child_output.iter().map(String::as_str).collect::<Vec<_>>();
        append_bridge_event(
            event_log,
            &BridgeEvent {
                status: "accepted",
                browser_tick: payload.tick,
                canonical_tick: Some(next_tick),
                tx_id: Some(&tx_id),
                successor_outpoint: Some(&successor_outpoint),
                fee_sompi,
                successor_utxo_value_sompi: successor_utxo_value,
                ticcmd_hex: &committed_ticcmd_hex,
                state_hash: Some(&committed_state_hash),
                state_bytes_hex: Some(&next_state_hex),
                covenant_id: locked.covenant_id.as_deref(),
                current_utxo_value_sompi: current_utxo_value,
                submit_elapsed_ms,
                captured_at: payload.captured_at.as_deref(),
                wallet_address: &payload.wallet_address,
                canonical_outpoint_before: expected_outpoint,
                error: None,
                rejection_class: None,
                rpc_submit: child_metrics.rpc_submit.as_deref(),
                mempool_seen: child_metrics.mempool_seen,
                mempool_is_orphan: child_metrics.mempool_is_orphan,
                mempool_seen_elapsed_ms: child_metrics.mempool_seen_elapsed_ms,
                inclusion_seen: child_metrics.inclusion_seen,
                inclusion_seen_elapsed_ms: child_metrics.inclusion_seen_elapsed_ms,
                accepting_block_hash: child_metrics.accepting_block_hash.as_deref(),
                child_output: &child_refs,
            },
        )?;
    }

    Ok(BridgeResponse {
        canonical_tick: next_tick,
        successor_outpoint,
        tx_id,
        fee_sompi,
        successor_utxo_value_sompi: successor_utxo_value,
        ticcmd_hex: committed_ticcmd_hex,
        state_hash: committed_state_hash,
        state_bytes_hex: next_state_hex,
        submit_elapsed_ms,
        child_output,
    })
}

#[derive(Debug)]
struct DirectSubmitError {
    message: String,
    lines: Vec<String>,
}

fn submit_built_transition_direct(
    cli: &Cli,
    current_state: &doom::CurrentState,
    input_outpoint: TransactionOutpoint,
    built: &doom::BuiltTransition,
) -> Result<Vec<String>, DirectSubmitError> {
    let runtime = Runtime::new()
        .map_err(|err| DirectSubmitError { message: format!("failed to create in-process RPC runtime: {err}"), lines: Vec::new() })?;
    runtime.block_on(submit_built_transition_direct_async(cli, current_state, input_outpoint, built))
}

async fn submit_built_transition_direct_async(
    cli: &Cli,
    current_state: &doom::CurrentState,
    input_outpoint: TransactionOutpoint,
    built: &doom::BuiltTransition,
) -> Result<Vec<String>, DirectSubmitError> {
    let mut lines = Vec::new();
    let client = connect_tn10(cli.url.as_deref(), cli.rpc_timeout_ms)
        .await
        .map_err(|message| DirectSubmitError { message, lines: Vec::new() })?;
    let result = async {
        let server_version = assert_toccata_endpoint(&client).await?;
        lines.push(format!("server_version={server_version}"));
        if cli.preflight_input {
            wait_for_starting_input(
                &client,
                current_state,
                input_outpoint,
                cli.wait_preflight,
                Duration::from_millis(cli.preflight_timeout_ms),
                Duration::from_millis(cli.preflight_poll_ms),
                &mut lines,
            )
            .await?;
        }
        let inclusion_start_sink = if cli.track_inclusion {
            match client.get_sink().await {
                Ok(response) => {
                    lines.push(format!("inclusion_start_sink={}", response.sink));
                    Some(response.sink)
                }
                Err(err) => {
                    lines.push("inclusion_start_sink=unavailable".to_string());
                    lines.push(format!("inclusion_start_sink_error={err}"));
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
                lines.push("rpc_submit=ok".to_string());
                lines.push(format!("submitted_tx_id={submitted_id}"));
                lines.push(format!("submit_elapsed_ms={:.2}", rpc_started.elapsed().as_secs_f64() * 1_000.0));
                if cli.track_mempool {
                    track_mempool_visibility(
                        &client,
                        submitted_id,
                        Duration::from_millis(cli.mempool_timeout_ms),
                        Duration::from_millis(cli.mempool_poll_ms),
                        &mut lines,
                    )
                    .await;
                } else {
                    lines.push("mempool_tracking=skipped".to_string());
                }
                if let Some(start_sink) = inclusion_start_sink {
                    track_inclusion(
                        &client,
                        submitted_id,
                        start_sink,
                        Duration::from_millis(cli.inclusion_timeout_ms),
                        Duration::from_millis(cli.inclusion_poll_ms),
                        &mut lines,
                    )
                    .await;
                } else if cli.track_inclusion {
                    lines.push("inclusion_tracking=skipped".to_string());
                }
                Ok(())
            }
            Err(err) => {
                let message = err.to_string();
                lines.push("rpc_submit=rejected".to_string());
                lines.push(format!("rpc_rejection_class={}", doom::classify_rpc_rejection(&message)));
                lines.push(format!("rpc_rejection_detail={message}"));
                Err(format!("TN10 submit_transaction rejected {} at tick {}: {message}", built.tx.id(), built.next_tick))
            }
        }
    }
    .await;
    let disconnect_result = client.disconnect().await.map_err(|err| format!("disconnect failed: {err}"));
    if let Err(message) = result {
        return Err(DirectSubmitError { message, lines });
    }
    if let Err(message) = disconnect_result {
        return Err(DirectSubmitError { message, lines });
    }
    Ok(lines)
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

async fn assert_toccata_endpoint(client: &KaspaRpcClient) -> Result<String, String> {
    let server_info = client.get_server_info().await.map_err(|err| format!("get_server_info failed: {err}"))?;
    let server_version = server_info.server_version.to_ascii_lowercase();
    if !server_version.contains("toc") {
        return Err(format!(
            "refusing live DoomState submit through non-Toccata endpoint {}; pass an upgraded TN10 Toccata wRPC URL such as ws://10.0.3.26:17210",
            server_info.server_version
        ));
    }
    Ok(server_info.server_version)
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
    lines: &mut Vec<String>,
) -> Result<(), String> {
    let started = Instant::now();
    let mut attempt = 0u64;
    loop {
        attempt += 1;
        let result = preflight_starting_input(client, current_state, input_outpoint).await?;
        lines.push(format!("preflight_attempt={attempt}"));
        lines.push(format!("preflight_state_address={}", result.rpc_address));
        lines.push(format!("preflight_state_utxo_count={}", result.utxo_count));
        lines.push(format!("preflight_input_visible={}", result.visible));
        if result.visible {
            return Ok(());
        }
        if !wait || (timeout != Duration::ZERO && started.elapsed() >= timeout) {
            return Err(format!(
                "preflight input ({}, {}) is not visible at {}; wait for node sync or deploy a fresh Toccata-visible DoomState genesis",
                input_outpoint.transaction_id, input_outpoint.index, result.rpc_address
            ));
        }
        lines.push(format!("preflight_wait_elapsed_ms={:.2}", started.elapsed().as_secs_f64() * 1_000.0));
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
    Ok(PreflightResult { rpc_address, utxo_count: utxos.len(), visible })
}

async fn track_mempool_visibility(
    client: &KaspaRpcClient,
    txid: kaspa_rpc_core::RpcTransactionId,
    timeout: Duration,
    poll: Duration,
    lines: &mut Vec<String>,
) {
    let started = Instant::now();
    loop {
        match client.get_mempool_entry(txid, true, false).await {
            Ok(entry) => {
                lines.push("mempool_seen=true".to_string());
                lines.push(format!("mempool_is_orphan={}", entry.is_orphan));
                lines.push(format!("mempool_fee_sompi={}", entry.fee));
                lines.push(format!("mempool_seen_elapsed_ms={:.2}", started.elapsed().as_secs_f64() * 1_000.0));
                return;
            }
            Err(err) if started.elapsed() >= timeout => {
                lines.push("mempool_seen=false".to_string());
                lines.push(format!("mempool_wait_elapsed_ms={:.2}", started.elapsed().as_secs_f64() * 1_000.0));
                lines.push(format!("mempool_last_error={err}"));
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
    lines: &mut Vec<String>,
) {
    let started = Instant::now();
    loop {
        match client.get_virtual_chain_from_block(start_sink, true, None).await {
            Ok(response) => {
                for accepted in &response.accepted_transaction_ids {
                    if accepted.accepted_transaction_ids.iter().any(|accepted_txid| accepted_txid == &txid) {
                        lines.push("inclusion_seen=true".to_string());
                        lines.push(format!("accepting_block_hash={}", accepted.accepting_block_hash));
                        lines.push(format!("inclusion_seen_elapsed_ms={:.2}", started.elapsed().as_secs_f64() * 1_000.0));
                        return;
                    }
                }

                if started.elapsed() >= timeout {
                    lines.push("inclusion_seen=false".to_string());
                    lines.push(format!("inclusion_wait_elapsed_ms={:.2}", started.elapsed().as_secs_f64() * 1_000.0));
                    lines.push(format!("inclusion_added_blocks={}", response.added_chain_block_hashes.len()));
                    lines.push(format!("inclusion_accepted_blocks={}", response.accepted_transaction_ids.len()));
                    return;
                }
            }
            Err(err) if started.elapsed() >= timeout => {
                lines.push("inclusion_seen=false".to_string());
                lines.push(format!("inclusion_wait_elapsed_ms={:.2}", started.elapsed().as_secs_f64() * 1_000.0));
                lines.push(format!("inclusion_last_error={err}"));
                return;
            }
            Err(_) => {}
        }
        sleep(poll).await;
    }
}

fn validate_kds4_state_snapshot(state_bytes: &[u8], expected_tick: u32, expected_ticcmd: &[u8]) -> Result<(), String> {
    if state_bytes.len() != KDS4_STATE_LEN || !state_bytes.starts_with(b"KDS4") {
        let marker = state_bytes.get(0..4).map(bytes_to_hex).unwrap_or_else(|| bytes_to_hex(state_bytes));
        return Err(format!("stateBytes must be a 96-byte KDS4 snapshot, got len={} marker={marker}", state_bytes.len()));
    }
    let state_tick =
        u32::from_le_bytes(state_bytes[4..8].try_into().map_err(|_| "stateBytes KDS4 tick field is malformed".to_string())?);
    if state_tick != expected_tick {
        return Err(format!("stateBytes KDS4 tick {state_tick} does not match expected canonical tick {expected_tick}"));
    }
    let state_ticcmd = &state_bytes[KDS4_TICCMD_OFFSET..KDS4_TICCMD_OFFSET + 8];
    if state_ticcmd != expected_ticcmd {
        return Err(format!(
            "stateBytes KDS4 ticcmd {} does not match payload ticcmd {}",
            bytes_to_hex(state_ticcmd),
            bytes_to_hex(expected_ticcmd)
        ));
    }
    Ok(())
}

fn append_rejected_event(
    cli: &Cli,
    payload: &TicPayload,
    canonical_tick: u32,
    canonical_outpoint_before: &str,
    ticcmd_hex: &str,
    submit_elapsed_ms: f64,
    error: &str,
    rejection_class: &str,
    child_metrics: &ChildMetrics,
    child_output: &[&str],
) -> Result<(), String> {
    let Some(event_log) = &cli.event_log else { return Ok(()) };
    append_bridge_event(
        event_log,
        &BridgeEvent {
            status: "rejected",
            browser_tick: payload.tick,
            canonical_tick: Some(canonical_tick),
            tx_id: None,
            successor_outpoint: None,
            fee_sompi: None,
            successor_utxo_value_sompi: None,
            ticcmd_hex,
            state_hash: None,
            state_bytes_hex: None,
            covenant_id: cli.covenant_id.as_deref(),
            current_utxo_value_sompi: None,
            submit_elapsed_ms,
            captured_at: payload.captured_at.as_deref(),
            wallet_address: &payload.wallet_address,
            canonical_outpoint_before,
            error: Some(error),
            rejection_class: Some(rejection_class),
            rpc_submit: child_metrics.rpc_submit.as_deref(),
            mempool_seen: child_metrics.mempool_seen,
            mempool_is_orphan: child_metrics.mempool_is_orphan,
            mempool_seen_elapsed_ms: child_metrics.mempool_seen_elapsed_ms,
            inclusion_seen: child_metrics.inclusion_seen,
            inclusion_seen_elapsed_ms: child_metrics.inclusion_seen_elapsed_ms,
            accepting_block_hash: child_metrics.accepting_block_hash.as_deref(),
            child_output,
        },
    )
}

fn load_bridge_state(cli: &Cli) -> Result<Option<BridgeState>, String> {
    if !cli.state_file.exists() {
        return Ok(None);
    }
    let state = fs::read_to_string(&cli.state_file).map_err(|err| format!("failed to read {}: {err}", cli.state_file.display()))?;
    let mut parsed: BridgeState =
        serde_json::from_str(&state).map_err(|err| format!("failed to parse {}: {err}", cli.state_file.display()))?;
    if !parsed.started && (parsed.prev_tick > 0 || parsed.covenant_id.is_some()) {
        parsed.started = true;
    }
    validate_loaded_bridge_state(&parsed).map_err(|err| format!("invalid bridge state file {}: {err}", cli.state_file.display()))?;
    println!("loaded_state_file={}", cli.state_file.display());
    Ok(Some(parsed))
}

fn validate_loaded_bridge_state(state: &BridgeState) -> Result<(), String> {
    if state.prev_tick == 0 {
        if state.prev_ticcmd_hex.is_some() || state.prev_state_hash_hex.is_some() || state.prev_state_hex.is_some() {
            return Err("tick-0 bridge state must not contain previous ticcmd, state hash, or state bytes".to_string());
        }
        return Ok(());
    }
    let ticcmd_hex = state.prev_ticcmd_hex.as_deref().ok_or("non-genesis bridge state is missing prev_ticcmd_hex")?;
    let state_hash_hex = state.prev_state_hash_hex.as_deref().ok_or("non-genesis bridge state is missing prev_state_hash_hex")?;
    let state_hex = state.prev_state_hex.as_deref().ok_or("non-genesis bridge state is missing prev_state_hex")?;
    let ticcmd = parse_fixed_hex(ticcmd_hex, 8, "prev_ticcmd_hex")?;
    let expected_hash = parse_fixed_hex(state_hash_hex, 32, "prev_state_hash_hex")?;
    let state_bytes = parse_fixed_hex(state_hex, KDS4_STATE_LEN, "prev_state_hex")?;
    validate_kds4_state_snapshot(&state_bytes, state.prev_tick, &ticcmd)?;
    let actual_hash = blake2b_bytes(&state_bytes);
    if actual_hash != expected_hash {
        return Err(format!(
            "prev_state_hex hash {} does not match prev_state_hash_hex {}",
            bytes_to_hex(&actual_hash),
            state_hash_hex
        ));
    }
    Ok(())
}

fn validate_cli_bridge_state(state: &BridgeState) -> Result<(), String> {
    if state.prev_state_hex.is_some() {
        return validate_loaded_bridge_state(state);
    }
    if state.prev_tick == 0 {
        if state.prev_ticcmd_hex.is_some() || state.prev_state_hash_hex.is_some() {
            return Err("tick-0 bridge state must not contain previous ticcmd or state hash".to_string());
        }
        return Ok(());
    }
    if state.prev_ticcmd_hex.is_some() != state.prev_state_hash_hex.is_some() {
        return Err("non-genesis bridge state must provide prev_ticcmd_hex and prev_state_hash_hex together".to_string());
    }
    if let Some(ticcmd_hex) = state.prev_ticcmd_hex.as_deref() {
        parse_fixed_hex(ticcmd_hex, 8, "prev_ticcmd_hex")?;
    }
    if let Some(state_hash_hex) = state.prev_state_hash_hex.as_deref() {
        parse_fixed_hex(state_hash_hex, 32, "prev_state_hash_hex")?;
    }
    Ok(())
}

fn persist_bridge_state(path: &PathBuf, state: &BridgeState) -> Result<(), String> {
    let body = serde_json::to_string_pretty(state).map_err(|err| format!("failed to encode bridge state: {err}"))?;
    fs::write(path, format!("{body}\n")).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn append_bridge_event(path: &PathBuf, event: &BridgeEvent<'_>) -> Result<(), String> {
    let mut file =
        OpenOptions::new().create(true).append(true).open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    let line = serde_json::to_string(event).map_err(|err| format!("failed to encode bridge event: {err}"))?;
    writeln!(file, "{line}").map_err(|err| format!("failed to append {}: {err}", path.display()))
}

fn parse_child_metrics(child_output: &[String]) -> ChildMetrics {
    let mut metrics = ChildMetrics::default();
    for line in child_output {
        if let Some(value) = line.strip_prefix("rpc_submit=") {
            metrics.rpc_submit = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("rpc_rejection_class=") {
            metrics.rpc_rejection_class = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("mempool_seen=") {
            metrics.mempool_seen = parse_bool(value.trim());
        } else if let Some(value) = line.strip_prefix("mempool_is_orphan=") {
            metrics.mempool_is_orphan = parse_bool(value.trim());
        } else if let Some(value) = line.strip_prefix("mempool_seen_elapsed_ms=") {
            metrics.mempool_seen_elapsed_ms = value.trim().parse().ok();
        } else if let Some(value) = line.strip_prefix("inclusion_seen=") {
            metrics.inclusion_seen = parse_bool(value.trim());
        } else if let Some(value) = line.strip_prefix("inclusion_seen_elapsed_ms=") {
            metrics.inclusion_seen_elapsed_ms = value.trim().parse().ok();
        } else if let Some(value) = line.strip_prefix("accepting_block_hash=") {
            metrics.accepting_block_hash = Some(value.trim().to_string());
        }
    }
    metrics
}

fn parse_child_metric_u64(child_output: &[String], prefix: &str) -> Option<u64> {
    child_output.iter().find_map(|line| line.strip_prefix(prefix)?.trim().parse().ok())
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn synthetic_bridge_txid(prev_tick: u32) -> String {
    format!("{:064x}", (prev_tick + 1) & 0xff)
}

fn bool_arg(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

fn parse_hex(hex: &str, name: &str) -> Result<Vec<u8>, String> {
    if hex.len() % 2 != 0 {
        return Err(format!("{name} must have an even number of hex chars, got {}", hex.len()));
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for idx in (0..hex.len()).step_by(2) {
        let byte = u8::from_str_radix(&hex[idx..idx + 2], 16)
            .map_err(|err| format!("{name} has invalid hex byte at chars {idx}..{}: {err}", idx + 2))?;
        out.push(byte);
    }
    Ok(out)
}

fn parse_fixed_hex(hex: &str, expected_bytes: usize, name: &str) -> Result<Vec<u8>, String> {
    let expected_len = expected_bytes * 2;
    if hex.len() != expected_len {
        return Err(format!("{name} must be exactly {expected_len} hex chars for {expected_bytes} bytes, got {}", hex.len()));
    }
    parse_hex(hex, name)
}

fn blake2b_bytes(data: &[u8]) -> Vec<u8> {
    Blake2bParams::new().hash_length(32).hash(data).as_bytes().to_vec()
}

fn header_len(request: &[u8]) -> Option<usize> {
    request.windows(4).position(|window| window == b"\r\n\r\n").map(|idx| idx + 4)
}

fn content_length(request: &[u8]) -> Result<usize, String> {
    let header_len = header_len(request).ok_or("missing HTTP headers")?;
    let headers = String::from_utf8_lossy(&request[..header_len]);
    for line in headers.lines() {
        let Some((name, value)) = line.split_once(':') else { continue };
        if name.eq_ignore_ascii_case("content-length") {
            return value.trim().parse::<usize>().map_err(|err| format!("invalid content-length: {err}"));
        }
    }
    Ok(0)
}

fn parse_string_line(output: &str, prefix: &str) -> Option<String> {
    output.lines().find_map(|line| line.strip_prefix(prefix).map(str::trim).map(str::to_string))
}

fn classify_bridge_error(error: &str) -> Option<&'static str> {
    if error.contains("canonical outpoint") {
        Some("canonical_outpoint_mismatch")
    } else if error.contains("preflight input") {
        Some("preflight_input_missing")
    } else if error.contains("non-Toccata") {
        Some("endpoint_missing_toccata")
    } else {
        None
    }
}

fn parse_u32_line(output: &str, prefix: &str) -> Option<u32> {
    parse_string_line(output, prefix)?.parse().ok()
}

fn parse_u64_line(output: &str, prefix: &str) -> Option<u64> {
    parse_string_line(output, prefix)?.parse().ok()
}

fn parse_bool_line(output: &str, prefix: &str) -> Option<bool> {
    parse_bool(parse_string_line(output, prefix)?.trim())
}

fn parse_successor_outpoint(output: &str) -> Option<String> {
    parse_outpoint_line(output, "successor_outpoint=")
}

fn parse_outpoint_line(output: &str, prefix: &str) -> Option<String> {
    let value = parse_string_line(output, prefix)?;
    let value = value.strip_prefix('(')?.strip_suffix(')')?;
    let (txid, index) = value.split_once(',')?;
    Some(format!("{}:{}", txid.trim(), index.trim()))
}

fn split_outpoint(outpoint: &str) -> Result<(String, u32), String> {
    let (txid, index) = outpoint.split_once(':').ok_or_else(|| format!("invalid outpoint {outpoint}: expected <txid>:<index>"))?;
    let index = index.parse().map_err(|err| format!("invalid outpoint index in {outpoint}: {err}"))?;
    Ok((txid.to_string(), index))
}

fn query_param(target: &str, key: &str) -> Option<String> {
    let query = target.split_once('?')?.1;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        if percent_decode(name) == key {
            return Some(percent_decode(value));
        }
    }
    None
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'+' => {
                out.push(b' ');
                idx += 1;
            }
            b'%' if idx + 2 < bytes.len() => {
                let hi = hex_value(bytes[idx + 1]);
                let lo = hex_value(bytes[idx + 2]);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi << 4) | lo);
                    idx += 3;
                } else {
                    out.push(bytes[idx]);
                    idx += 1;
                }
            }
            byte => {
                out.push(byte);
                idx += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn write_response(stream: &mut TcpStream, status: u16, reason: &str, body: &str) -> Result<(), String> {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: content-type\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).map_err(|err| format!("failed to write response: {err}"))
}
