use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use blake2b_simd::Params as Blake2bParams;
use clap::Parser;
use serde_json::Value;

const DEFAULT_WALLET_ADDRESS: &str = "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz";
const DEFAULT_URL: &str = "wss://testnet10-wrpc.kasia.fyi";
const DEFAULT_INPUT_TXID: &str = "44958358cd848ef43c91112738ef8be5ffed574982e152be98913eeb94b726c0";
const DEFAULT_COVENANT_ID: &str = "b33892ffcac705a87ce7a747cf8e289c7d48d87cecf1469731f5374a13625fb0";

#[derive(Debug, Parser)]
#[command(
    name = "doom-tn10-smoke",
    about = "Run a real local bridge dry-run smoke test for browser start/tic/resume wiring",
    next_line_help = true
)]
struct Cli {
    /// Local bridge listen address.
    #[arg(long, default_value = "127.0.0.1:8799")]
    listen: String,

    /// TN10 wRPC endpoint passed to the bridge. Used for readiness/start diagnostics even in dry-run mode.
    #[arg(long, default_value = DEFAULT_URL)]
    url: String,

    /// Wallet address posted to the bridge.
    #[arg(long = "wallet-address", default_value = DEFAULT_WALLET_ADDRESS)]
    wallet_address: String,

    /// Timeout for the whole smoke test.
    #[arg(long = "timeout-ms", default_value_t = 30_000)]
    timeout_ms: u64,

    /// Number of canonical browser tics to submit through the bridge.
    #[arg(long, default_value_t = 1)]
    ticks: u32,

    /// Target authoritative cadence used for smoke reporting.
    #[arg(long = "target-tps", default_value_t = 1.0)]
    target_tps: f64,

    /// Captured game cadence encoded into each browser tic timestamp for report TPS calculations.
    #[arg(long = "captured-tps", default_value_t = 1.0)]
    captured_tps: f64,

    /// Fail if measured local smoke throughput is below this TPS.
    #[arg(long = "min-smoke-tps")]
    min_smoke_tps: Option<f64>,

    /// Fail if measured local smoke throughput divided by --target-tps is below this ratio.
    #[arg(long = "require-smoke-vs-target")]
    require_smoke_vs_target: Option<f64>,

    /// Kill and restart the bridge after this accepted canonical tick, then continue from the persisted state.
    #[arg(long = "restart-after-tick")]
    restart_after_tick: Option<u32>,

    /// Keep the temporary bridge state and event log files after the smoke run.
    #[arg(long = "keep-artifacts", default_value_t = false)]
    keep_artifacts: bool,

    /// After the first accepted tic, submit one intentionally skipped canonical tick and verify rejection.
    #[arg(long = "probe-bad-tick", default_value_t = false)]
    probe_bad_tick: bool,

    /// After the first accepted tic, submit one old-format KDS2 state snapshot and verify rejection.
    #[arg(long = "probe-bad-state", default_value_t = false)]
    probe_bad_state: bool,

    /// After the first accepted tic, submit one KDS4 snapshot whose embedded ticcmd differs from the payload.
    #[arg(long = "probe-bad-state-ticcmd", default_value_t = false)]
    probe_bad_state_ticcmd: bool,

    /// After start, submit one tic using a different wallet address and verify rejection.
    #[arg(long = "probe-wallet-mismatch", default_value_t = false)]
    probe_wallet_mismatch: bool,

    /// After the main smoke, verify a bridge restart rejects a corrupted persisted KDS4 resume state.
    #[arg(long = "probe-corrupt-resume", default_value_t = false)]
    probe_corrupt_resume: bool,

    /// After the main smoke, verify a fresh bridge can hydrate from explicit resume tuple CLI args.
    #[arg(long = "probe-cli-resume", default_value_t = false)]
    probe_cli_resume: bool,

    /// After the main smoke, verify live in-process mode rejects the current invisible genesis at preflight and logs it.
    #[arg(long = "probe-live-preflight", default_value_t = false)]
    probe_live_preflight: bool,

    /// Bridge submit backend to exercise during smoke, for example child or in-process.
    #[arg(long = "submit-backend", default_value = "child")]
    submit_backend: String,

    /// Bridge executable to launch. Defaults to the sibling doom_tn10_bridge binary next to this smoke binary.
    #[arg(long = "bridge-bin")]
    bridge_bin: Option<PathBuf>,

    /// Allow launching a bridge executable that is older than doom_tn10_bridge.rs.
    #[arg(long = "allow-stale-bridge-bin", default_value_t = false)]
    allow_stale_bridge_bin: bool,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let bridge = cli.bridge_bin.clone().map(Ok).unwrap_or_else(|| sibling_bin("doom_tn10_bridge"))?;
    validate_bridge_bin(&bridge, cli.allow_stale_bridge_bin)?;
    let pid = std::process::id();
    let state_file = format!(".doom-tn10-smoke-{pid}.state.json");
    let event_log = format!(".doom-tn10-smoke-{pid}.events.jsonl");
    let started = Instant::now();
    let timeout = Duration::from_millis(cli.timeout_ms);
    let mut child = spawn_bridge(&bridge, &cli, &state_file, &event_log)?;

    let result = run_smoke(&cli, &bridge, &state_file, &event_log, &mut child, started, timeout);
    stop_child(&mut child);
    if !cli.keep_artifacts {
        let _ = std::fs::remove_file(&state_file);
        let _ = std::fs::remove_file(&event_log);
    }
    result
}

