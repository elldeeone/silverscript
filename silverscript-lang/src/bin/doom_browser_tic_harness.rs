use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use clap::Parser;
use serde::{Deserialize, Serialize};
use silverscript_lang::doom_tn10 as doom;
use tokio::time::sleep;

const DEFAULT_BRIDGE_URL: &str = "http://127.0.0.1:8787";
const DEFAULT_WALLET_ADDRESS: &str = "kaspatest:qzma22j09zrjn5zxw8mx3epm49etfma9y9jc6z80g43mwyk0svvg6h34ars7t";

#[derive(Debug, Parser)]
#[command(
    name = "doom-browser-tic-harness",
    about = "Exercise the browser Doom -> local bridge tic contract without a compiled WASM build",
    next_line_help = true
)]
struct Cli {
    /// Local bridge base URL, for example http://127.0.0.1:8787.
    #[arg(long = "bridge-url", default_value = DEFAULT_BRIDGE_URL)]
    bridge_url: String,

    /// Wallet address to send in browser-shaped tic payloads.
    #[arg(long = "wallet-address", default_value = DEFAULT_WALLET_ADDRESS)]
    wallet_address: String,

    /// Number of canonical browser-shaped tics to submit.
    #[arg(long = "ticks", default_value_t = 1)]
    ticks: u32,

    /// Browser target cadence. Set 1 for the current practical live target.
    #[arg(long = "target-tps", default_value_t = 1.0)]
    target_tps: f64,