fn run_smoke(
    cli: &Cli,
    bridge: &PathBuf,
    state_file: &str,
    event_log: &str,
    child: &mut Child,
    started: Instant,
    timeout: Duration,
) -> Result<(), String> {
    if cli.ticks == 0 {
        return Err("--ticks must be greater than 0".to_string());
    }
    if !cli.target_tps.is_finite() || cli.target_tps <= 0.0 {
        return Err("--target-tps must be a positive finite number".to_string());
    }
    if !cli.captured_tps.is_finite() || cli.captured_tps <= 0.0 {
        return Err("--captured-tps must be a positive finite number".to_string());
    }
    if let Some(min_smoke_tps) = cli.min_smoke_tps {
        if !min_smoke_tps.is_finite() || min_smoke_tps <= 0.0 {
            return Err("--min-smoke-tps must be a positive finite number".to_string());
        }
    }
    if let Some(require_smoke_vs_target) = cli.require_smoke_vs_target {
        if !require_smoke_vs_target.is_finite() || require_smoke_vs_target <= 0.0 {
            return Err("--require-smoke-vs-target must be a positive finite number".to_string());
        }
    }
    if let Some(restart_after_tick) = cli.restart_after_tick {
        if restart_after_tick == 0 || restart_after_tick >= cli.ticks {
            return Err("--restart-after-tick must be greater than 0 and less than --ticks".to_string());
        }
    }

    wait_for_bridge(&cli.listen, started, timeout)?;
    let ready = get_json(&cli.listen, &format!("/ready?walletAddress={}", url_encode_query_value(&cli.wallet_address)))?;
    let ready_start_mode = string_field(&ready, "startMode")?;
    if ready_start_mode != "synthetic_dry_run" {
        return Err(format!("dry-run bridge /ready startMode {ready_start_mode}, expected synthetic_dry_run"));
    }
    if !bool_field(&ready, "startAvailable")? {
        return Err(format!("dry-run bridge /ready reported startAvailable=false: {ready}"));
    }
    if bool_field(&ready, "submit")? {
        return Err(format!("dry-run bridge /ready reported submit=true: {ready}"));
    }
    if ready.get("walletKeyAvailable").is_none() || ready.get("walletKeyMatches").is_none() {
        return Err(format!("bridge /ready response did not expose wallet key status: {ready}"));
    }
    let prestart_state = get_json(&cli.listen, "/state")?;
    if bool_field(&prestart_state, "started")? {
        return Err(format!("bridge /state reported started=true before /start: {prestart_state}"));
    }
    let prestart_rejected = post_json_expect_error(
        &cli.listen,
        "/tic",
        &serde_json::json!({
            "tick": 1,
            "walletAddress": cli.wallet_address,
            "canonicalOutpoint": "",
            "ticcmd": ticcmd_bytes(1),
            "stateBytes": state_bytes(1),
            "capturedAt": captured_at(1, cli.captured_tps)?,
        }),
    )?;
    let prestart_error = string_field(&prestart_rejected, "error")?;
    if !prestart_error.contains("game session has not been started") {
        return Err(format!("pre-start /tic rejection was unexpected: {prestart_rejected}"));
    }
    let start = post_json(
        &cli.listen,
        "/start",
        &serde_json::json!({
            "walletAddress": cli.wallet_address,
        }),
    )?;
    let start_outpoint = string_field(&start, "initialGameOutpoint")?;
    let synthetic = bool_field(&start, "synthetic")?;
    let started_state = get_json(&cli.listen, "/state")?;
    if !bool_field(&started_state, "started")? {
        return Err(format!("bridge /state reported started=false after /start: {started_state}"));
    }
    let started_outpoint = string_field(&started_state, "canonicalOutpoint")?;
    if started_outpoint != start_outpoint {
        return Err(format!("bridge /state outpoint {started_outpoint} did not match /start outpoint {start_outpoint}"));
    }
    let started_wallet = string_field(&started_state, "walletAddress")?;
    if started_wallet != cli.wallet_address {
        return Err(format!("bridge /state wallet {started_wallet} did not match started wallet {}", cli.wallet_address));
    }

    let first_tic_started = Instant::now();
    let mut canonical_outpoint = start_outpoint.clone();
    let mut last_tic = None;
    let mut restart_state = None;
    let mut bad_tick_probe = None;
    let mut bad_state_probe = None;
    let mut bad_state_ticcmd_probe = None;
    let mut wallet_mismatch_probe = None;
    for tick in 1..=cli.ticks {
        if cli.probe_wallet_mismatch && tick == 1 {
            let rejected = post_json_expect_error(
                &cli.listen,
                "/tic",
                &serde_json::json!({
                    "tick": tick,
                    "walletAddress": "kaspatest:qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq",
                    "canonicalOutpoint": canonical_outpoint,
                    "ticcmd": ticcmd_bytes(tick),
                    "stateBytes": state_bytes(tick),
                    "capturedAt": captured_at(tick, cli.captured_tps)?,
                }),
            )?;
            let rejection = string_field(&rejected, "error")?;
            if !rejection.contains("does not match started game wallet") {
                return Err(format!("wallet mismatch rejection was unexpected: {rejected}"));
            }
            let canonical_tick_after_reject = u64_field(&rejected, "canonicalTick")?;
            if canonical_tick_after_reject != 0 {
                return Err(format!("wallet mismatch rejection reported canonical tick {canonical_tick_after_reject}, expected 0"));
            }
            wallet_mismatch_probe = Some(());
        }
        let tic = post_json(
            &cli.listen,
            "/tic",
            &serde_json::json!({
                "tick": tick,
                "walletAddress": cli.wallet_address,
                "canonicalOutpoint": canonical_outpoint,
                "ticcmd": ticcmd_bytes(tick),
                "stateBytes": state_bytes(tick),
                "capturedAt": captured_at(tick, cli.captured_tps)?,
            }),
        )?;
        let canonical_tick = u64_field(&tic, "canonicalTick")?;
        if canonical_tick != tick as u64 {
            return Err(format!("bridge accepted tick {tick} as unexpected canonical tick {canonical_tick}"));
        }
        canonical_outpoint = string_field(&tic, "successorOutpoint")?;
        last_tic = Some(tic);
        if cli.probe_bad_tick && tick == 1 {
            let skipped_tick = tick + 2;
            let rejected = post_json_expect_error(
                &cli.listen,
                "/tic",
                &serde_json::json!({
                    "tick": skipped_tick,
                    "walletAddress": cli.wallet_address,
                    "canonicalOutpoint": canonical_outpoint,
                    "ticcmd": ticcmd_bytes(skipped_tick),
                    "stateBytes": state_bytes(skipped_tick),
                    "capturedAt": captured_at(skipped_tick, cli.captured_tps)?,
                }),
            )?;
            let canonical_tick_after_reject = u64_field(&rejected, "canonicalTick")?;
            let canonical_outpoint_after_reject = string_field(&rejected, "canonicalOutpoint")?;
            if canonical_tick_after_reject != tick as u64 {
                return Err(format!("bad tick rejection reported canonical tick {canonical_tick_after_reject}, expected {tick}"));
            }
            if canonical_outpoint_after_reject != canonical_outpoint {
                return Err(format!(
                    "bad tick rejection reported canonical outpoint {canonical_outpoint_after_reject}, expected {canonical_outpoint}"
                ));
            }
            bad_tick_probe = Some(BadTickProbe { skipped_tick, canonical_tick: tick, canonical_outpoint: canonical_outpoint.clone() });
        }
        if cli.probe_bad_state && tick == 1 {
            let rejected = post_json_expect_error(
                &cli.listen,
                "/tic",
                &serde_json::json!({
                    "tick": tick + 1,
                    "walletAddress": cli.wallet_address,
                    "canonicalOutpoint": canonical_outpoint,
                    "ticcmd": ticcmd_bytes(tick + 1),
                    "stateBytes": legacy_state_bytes(tick + 1),
                    "capturedAt": captured_at(tick + 1, cli.captured_tps)?,
                }),
            )?;
            let canonical_tick_after_reject = u64_field(&rejected, "canonicalTick")?;
            let canonical_outpoint_after_reject = string_field(&rejected, "canonicalOutpoint")?;
            if canonical_tick_after_reject != tick as u64 {
                return Err(format!("bad state rejection reported canonical tick {canonical_tick_after_reject}, expected {tick}"));
            }
            if canonical_outpoint_after_reject != canonical_outpoint {
                return Err(format!(
                    "bad state rejection reported canonical outpoint {canonical_outpoint_after_reject}, expected {canonical_outpoint}"
                ));
            }
            bad_state_probe = Some(BadStateProbe { canonical_tick: tick, canonical_outpoint: canonical_outpoint.clone() });
        }
        if cli.probe_bad_state_ticcmd && tick == 1 {
            let rejected = post_json_expect_error(
                &cli.listen,
                "/tic",
                &serde_json::json!({
                    "tick": tick + 1,
                    "walletAddress": cli.wallet_address,
                    "canonicalOutpoint": canonical_outpoint,
                    "ticcmd": ticcmd_bytes(tick + 1),
                    "stateBytes": mismatched_ticcmd_state_bytes(tick + 1),
                    "capturedAt": captured_at(tick + 1, cli.captured_tps)?,
                }),
            )?;
            let canonical_tick_after_reject = u64_field(&rejected, "canonicalTick")?;
            let canonical_outpoint_after_reject = string_field(&rejected, "canonicalOutpoint")?;
            if canonical_tick_after_reject != tick as u64 {
                return Err(format!(
                    "bad state ticcmd rejection reported canonical tick {canonical_tick_after_reject}, expected {tick}"
                ));
            }
            if canonical_outpoint_after_reject != canonical_outpoint {
                return Err(format!(
                    "bad state ticcmd rejection reported canonical outpoint {canonical_outpoint_after_reject}, expected {canonical_outpoint}"
                ));
            }
            bad_state_ticcmd_probe = Some(BadStateProbe { canonical_tick: tick, canonical_outpoint: canonical_outpoint.clone() });
        }
        if cli.restart_after_tick == Some(tick) {
            stop_child(child);
            *child = spawn_bridge(bridge, cli, state_file, event_log)?;
            wait_for_bridge(&cli.listen, Instant::now(), remaining_timeout(started, timeout)?)?;
            let state_after_restart = get_json(&cli.listen, "/state")?;
            if !bool_field(&state_after_restart, "started")? {
                return Err(format!("bridge restart loaded started=false state: {state_after_restart}"));
            }
            let restarted_tick = u64_field(&state_after_restart, "canonicalTick")?;
            let restarted_outpoint = string_field(&state_after_restart, "canonicalOutpoint")?;
            if restarted_tick != tick as u64 {
                return Err(format!("bridge restart loaded canonical tick {restarted_tick}, expected {tick}"));
            }
            if restarted_outpoint != canonical_outpoint {
                return Err(format!("bridge restart loaded canonical outpoint {restarted_outpoint}, expected {canonical_outpoint}"));
            }
            let expected_restart_hash = state_hash_hex(&state_bytes(tick));
            let restarted_hash = string_field(&state_after_restart, "stateHash")?;
            if restarted_hash != expected_restart_hash {
                return Err(format!("bridge restart loaded state hash {restarted_hash}, expected {expected_restart_hash}"));
            }
            let expected_restart_state_hex = bytes_to_hex(&state_bytes(tick));
            let restarted_state_hex = string_field(&state_after_restart, "stateBytesHex")?;
            if restarted_state_hex != expected_restart_state_hex {
                return Err(format!("bridge restart loaded state bytes {restarted_state_hex}, expected {expected_restart_state_hex}"));
            }
            restart_state = Some(state_after_restart);
        }
    }
    let smoke_elapsed = first_tic_started.elapsed();
    let smoke_elapsed_ms = smoke_elapsed.as_secs_f64() * 1000.0;
    let smoke_tps = cli.ticks as f64 / smoke_elapsed.as_secs_f64().max(0.001);
    let smoke_vs_target = smoke_tps / cli.target_tps;
    let state_bytes_per_tick = state_bytes(cli.ticks).len();
    let target_state_bytes_per_second = state_bytes_per_tick as f64 * cli.target_tps;
    let captured_state_bytes_per_second = state_bytes_per_tick as f64 * cli.captured_tps;
    let smoke_state_bytes_per_second = state_bytes_per_tick as f64 * smoke_tps;
    if let Some(min_smoke_tps) = cli.min_smoke_tps {
        if smoke_tps < min_smoke_tps {
            return Err(format!("smoke_tps {smoke_tps:.3} is below required --min-smoke-tps {min_smoke_tps:.3}"));
        }
    }
    if let Some(require_smoke_vs_target) = cli.require_smoke_vs_target {
        if smoke_vs_target < require_smoke_vs_target {
            return Err(format!(
                "smoke_vs_target {smoke_vs_target:.3} is below required --require-smoke-vs-target {require_smoke_vs_target:.3}"
            ));
        }
    }
    let last_tic = last_tic.ok_or_else(|| "missing last tic response".to_string())?;
    let state = get_json(&cli.listen, "/state")?;
    if !bool_field(&state, "started")? {
        return Err(format!("bridge final /state reported started=false: {state}"));
    }
    let event_summary = summarize_event_log(event_log, cli.ticks)?;
    let expected_rejections = 1
        + usize::from(cli.probe_wallet_mismatch)
        + usize::from(cli.probe_bad_tick)
        + usize::from(cli.probe_bad_state)
        + usize::from(cli.probe_bad_state_ticcmd);
    if event_summary.accepted_events != cli.ticks as usize {
        return Err(format!("event log accepted {} tics, expected {}", event_summary.accepted_events, cli.ticks));
    }
    if event_summary.rejected_events != expected_rejections {
        return Err(format!("event log contains {} rejected tics, expected {expected_rejections}", event_summary.rejected_events));
    }
    if cli.probe_bad_tick && event_summary.canonical_tick_mismatch_rejections != 1 {
        return Err(format!(
            "event log canonical_tick_mismatch count {}, expected 1",
            event_summary.canonical_tick_mismatch_rejections
        ));
    }
    if !cli.probe_bad_tick && event_summary.canonical_tick_mismatch_rejections != 0 {
        return Err(format!(
            "event log canonical_tick_mismatch count {}, expected 0",
            event_summary.canonical_tick_mismatch_rejections
        ));
    }
    let expected_invalid_state_rejections = usize::from(cli.probe_bad_state) + usize::from(cli.probe_bad_state_ticcmd);
    if event_summary.invalid_state_snapshot_rejections != expected_invalid_state_rejections {
        return Err(format!(
            "event log invalid_state_snapshot count {}, expected {expected_invalid_state_rejections}",
            event_summary.invalid_state_snapshot_rejections
        ));
    }
    if event_summary.session_not_started_rejections != 1 {
        return Err(format!("event log session_not_started count {}, expected 1", event_summary.session_not_started_rejections));
    }
    let expected_wallet_mismatch_rejections = usize::from(cli.probe_wallet_mismatch);
    if event_summary.wallet_mismatch_rejections != expected_wallet_mismatch_rejections {
        return Err(format!(
            "event log wallet_mismatch count {}, expected {expected_wallet_mismatch_rejections}",
            event_summary.wallet_mismatch_rejections
        ));
    }
    if event_summary.last_canonical_tick != Some(cli.ticks) {
        return Err(format!("event log last canonical tick {:?}, expected {}", event_summary.last_canonical_tick, cli.ticks));
    }
    let state_canonical_outpoint = string_field(&state, "canonicalOutpoint")?;
    let state_hash = string_field(&state, "stateHash")?;
    let state_bytes_hex = string_field(&state, "stateBytesHex")?;
    let expected_final_state_hash = state_hash_hex(&state_bytes(cli.ticks));
    let expected_final_state_hex = bytes_to_hex(&state_bytes(cli.ticks));
    if event_summary.last_successor_outpoint.as_deref() != Some(state_canonical_outpoint.as_str()) {
        return Err(format!(
            "event log last outpoint {:?} does not match bridge state {state_canonical_outpoint}",
            event_summary.last_successor_outpoint
        ));
    }
    if event_summary.last_state_hash.as_deref() != Some(state_hash.as_str()) {
        return Err(format!("event log last state hash {:?} does not match bridge state {state_hash}", event_summary.last_state_hash));
    }
    if state_hash != expected_final_state_hash {
        return Err(format!("bridge final state hash {state_hash} does not match expected hash {expected_final_state_hash}"));
    }
    if state_bytes_hex != expected_final_state_hex {
        return Err(format!("bridge final state bytes {state_bytes_hex} do not match expected {expected_final_state_hex}"));
    }
    if cli.probe_corrupt_resume {
        verify_corrupt_resume_rejected(bridge, cli, state_file, event_log)?;
    }
    if cli.probe_cli_resume {
        verify_cli_resume(bridge, cli, &state_canonical_outpoint, &state_hash, &state_bytes_hex)?;
    }
    if cli.probe_live_preflight {
        verify_live_preflight_rejected(bridge, cli)?;
    }

    println!("mode=doom-tn10-smoke");
    println!("url={}", cli.url);
    println!("bridge_bin={}", bridge.display());
    println!("wallet_address={}", cli.wallet_address);
    println!("ready_start_mode={ready_start_mode}");
    println!("ready_start_available={}", bool_field(&ready, "startAvailable")?);
    println!("ready_submit={}", bool_field(&ready, "submit")?);
    println!("ready_wallet_key_available={}", bool_field(&ready, "walletKeyAvailable")?);
    if let Some(matches) = ready.get("walletKeyMatches").and_then(Value::as_bool) {
        println!("ready_wallet_key_matches={matches}");
    }
    println!("start_synthetic={synthetic}");
    println!("start_outpoint={start_outpoint}");
    println!("prestart_state_started=false");
    println!("poststart_state_started=true");
    println!("state_file={state_file}");
    println!("event_log={event_log}");
    println!("artifacts_kept={}", cli.keep_artifacts);
    println!("ticks_requested={}", cli.ticks);
    println!("ticks_accepted={}", cli.ticks);
    println!("captured_tps={:.3}", cli.captured_tps);
    println!("event_log_events={}", event_summary.events);
    println!("event_log_accepted={}", event_summary.accepted_events);
    println!("event_log_rejected={}", event_summary.rejected_events);
    println!("event_log_canonical_tick_mismatch={}", event_summary.canonical_tick_mismatch_rejections);
    println!("event_log_invalid_state_snapshot={}", event_summary.invalid_state_snapshot_rejections);
    println!("event_log_session_not_started={}", event_summary.session_not_started_rejections);
    println!("event_log_wallet_mismatch={}", event_summary.wallet_mismatch_rejections);
    println!("event_log_last_canonical_tick={}", event_summary.last_canonical_tick.unwrap_or(0));
    println!("event_log_last_successor_outpoint={}", event_summary.last_successor_outpoint.as_deref().unwrap_or(""));
    println!("event_log_last_state_hash={}", event_summary.last_state_hash.as_deref().unwrap_or(""));
    println!("event_log_ticcmds_verified={}", event_summary.ticcmds_verified);
    println!("event_log_state_hashes_verified={}", event_summary.state_hashes_verified);
    println!("state_bytes_verified=true");
    println!("state_bytes_per_tick={state_bytes_per_tick}");
    println!("target_tps={:.3}", cli.target_tps);
    println!("target_state_bytes_per_second={target_state_bytes_per_second:.3}");
    println!("captured_state_bytes_per_second={captured_state_bytes_per_second:.3}");
    println!("prestart_rejection_verified=true");
    println!("wallet_mismatch_probe_performed={}", wallet_mismatch_probe.is_some());
    println!("bad_tick_probe_performed={}", bad_tick_probe.is_some());
    if let Some(probe) = bad_tick_probe.as_ref() {
        println!("bad_tick_probe_tick={}", probe.skipped_tick);
        println!("bad_tick_probe_canonical_tick={}", probe.canonical_tick);
        println!("bad_tick_probe_canonical_outpoint={}", probe.canonical_outpoint);
    }
    println!("bad_state_probe_performed={}", bad_state_probe.is_some());
    if let Some(probe) = bad_state_probe.as_ref() {
        println!("bad_state_probe_canonical_tick={}", probe.canonical_tick);
        println!("bad_state_probe_canonical_outpoint={}", probe.canonical_outpoint);
    }
    println!("bad_state_ticcmd_probe_performed={}", bad_state_ticcmd_probe.is_some());
    if let Some(probe) = bad_state_ticcmd_probe.as_ref() {
        println!("bad_state_ticcmd_probe_canonical_tick={}", probe.canonical_tick);
        println!("bad_state_ticcmd_probe_canonical_outpoint={}", probe.canonical_outpoint);
    }
    if let Some(restart_after_tick) = cli.restart_after_tick {
        println!("restart_after_tick={restart_after_tick}");
    }
    println!("corrupt_resume_probe_performed={}", cli.probe_corrupt_resume);
    println!("cli_resume_probe_performed={}", cli.probe_cli_resume);
    println!("live_preflight_probe_performed={}", cli.probe_live_preflight);
    println!("restart_performed={}", restart_state.is_some());
    if let Some(restart_state) = restart_state.as_ref() {
        println!("restart_loaded_tick={}", u64_field(restart_state, "canonicalTick")?);
        println!("restart_loaded_started={}", bool_field(restart_state, "started")?);
        println!("restart_loaded_outpoint={}", string_field(restart_state, "canonicalOutpoint")?);
        println!("restart_loaded_state_hash={}", string_field(restart_state, "stateHash")?);
        println!("restart_loaded_state_bytes_hex={}", string_field(restart_state, "stateBytesHex")?);
    }
    println!("smoke_elapsed_ms={smoke_elapsed_ms:.3}");
    println!("smoke_tps={smoke_tps:.3}");
    println!("smoke_vs_target={smoke_vs_target:.3}");
    println!("smoke_state_bytes_per_second={smoke_state_bytes_per_second:.3}");
    if let Some(min_smoke_tps) = cli.min_smoke_tps {
        println!("min_smoke_tps={min_smoke_tps:.3}");
    }
    if let Some(require_smoke_vs_target) = cli.require_smoke_vs_target {
        println!("required_smoke_vs_target={require_smoke_vs_target:.3}");
    }
    println!("last_tic_canonical_tick={}", u64_field(&last_tic, "canonicalTick")?);
    println!("last_tic_successor_outpoint={}", string_field(&last_tic, "successorOutpoint")?);
    println!("last_tic_state_hash={}", string_field(&last_tic, "stateHash")?);
    println!("expected_final_state_hash={expected_final_state_hash}");
    println!("state_canonical_tick={}", u64_field(&state, "canonicalTick")?);
    println!("state_started={}", bool_field(&state, "started")?);
    println!("state_canonical_outpoint={state_canonical_outpoint}");
    println!("state_hash={state_hash}");
    println!("state_bytes_hex={state_bytes_hex}");
    println!("state_hash_verified=true");
    println!("smoke_ok=true");
    Ok(())
}

#[derive(Debug)]
struct EventSummary {
    events: usize,
    accepted_events: usize,
    rejected_events: usize,
    last_canonical_tick: Option<u32>,
    last_successor_outpoint: Option<String>,
    last_state_hash: Option<String>,
    ticcmds_verified: bool,
    state_hashes_verified: bool,
    canonical_tick_mismatch_rejections: usize,
    invalid_state_snapshot_rejections: usize,
    session_not_started_rejections: usize,
    wallet_mismatch_rejections: usize,
}

#[derive(Debug)]
struct BadTickProbe {
    skipped_tick: u32,
    canonical_tick: u32,
    canonical_outpoint: String,
}

#[derive(Debug)]
struct BadStateProbe {
    canonical_tick: u32,
    canonical_outpoint: String,
}

fn summarize_event_log(path: &str, expected_ticks: u32) -> Result<EventSummary, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("failed to read bridge event log {path}: {err}"))?;
    let mut summary = EventSummary {
        events: 0,
        accepted_events: 0,
        rejected_events: 0,
        last_canonical_tick: None,
        last_successor_outpoint: None,
        last_state_hash: None,
        ticcmds_verified: true,
        state_hashes_verified: true,
        canonical_tick_mismatch_rejections: 0,
        invalid_state_snapshot_rejections: 0,
        session_not_started_rejections: 0,
        wallet_mismatch_rejections: 0,
    };
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event = serde_json::from_str::<Value>(line).map_err(|err| format!("failed to parse {path} line {}: {err}", index + 1))?;
        summary.events += 1;
        let status = event.get("status").and_then(Value::as_str).unwrap_or("accepted");
        match status {
            "accepted" => {
                summary.accepted_events += 1;
                let browser_tick = u32_field(&event, "browserTick", path, index)?;
                let canonical_tick = u32_field(&event, "canonicalTick", path, index)?;
                if browser_tick != canonical_tick {
                    return Err(format!(
                        "{path} line {} browser tick {browser_tick} did not match canonical tick {canonical_tick}",
                        index + 1
                    ));
                }
                if browser_tick == 0 || browser_tick > expected_ticks {
                    return Err(format!(
                        "{path} line {} accepted unexpected tick {browser_tick}; expected 1..={expected_ticks}",
                        index + 1
                    ));
                }
                let expected_ticcmd = bytes_to_hex(&ticcmd_bytes(browser_tick));
                let actual_ticcmd = string_json_field(&event, "ticcmdHex", path, index)?;
                if actual_ticcmd != expected_ticcmd {
                    return Err(format!("{path} line {} ticcmd {actual_ticcmd} did not match expected {expected_ticcmd}", index + 1));
                }
                let expected_state_hash = state_hash_hex(&state_bytes(browser_tick));
                let actual_state_hash = string_json_field(&event, "stateHash", path, index)?;
                if actual_state_hash != expected_state_hash {
                    return Err(format!(
                        "{path} line {} state hash {actual_state_hash} did not match expected {expected_state_hash}",
                        index + 1
                    ));
                }
                let expected_state_bytes_hex = bytes_to_hex(&state_bytes(browser_tick));
                let actual_state_bytes_hex = string_json_field(&event, "stateBytesHex", path, index)?;
                if actual_state_bytes_hex != expected_state_bytes_hex {
                    return Err(format!(
                        "{path} line {} stateBytesHex {actual_state_bytes_hex} did not match expected {expected_state_bytes_hex}",
                        index + 1
                    ));
                }
                summary.last_canonical_tick = Some(canonical_tick);
                summary.last_successor_outpoint = event.get("successorOutpoint").and_then(Value::as_str).map(str::to_string);
                summary.last_state_hash = event.get("stateHash").and_then(Value::as_str).map(str::to_string);
            }
            "rejected" => {
                summary.rejected_events += 1;
                if event.get("rejectionClass").and_then(Value::as_str) == Some("canonical_tick_mismatch") {
                    summary.canonical_tick_mismatch_rejections += 1;
                }
                if event.get("rejectionClass").and_then(Value::as_str) == Some("invalid_state_snapshot") {
                    summary.invalid_state_snapshot_rejections += 1;
                }
                if event.get("rejectionClass").and_then(Value::as_str) == Some("session_not_started") {
                    summary.session_not_started_rejections += 1;
                }
                if event.get("rejectionClass").and_then(Value::as_str) == Some("wallet_mismatch") {
                    summary.wallet_mismatch_rejections += 1;
                }
            }
            other => return Err(format!("unexpected event status {other:?} in {path} line {}", index + 1)),
        }
    }
    Ok(summary)
}