    /// Do not POST tics; only verify that /state is resumable.
    #[arg(long = "state-only", default_value_t = false)]
    state_only: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeState {
    canonical_tick: u32,
    canonical_outpoint: String,
    utxo_value: Option<u64>,
    started: bool,
    wallet_address: Option<String>,
    ticcmd_hex: Option<String>,
    state_hash: Option<String>,
    state_bytes_hex: Option<String>,
    covenant_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TicPayload<'a> {
    tick: u32,
    target_tps: f64,
    wallet_address: &'a str,
    canonical_outpoint: &'a str,
    ticcmd: Vec<u8>,
    state_bytes: Vec<u8>,
    captured_at: &'a str,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TicResponse {
    canonical_tick: u32,
    successor_outpoint: String,
    tx_id: String,
    fee_sompi: Option<u64>,
    successor_utxo_value_sompi: Option<u64>,
    ticcmd_hex: String,
    state_hash: String,
    state_bytes_hex: String,
    submit_elapsed_ms: f64,
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
    if cli.ticks == 0 && !cli.state_only {
        return Err("--ticks must be at least 1 unless --state-only is used".to_string());
    }
    if cli.target_tps <= 0.0 {
        return Err("--target-tps must be greater than zero".to_string());
    }

    let mut state = get_state(&cli.bridge_url)?;
    if !state.started {
        return Err("bridge /state reports started=false; call /start or resume a bridge state file first".to_string());
    }
    if state.wallet_address.as_deref() != Some(cli.wallet_address.as_str()) {
        return Err(format!("bridge wallet {:?} does not match harness wallet {}", state.wallet_address, cli.wallet_address));
    }

    println!("mode=doom-browser-tic-harness");
    println!("bridge_url={}", cli.bridge_url);
    println!("wallet_address={}", cli.wallet_address);
    println!("start_canonical_tick={}", state.canonical_tick);
    println!("start_canonical_outpoint={}", state.canonical_outpoint);
    if let Some(value) = state.utxo_value {
        println!("start_utxo_value_sompi={value}");
    }
    if let Some(covenant_id) = state.covenant_id.as_deref() {
        println!("covenant_id={covenant_id}");
    }
    if cli.state_only {
        println!("state_only=true");
        return Ok(());
    }

    let pace = Duration::from_secs_f64(1.0 / cli.target_tps);
    for step in 0..cli.ticks {
        if step > 0 {
            sleep(pace).await;
        }
        let next_tick = state.canonical_tick.checked_add(1).ok_or("canonical tick overflow")?;
        let ticcmd = doom::ticcmd_for_tick(next_tick);
        let state_bytes = doom::state_chunk_for_tick_and_ticcmd(next_tick, &ticcmd);
        doom::validate_kds4_state_snapshot(&state_bytes, next_tick, &ticcmd)?;
        let payload = TicPayload {
            tick: next_tick,
            target_tps: cli.target_tps,
            wallet_address: &cli.wallet_address,
            canonical_outpoint: &state.canonical_outpoint,
            ticcmd,
            state_bytes,
            captured_at: "browser-harness",
        };
        let response: TicResponse = post_json(&cli.bridge_url, "/tic", &payload)?;
        println!("tick_step={step}");
        println!("canonical_tick={}", response.canonical_tick);
        println!("tx_id={}", response.tx_id);
        println!("successor_outpoint={}", response.successor_outpoint);
        if let Some(fee) = response.fee_sompi {
            println!("fee_sompi={fee}");
        }
        if let Some(value) = response.successor_utxo_value_sompi {
            println!("successor_utxo_value_sompi={value}");
        }
        println!("ticcmd_hex={}", response.ticcmd_hex);
        println!("state_hash={}", response.state_hash);
        println!("state_bytes_len={}", response.state_bytes_hex.len() / 2);
        println!("submit_elapsed_ms={:.2}", response.submit_elapsed_ms);
        state = get_state(&cli.bridge_url)?;
    }

    println!("ticks_attempted={}", cli.ticks);
    println!("final_canonical_tick={}", state.canonical_tick);
    println!("final_canonical_outpoint={}", state.canonical_outpoint);
    if let Some(value) = state.utxo_value {
        println!("final_utxo_value_sompi={value}");
    }
    if let Some(ticcmd) = state.ticcmd_hex.as_deref() {
        println!("final_ticcmd_hex={ticcmd}");
    }
    if let Some(state_hash) = state.state_hash.as_deref() {
        println!("final_state_hash={state_hash}");
    }
    if let Some(state_bytes) = state.state_bytes_hex.as_deref() {
        println!("final_state_bytes_len={}", state_bytes.len() / 2);
    }
    Ok(())
}

fn get_state(base_url: &str) -> Result<BridgeState, String> {
    get_json(base_url, "/state")
}

fn get_json<T: for<'de> Deserialize<'de>>(base_url: &str, path: &str) -> Result<T, String> {
    let response = http_request(base_url, "GET", path, None)?;
    serde_json::from_str(&response).map_err(|err| format!("failed to parse {path} JSON response: {err}; body={response}"))
}

fn post_json<T: Serialize, R: for<'de> Deserialize<'de>>(base_url: &str, path: &str, payload: &T) -> Result<R, String> {
    let body = serde_json::to_string(payload).map_err(|err| format!("failed to encode {path} request JSON: {err}"))?;
    let response = http_request(base_url, "POST", path, Some(&body))?;
    serde_json::from_str(&response).map_err(|err| format!("failed to parse {path} JSON response: {err}; body={response}"))
}

fn http_request(base_url: &str, method: &str, path: &str, body: Option<&str>) -> Result<String, String> {
    let (host, port) = parse_http_base_url(base_url)?;
    let mut stream = TcpStream::connect((host.as_str(), port)).map_err(|err| format!("failed to connect to {host}:{port}: {err}"))?;
    let body = body.unwrap_or("");
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
    .map_err(|err| format!("failed to write HTTP request: {err}"))?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|err| format!("failed to read HTTP response: {err}"))?;
    parse_http_response(&response)
}

fn parse_http_base_url(url: &str) -> Result<(String, u16), String> {
    let rest = url.strip_prefix("http://").ok_or_else(|| format!("only http:// bridge URLs are supported, got {url:?}"))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = authority.rsplit_once(':').ok_or_else(|| format!("bridge URL must include host:port, got {url:?}"))?;
    let port = port.parse::<u16>().map_err(|err| format!("invalid bridge URL port {port:?}: {err}"))?;
    Ok((host.to_string(), port))
}

fn parse_http_response(response: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(response);
    let (header, body) = text.split_once("\r\n\r\n").ok_or_else(|| format!("malformed HTTP response: {text}"))?;
    let status = header.lines().next().unwrap_or("");
    if !status.contains(" 200 ") {
        return Err(format!("bridge returned {status}: {body}"));
    }
    Ok(body.to_string())
}