fn string_json_field<'a>(value: &'a Value, name: &str, path: &str, index: usize) -> Result<&'a str, String> {
    value.get(name).and_then(Value::as_str).ok_or_else(|| format!("missing string field {name} in {path} line {}", index + 1))
}

fn u32_field(value: &Value, name: &str, path: &str, index: usize) -> Result<u32, String> {
    value
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|tick| u32::try_from(tick).ok())
        .ok_or_else(|| format!("missing u32 field {name} in {path} line {}", index + 1))
}

fn spawn_bridge(bridge: &PathBuf, cli: &Cli, state_file: &str, event_log: &str) -> Result<Child, String> {
    Command::new(bridge)
        .args([
            "--listen",
            &cli.listen,
            "--url",
            &cli.url,
            "--submit",
            "false",
            "--submit-backend",
            &cli.submit_backend,
            "--state-file",
            state_file,
            "--event-log",
            event_log,
            "--ready-timeout-ms",
            "10000",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("failed to launch {}: {err}", bridge.display()))
}

fn verify_corrupt_resume_rejected(bridge: &PathBuf, cli: &Cli, state_file: &str, event_log: &str) -> Result<(), String> {
    let corrupt_state_file = format!("{state_file}.corrupt");
    let raw = std::fs::read_to_string(state_file).map_err(|err| format!("failed to read smoke state file {state_file}: {err}"))?;
    let mut value =
        serde_json::from_str::<Value>(&raw).map_err(|err| format!("failed to parse smoke state file {state_file}: {err}"))?;
    let state_hex = value
        .get("prev_state_hex")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{state_file} did not contain prev_state_hex for corrupt resume probe"))?
        .to_string();
    let mut corrupted = state_hex;
    let replacement = if corrupted.ends_with('0') { '1' } else { '0' };
    corrupted.pop();
    corrupted.push(replacement);
    value["prev_state_hex"] = Value::String(corrupted);
    let body = serde_json::to_string_pretty(&value).map_err(|err| format!("failed to encode corrupt state file: {err}"))?;
    std::fs::write(&corrupt_state_file, format!("{body}\n"))
        .map_err(|err| format!("failed to write corrupt state file {corrupt_state_file}: {err}"))?;

    let mut child = Command::new(bridge)
        .args([
            "--listen",
            "127.0.0.1:0",
            "--url",
            &cli.url,
            "--submit",
            "false",
            "--submit-backend",
            &cli.submit_backend,
            "--state-file",
            &corrupt_state_file,
            "--event-log",
            event_log,
            "--ready-timeout-ms",
            "10000",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to launch corrupt resume probe {}: {err}", bridge.display()))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if child.try_wait().map_err(|err| format!("failed to poll corrupt resume bridge child: {err}"))?.is_some() {
            let output = child.wait_with_output().map_err(|err| format!("failed to read corrupt resume bridge output: {err}"))?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = std::fs::remove_file(&corrupt_state_file);
            if output.status.success() {
                return Err(format!("corrupt resume bridge unexpectedly started successfully: {stdout} {stderr}"));
            }
            let combined = format!("{stdout}\n{stderr}");
            if !combined.contains("invalid bridge state file") {
                return Err(format!("corrupt resume bridge failed for unexpected reason: {combined}"));
            }
            return Ok(());
        }
        sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&corrupt_state_file);
    Err("corrupt resume bridge did not exit within 2s".to_string())
}

fn verify_cli_resume(
    bridge: &PathBuf,
    cli: &Cli,
    expected_outpoint: &str,
    expected_state_hash: &str,
    expected_state_hex: &str,
) -> Result<(), String> {
    let (resume_txid, resume_index) = expected_outpoint
        .split_once(':')
        .ok_or_else(|| format!("invalid expected outpoint for cli resume probe: {expected_outpoint}"))?;
    let listen = alternate_listen(&cli.listen)?;
    let state_file = format!(".doom-tn10-smoke-{}-cli-resume.state.json", std::process::id());
    let mut child = Command::new(bridge)
        .args([
            "--listen",
            &listen,
            "--url",
            &cli.url,
            "--submit",
            "false",
            "--submit-backend",
            &cli.submit_backend,
            "--state-file",
            &state_file,
            "--ready-timeout-ms",
            "10000",
            "--input-txid",
            resume_txid,
            "--input-index",
            resume_index,
            "--wallet-address",
            &cli.wallet_address,
            "--prev-tick",
            &cli.ticks.to_string(),
            "--prev-ticcmd-hex",
            &bytes_to_hex(&ticcmd_bytes(cli.ticks)),
            "--prev-state-hash-hex",
            expected_state_hash,
            "--prev-state-hex",
            expected_state_hex,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to launch cli resume probe {}: {err}", bridge.display()))?;
    if let Err(err) = wait_for_bridge(&listen, Instant::now(), Duration::from_secs(2)) {
        let _ = child.kill();
        let output = child.wait_with_output().map_err(|err| format!("failed to read failed cli resume bridge output: {err}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = std::fs::remove_file(&state_file);
        return Err(format!("cli resume bridge did not start from explicit tuple: {err}: {stdout} {stderr}"));
    }
    let state = get_json(&listen, "/state")?;
    if u64_field(&state, "canonicalTick")? != cli.ticks as u64 {
        return Err(format!("cli resume bridge loaded unexpected state: {state}"));
    }
    if string_field(&state, "canonicalOutpoint")? != expected_outpoint {
        return Err(format!("cli resume outpoint mismatch: {state}"));
    }
    if string_field(&state, "stateHash")? != expected_state_hash {
        return Err(format!("cli resume state hash mismatch: {state}"));
    }
    if string_field(&state, "stateBytesHex")? != expected_state_hex {
        return Err(format!("cli resume state bytes mismatch: {state}"));
    }
    if string_field(&state, "walletAddress")? != cli.wallet_address {
        return Err(format!("cli resume wallet mismatch: {state}"));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&state_file);
    Ok(())
}

fn verify_live_preflight_rejected(bridge: &PathBuf, cli: &Cli) -> Result<(), String> {
    let listen = alternate_listen_with_offset(&cli.listen, 200)?;
    let pid = std::process::id();
    let state_file = format!(".doom-tn10-smoke-{pid}-live-preflight.state.json");
    let event_log = format!(".doom-tn10-smoke-{pid}-live-preflight.events.jsonl");
    let mut child = Command::new(bridge)
        .args([
            "--listen",
            &listen,
            "--url",
            &cli.url,
            "--submit",
            "true",
            "--submit-backend",
            "in-process",
            "--input-txid",
            DEFAULT_INPUT_TXID,
            "--input-index",
            "0",
            "--utxo-value",
            "100000000",
            "--covenant-id",
            DEFAULT_COVENANT_ID,
            "--prev-tick",
            "0",
            "--preflight-timeout-ms",
            "1",
            "--mempool-timeout-ms",
            "1",
            "--state-file",
            &state_file,
            "--event-log",
            &event_log,
            "--ready-timeout-ms",
            "10000",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to launch live preflight bridge {}: {err}", bridge.display()))?;
    if let Err(err) = wait_for_bridge(&listen, Instant::now(), Duration::from_secs(3)) {
        let _ = child.kill();
        let output = child.wait_with_output().map_err(|err| format!("failed to read failed live preflight bridge output: {err}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        cleanup_live_probe_files(&state_file, &event_log);
        return Err(format!("live preflight bridge did not start: {err}: {stdout} {stderr}"));
    }
    let rejected = post_json_expect_error(
        &listen,
        "/tic",
        &serde_json::json!({
            "tick": 1,
            "walletAddress": cli.wallet_address,
            "canonicalOutpoint": format!("{DEFAULT_INPUT_TXID}:0"),
            "ticcmd": ticcmd_bytes(1),
            "stateBytes": state_bytes(1),
            "capturedAt": captured_at(1, cli.captured_tps)?,
        }),
    )?;
    let error = string_field(&rejected, "error")?;
    if !error.contains("preflight input") {
        let _ = child.kill();
        let _ = child.wait();
        cleanup_live_probe_files(&state_file, &event_log);
        return Err(format!("live preflight probe returned unexpected error: {rejected}"));
    }
    let text =
        std::fs::read_to_string(&event_log).map_err(|err| format!("failed to read live preflight event log {event_log}: {err}"))?;
    let event =
        text.lines().find(|line| !line.trim().is_empty()).ok_or_else(|| format!("live preflight event log {event_log} was empty"))?;
    let event = serde_json::from_str::<Value>(event).map_err(|err| format!("failed to parse live preflight event: {err}"))?;
    if event.get("status").and_then(Value::as_str) != Some("rejected") {
        return Err(format!("live preflight event was not rejected: {event}"));
    }
    if event.get("rejectionClass").and_then(Value::as_str) != Some("preflight_input_missing") {
        return Err(format!("live preflight rejection class mismatch: {event}"));
    }
    let child_output = event.get("childOutput").and_then(Value::as_array).ok_or_else(|| format!("missing childOutput: {event}"))?;
    let contains_line = |needle: &str| child_output.iter().filter_map(Value::as_str).any(|line| line.contains(needle));
    if !contains_line("mode=bridge-in-process-live")
        || !contains_line("local_validation=ok")
        || !contains_line("server_version=1.2.0-toc")
        || !contains_line("preflight_input_visible=false")
    {
        return Err(format!("live preflight childOutput missing expected direct-backend evidence: {event}"));
    }
    let _ = child.kill();
    let _ = child.wait();
    cleanup_live_probe_files(&state_file, &event_log);
    Ok(())
}

fn cleanup_live_probe_files(state_file: &str, event_log: &str) {
    let _ = std::fs::remove_file(state_file);
    let _ = std::fs::remove_file(event_log);
}

fn alternate_listen(listen: &str) -> Result<String, String> {
    alternate_listen_with_offset(listen, 100)
}

fn alternate_listen_with_offset(listen: &str, offset: u16) -> Result<String, String> {
    let (host, port) = listen.rsplit_once(':').ok_or_else(|| format!("invalid listen address {listen}"))?;
    let port = port.parse::<u16>().map_err(|err| format!("invalid listen port in {listen}: {err}"))?;
    let alternate = port.checked_add(offset).ok_or_else(|| format!("listen port {port} too high for alternate probe port"))?;
    Ok(format!("{host}:{alternate}"))
}

fn remaining_timeout(started: Instant, timeout: Duration) -> Result<Duration, String> {
    timeout.checked_sub(started.elapsed()).ok_or_else(|| format!("smoke timeout expired after {} ms", timeout.as_millis()))
}

fn ticcmd_bytes(tick: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; 8];
    bytes[0..4].copy_from_slice(&tick.to_le_bytes());
    bytes[4] = tick.wrapping_mul(3) as u8;
    bytes[5] = tick.wrapping_mul(5) as u8;
    bytes[6] = tick.wrapping_mul(7) as u8;
    bytes[7] = 0x80 | (tick as u8 & 0x0f);
    bytes
}

fn state_bytes(tick: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; 96];
    bytes[0..4].copy_from_slice(b"KDS4");
    bytes[4..8].copy_from_slice(&tick.to_le_bytes());
    bytes[8..12].copy_from_slice(&(tick * 35).to_le_bytes());
    bytes[12] = tick.wrapping_mul(11) as u8;
    bytes[13] = tick.wrapping_mul(13) as u8;
    bytes[14] = 0;
    bytes[15] = 1;
    bytes[16..20].copy_from_slice(&(tick + 3).to_le_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
    bytes[24..28].copy_from_slice(&(tick + 1).to_le_bytes());
    for (index, byte) in bytes[28..60].iter_mut().enumerate() {
        *byte = tick.wrapping_mul(17).wrapping_add(index as u32) as u8;
    }
    bytes[60..64].copy_from_slice(&(tick + 2).to_le_bytes());
    bytes[64..68].copy_from_slice(&(tick + 10).to_le_bytes());
    bytes[68..72].copy_from_slice(&(tick + 20).to_le_bytes());
    bytes[72..76].copy_from_slice(&(tick + 30).to_le_bytes());
    bytes[76..80].copy_from_slice(&(tick + 40).to_le_bytes());
    bytes[80..84].copy_from_slice(&(tick + 50).to_le_bytes());
    bytes[84..88].copy_from_slice(&(tick + 60).to_le_bytes());
    bytes[88..96].copy_from_slice(&ticcmd_bytes(tick));
    bytes
}

fn legacy_state_bytes(tick: u32) -> Vec<u8> {
    let mut bytes = state_bytes(tick);
    bytes[3] = b'3';
    bytes
}

fn mismatched_ticcmd_state_bytes(tick: u32) -> Vec<u8> {
    let mut bytes = state_bytes(tick);
    bytes[88] ^= 0x80;
    bytes
}

fn state_hash_hex(bytes: &[u8]) -> String {
    bytes_to_hex(Blake2bParams::new().hash_length(32).hash(bytes).as_bytes())
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn captured_at(tick: u32, captured_tps: f64) -> Result<String, String> {
    if tick == 0 {
        return Err("captured timestamp tick must be greater than zero".to_string());
    }
    let elapsed_ms = ((tick - 1) as f64 * 1_000.0 / captured_tps).round();
    if !elapsed_ms.is_finite() || elapsed_ms < 0.0 || elapsed_ms > u32::MAX as f64 {
        return Err(format!("captured timestamp overflow for tick {tick} at {captured_tps} TPS"));
    }
    let total_ms = elapsed_ms as u32;
    let millis = total_ms % 1_000;
    let total_seconds = total_ms / 1_000;
    let seconds = total_seconds % 60;
    let total_minutes = (12 * 60) + 45 + (total_seconds / 60);
    let minutes = total_minutes % 60;
    let hours = total_minutes / 60;
    if hours >= 24 {
        return Err(format!("captured timestamp exceeded one-day smoke window at tick {tick}"));
    }
    Ok(format!("2026-05-23T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z"))
}

fn sibling_bin(name: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|err| format!("failed to locate current executable: {err}"))?;
    let dir = exe.parent().ok_or_else(|| format!("failed to locate executable directory for {}", exe.display()))?;
    Ok(dir.join(name))
}

fn validate_bridge_bin(bridge: &Path, allow_stale_bridge_bin: bool) -> Result<(), String> {
    let bridge_meta = std::fs::metadata(bridge).map_err(|err| format!("failed to stat bridge binary {}: {err}", bridge.display()))?;
    if !bridge_meta.is_file() {
        return Err(format!("bridge binary {} is not a file", bridge.display()));
    }
    if allow_stale_bridge_bin {
        return Ok(());
    }
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bin/doom_tn10_bridge.rs");
    let source_meta = std::fs::metadata(&source).map_err(|err| format!("failed to stat bridge source {}: {err}", source.display()))?;
    let bridge_modified =
        bridge_meta.modified().map_err(|err| format!("failed to read modified time for {}: {err}", bridge.display()))?;
    let source_modified =
        source_meta.modified().map_err(|err| format!("failed to read modified time for {}: {err}", source.display()))?;
    if bridge_modified < source_modified {
        return Err(format!(
            "bridge binary {} is older than {}; rebuild doom_tn10_bridge or pass --allow-stale-bridge-bin",
            bridge.display(),
            source.display()
        ));
    }
    Ok(())
}

fn wait_for_bridge(listen: &str, started: Instant, timeout: Duration) -> Result<(), String> {
    while started.elapsed() < timeout {
        if TcpStream::connect(listen).is_ok() {
            return Ok(());
        }
        sleep(Duration::from_millis(50));
    }
    Err(format!("bridge did not accept TCP connections at {listen} within {} ms", timeout.as_millis()))
}

fn post_json(listen: &str, path: &str, body: &Value) -> Result<Value, String> {
    request_json(listen, "POST", path, Some(&body.to_string()))
}

fn post_json_expect_error(listen: &str, path: &str, body: &Value) -> Result<Value, String> {
    let (status, value) = request_json_with_status(listen, "POST", path, Some(&body.to_string()))?;
    if (200..300).contains(&status) {
        return Err(format!("bridge request POST {path} unexpectedly succeeded: {value}"));
    }
    Ok(value)
}

fn get_json(listen: &str, path: &str) -> Result<Value, String> {
    request_json(listen, "GET", path, None)
}

fn url_encode_query_value(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => encoded.push(byte as char),
            _ => {
                use std::fmt::Write;
                write!(&mut encoded, "%{byte:02X}").expect("writing to String cannot fail");
            }
        }
    }
    encoded
}

fn request_json(listen: &str, method: &str, path: &str, body: Option<&str>) -> Result<Value, String> {
    let (status_code, value) = request_json_with_status(listen, method, path, body)?;
    if !(200..300).contains(&status_code) {
        return Err(format!("bridge request {method} {path} failed with HTTP {status_code}: {value}"));
    }
    Ok(value)
}

fn request_json_with_status(listen: &str, method: &str, path: &str, body: Option<&str>) -> Result<(u16, Value), String> {
    let mut stream = TcpStream::connect(listen).map_err(|err| format!("failed to connect to bridge at {listen}: {err}"))?;
    let body = body.unwrap_or("");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {listen}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).map_err(|err| format!("failed to write HTTP request: {err}"))?;
    let mut response = String::new();
    stream.read_to_string(&mut response).map_err(|err| format!("failed to read HTTP response: {err}"))?;
    let (headers, body) = response.split_once("\r\n\r\n").ok_or_else(|| format!("invalid HTTP response: {response}"))?;
    let status = headers.lines().next().unwrap_or("");
    let status_code = status
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("invalid HTTP status line for {method} {path}: {status}"))?
        .parse::<u16>()
        .map_err(|err| format!("invalid HTTP status code for {method} {path}: {status}: {err}"))?;
    let value = serde_json::from_str(body).map_err(|err| format!("failed to parse bridge JSON for {method} {path}: {err}: {body}"))?;
    Ok((status_code, value))
}

fn string_field(value: &Value, name: &str) -> Result<String, String> {
    value.get(name).and_then(Value::as_str).map(str::to_string).ok_or_else(|| format!("missing string field {name}: {value}"))
}

fn bool_field(value: &Value, name: &str) -> Result<bool, String> {
    value.get(name).and_then(Value::as_bool).ok_or_else(|| format!("missing bool field {name}: {value}"))
}

fn u64_field(value: &Value, name: &str) -> Result<u64, String> {
    value.get(name).and_then(Value::as_u64).ok_or_else(|| format!("missing u64 field {name}: {value}"))
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}
