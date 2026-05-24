use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use blake2b_simd::Params as Blake2bParams;
use chrono::DateTime;
use clap::Parser;
use serde::{Deserialize, Serialize};

const KDS4_STATE_LEN: usize = 96;
const KDS4_TICCMD_OFFSET: usize = 88;

#[derive(Debug, Parser)]
#[command(
    name = "doom-tn10-report",
    about = "Summarize Doom TN10 bridge event logs for cadence and resume evidence",
    next_line_help = true
)]
struct Cli {
    /// Bridge JSONL event log produced by doom_tn10_bridge.
    #[arg(long = "event-log", default_value = ".doom-tn10-bridge-events.jsonl")]
    event_log: PathBuf,

    /// Target authoritative tic rate to compare against.
    #[arg(long = "target-tps", default_value_t = 10.0)]
    target_tps: f64,

    /// Write a doom_tn10_bridge-compatible state file from the latest accepted event.
    #[arg(long = "write-bridge-state")]
    write_bridge_state: Option<PathBuf>,

    /// Emit a machine-readable JSON summary after validation.
    #[arg(long = "emit-json", default_value_t = false)]
    emit_json: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeEvent {
    #[serde(default = "accepted_status")]
    status: String,
    browser_tick: u32,
    canonical_tick: Option<u32>,
    tx_id: Option<String>,
    successor_outpoint: Option<String>,
    fee_sompi: Option<u64>,
    successor_utxo_value_sompi: Option<u64>,
    ticcmd_hex: String,
    state_hash: Option<String>,
    state_bytes_hex: Option<String>,
    checkpoint_manifest_root_hex: Option<String>,
    checkpoint_state_bytes: Option<usize>,
    checkpoint_chunk_count: Option<usize>,
    covenant_id: Option<String>,
    current_utxo_value_sompi: Option<u64>,
    submit_elapsed_ms: f64,
    captured_at: Option<String>,
    wallet_address: String,
    canonical_outpoint_before: Option<String>,
    rejection_class: Option<String>,
    rpc_submit: Option<String>,
    mempool_seen: Option<bool>,
    mempool_is_orphan: Option<bool>,
    mempool_seen_elapsed_ms: Option<f64>,
    inclusion_seen: Option<bool>,
    inclusion_seen_elapsed_ms: Option<f64>,
    accepting_block_hash: Option<String>,
}

#[derive(Debug, Serialize)]
struct BridgeStateFile {
    input_txid: String,
    input_index: u32,
    utxo_value: Option<u64>,
    prev_tick: u32,
    prev_ticcmd_hex: Option<String>,
    prev_state_hash_hex: Option<String>,
    prev_state_hex: Option<String>,
    covenant_id: Option<String>,
    wallet_address: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReportSummary {
    mode: &'static str,
    event_log: String,
    target_tps: f64,
    events: usize,
    accepted_events: usize,
    rejected_events: usize,
    unique_txids: usize,
    duplicate_txids: usize,
    unique_wallets: usize,
    rpc_submit_ok: usize,
    rpc_submit_skipped: usize,
    mempool_seen_count: usize,
    mempool_orphan_count: usize,
    inclusion_seen_count: usize,
    first_browser_tick: u32,
    last_browser_tick: u32,
    first_canonical_tick: u32,
    last_canonical_tick: u32,
    canonical_non_monotonic_steps: usize,
    browser_non_monotonic_steps: usize,
    accepted_txid_verified_count: usize,
    accepted_outpoint_link_verified_count: usize,
    first_successor_outpoint: String,
    last_successor_outpoint: String,
    last_ticcmd_hex: String,
    last_state_hash: String,
    last_state_bytes_hex: String,
    state_bytes_present_count: usize,
    latest_state_bytes_len: usize,
    state_bytes_min_len: usize,
    state_bytes_avg_len: f64,
    state_bytes_max_len: usize,
    target_state_bytes_per_second: f64,
    accepted_state_bytes_per_second: Option<f64>,
    latest_kds4: Option<Kds4Metadata>,
    state_hash_verified_count: usize,
    state_snapshot_verified_count: usize,
    checkpoint_count: usize,
    checkpoint_verified_count: usize,
    latest_checkpoint_manifest_root_hex: Option<String>,
    latest_checkpoint_state_bytes: Option<usize>,
    latest_checkpoint_chunk_count: Option<usize>,
    latest_current_utxo_value_sompi: Option<u64>,
    latest_successor_utxo_value_sompi: Option<u64>,
    latest_fee_sompi: Option<u64>,
    submit_elapsed_avg_ms: f64,
    submit_elapsed_p50_ms: f64,
    submit_elapsed_p95_ms: f64,
    submit_elapsed_max_ms: f64,
    mempool_seen_elapsed_avg_ms: Option<f64>,
    mempool_seen_elapsed_p95_ms: Option<f64>,
    inclusion_seen_elapsed_avg_ms: Option<f64>,
    inclusion_seen_elapsed_p95_ms: Option<f64>,
    captured_duration_ms: Option<f64>,
    accepted_tps: Option<f64>,
    accepted_vs_target: Option<f64>,
    rejection_counts: std::collections::BTreeMap<String, usize>,
    resume_tuple_prev_tick: u32,
    resume_tuple_input_outpoint: String,
    resume_tuple_prev_ticcmd_hex: String,
    resume_tuple_prev_state_hash_hex: String,
    resume_tuple_prev_state_bytes_hex: String,
    resume_tuple_covenant_id: String,
    bridge_resume_command: Option<String>,
    latest_accepting_block_hash: Option<String>,
    bridge_state_written: bool,
    bridge_state_file: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Kds4Metadata {
    tick: u32,
    level_time: u32,
    prnd_index: u8,
    rnd_index: u8,
    active_player: u8,
    player_mask: u8,
    max_players: u32,
    live_players: u32,
    mobj_count: u32,
    player_hash_hex: String,
    mobj_hash_hex: String,
    world_hash_hex: String,
    special_hash_hex: String,
    special_count: u32,
    sector_count: u32,
    line_count: u32,
    side_count: u32,
    total_kills: u32,
    total_items: u32,
    total_secrets: u32,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    if cli.target_tps <= 0.0 {
        return Err("--target-tps must be greater than zero".to_string());
    }
    let events = read_events(&cli.event_log)?;
    if events.is_empty() {
        return Err(format!("{} contains no bridge events", cli.event_log.display()));
    }

    let accepted_events = events.iter().filter(|event| event.status == "accepted").collect::<Vec<_>>();
    let rejected_events = events.iter().filter(|event| event.status == "rejected").collect::<Vec<_>>();

    let mut submit_elapsed = accepted_events.iter().map(|event| event.submit_elapsed_ms).collect::<Vec<_>>();
    submit_elapsed.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let unique_txids = accepted_events.iter().filter_map(|event| event.tx_id.as_deref()).collect::<HashSet<_>>().len();
    let unique_wallets = events.iter().map(|event| event.wallet_address.as_str()).collect::<HashSet<_>>().len();
    let duplicate_txids = accepted_events.len().saturating_sub(unique_txids);
    let canonical_gaps = count_non_monotonic_steps(&accepted_events, |event| event.canonical_tick.unwrap_or(0));
    let browser_gaps = count_non_monotonic_steps(&accepted_events, |event| event.browser_tick);
    let (accepted_txid_verified, accepted_outpoint_links_verified) = verify_accepted_chain_links(&accepted_events)?;
    let captured_duration_ms = captured_duration_ms(&accepted_events);
    let accepted_tps = captured_duration_ms.and_then(|duration_ms| {
        if duration_ms > 0.0 { Some((accepted_events.len().saturating_sub(1) as f64) / (duration_ms / 1_000.0)) } else { None }
    });
    let latest_accepted = accepted_events.last().copied();
    let state_bytes_present = accepted_events.iter().filter(|event| event.state_bytes_hex.is_some()).count();
    let state_byte_lengths = state_byte_lengths(&accepted_events)?;
    let latest_state_bytes_len = latest_accepted.and_then(|event| event.state_bytes_hex.as_deref()).map(hex_byte_len).unwrap_or(0);
    let state_bytes_min_len = state_byte_lengths.iter().copied().min().unwrap_or(0);
    let state_bytes_max_len = state_byte_lengths.iter().copied().max().unwrap_or(0);
    let state_bytes_avg_len = if state_byte_lengths.is_empty() {
        0.0
    } else {
        state_byte_lengths.iter().sum::<usize>() as f64 / state_byte_lengths.len() as f64
    };
    let (state_hash_verified, state_snapshot_verified) =
        accepted_events.iter().filter(|event| event.state_bytes_hex.is_some()).try_fold((0usize, 0usize), |counts, event| {
            verify_event_state_bytes(event)?;
            Ok::<(usize, usize), String>((counts.0 + 1, counts.1 + 1))
        })?;
    let checkpoint_count = accepted_events.iter().filter(|event| event.checkpoint_manifest_root_hex.is_some()).count();
    let checkpoint_verified_count = accepted_events.iter().try_fold(0usize, |count, event| {
        if event.checkpoint_manifest_root_hex.is_some() {
            verify_event_checkpoint(event)?;
            Ok::<usize, String>(count + 1)
        } else {
            Ok(count)
        }
    })?;
    let latest_kds4 = latest_accepted
        .and_then(|event| event.state_bytes_hex.as_deref())
        .map(|hex| parse_hex(hex, "latest stateBytesHex").and_then(|bytes| decode_kds4_metadata(&bytes)))
        .transpose()?;
    let rpc_ok = accepted_events.iter().filter(|event| event.rpc_submit.as_deref() == Some("ok")).count();
    let rpc_skipped = accepted_events.iter().filter(|event| event.rpc_submit.as_deref() == Some("skipped")).count();
    let mempool_seen = accepted_events.iter().filter(|event| event.mempool_seen == Some(true)).count();
    let mempool_orphans = accepted_events.iter().filter(|event| event.mempool_is_orphan == Some(true)).count();
    let inclusion_seen = accepted_events.iter().filter(|event| event.inclusion_seen == Some(true)).count();
    let mut mempool_elapsed = accepted_events.iter().filter_map(|event| event.mempool_seen_elapsed_ms).collect::<Vec<_>>();
    mempool_elapsed.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut inclusion_elapsed = accepted_events.iter().filter_map(|event| event.inclusion_seen_elapsed_ms).collect::<Vec<_>>();
    inclusion_elapsed.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let bridge_resume_command = bridge_resume_command(latest_accepted);
    let mut bridge_state_written = false;
    let mut written_bridge_state_file = None;
    if let Some(path) = cli.write_bridge_state.as_ref() {
        let state = bridge_state_file(latest_accepted)?;
        let body = serde_json::to_string_pretty(&state).map_err(|err| format!("failed to encode bridge state file: {err}"))?;
        fs::write(path, format!("{body}\n")).map_err(|err| format!("failed to write bridge state file {}: {err}", path.display()))?;
        bridge_state_written = true;
        written_bridge_state_file = Some(path.display().to_string());
    }

    let rejection_counts = rejection_counts(&rejected_events).into_iter().collect::<std::collections::BTreeMap<_, _>>();
    let summary = ReportSummary {
        mode: "doom-tn10-report",
        event_log: cli.event_log.display().to_string(),
        target_tps: cli.target_tps,
        events: events.len(),
        accepted_events: accepted_events.len(),
        rejected_events: rejected_events.len(),
        unique_txids,
        duplicate_txids,
        unique_wallets,
        rpc_submit_ok: rpc_ok,
        rpc_submit_skipped: rpc_skipped,
        mempool_seen_count: mempool_seen,
        mempool_orphan_count: mempool_orphans,
        inclusion_seen_count: inclusion_seen,
        first_browser_tick: accepted_events.first().map(|event| event.browser_tick).unwrap_or(0),
        last_browser_tick: latest_accepted.map(|event| event.browser_tick).unwrap_or(0),
        first_canonical_tick: accepted_events.first().and_then(|event| event.canonical_tick).unwrap_or(0),
        last_canonical_tick: latest_accepted.and_then(|event| event.canonical_tick).unwrap_or(0),
        canonical_non_monotonic_steps: canonical_gaps,
        browser_non_monotonic_steps: browser_gaps,
        accepted_txid_verified_count: accepted_txid_verified,
        accepted_outpoint_link_verified_count: accepted_outpoint_links_verified,
        first_successor_outpoint: accepted_events.first().and_then(|event| event.successor_outpoint.clone()).unwrap_or_default(),
        last_successor_outpoint: latest_accepted.and_then(|event| event.successor_outpoint.clone()).unwrap_or_default(),
        last_ticcmd_hex: latest_accepted.map(|event| event.ticcmd_hex.clone()).unwrap_or_default(),
        last_state_hash: latest_accepted.and_then(|event| event.state_hash.clone()).unwrap_or_default(),
        last_state_bytes_hex: latest_accepted.and_then(|event| event.state_bytes_hex.clone()).unwrap_or_default(),
        state_bytes_present_count: state_bytes_present,
        latest_state_bytes_len,
        state_bytes_min_len,
        state_bytes_avg_len,
        state_bytes_max_len,
        target_state_bytes_per_second: state_bytes_avg_len * cli.target_tps,
        accepted_state_bytes_per_second: accepted_tps.map(|tps| state_bytes_avg_len * tps),
        latest_kds4,
        state_hash_verified_count: state_hash_verified,
        state_snapshot_verified_count: state_snapshot_verified,
        checkpoint_count,
        checkpoint_verified_count,
        latest_checkpoint_manifest_root_hex: latest_accepted.and_then(|event| event.checkpoint_manifest_root_hex.clone()),
        latest_checkpoint_state_bytes: latest_accepted.and_then(|event| event.checkpoint_state_bytes),
        latest_checkpoint_chunk_count: latest_accepted.and_then(|event| event.checkpoint_chunk_count),
        latest_current_utxo_value_sompi: latest_accepted.and_then(|event| event.current_utxo_value_sompi),
        latest_successor_utxo_value_sompi: latest_accepted.and_then(|event| event.successor_utxo_value_sompi),
        latest_fee_sompi: latest_accepted.and_then(|event| event.fee_sompi),
        submit_elapsed_avg_ms: average(&submit_elapsed),
        submit_elapsed_p50_ms: percentile(&submit_elapsed, 0.50),
        submit_elapsed_p95_ms: percentile(&submit_elapsed, 0.95),
        submit_elapsed_max_ms: submit_elapsed.last().copied().unwrap_or(0.0),
        mempool_seen_elapsed_avg_ms: optional_average(&mempool_elapsed),
        mempool_seen_elapsed_p95_ms: optional_percentile(&mempool_elapsed, 0.95),
        inclusion_seen_elapsed_avg_ms: optional_average(&inclusion_elapsed),
        inclusion_seen_elapsed_p95_ms: optional_percentile(&inclusion_elapsed, 0.95),
        captured_duration_ms,
        accepted_tps,
        accepted_vs_target: accepted_tps.map(|tps| tps / cli.target_tps),
        rejection_counts,
        resume_tuple_prev_tick: latest_accepted.and_then(|event| event.canonical_tick).unwrap_or(0),
        resume_tuple_input_outpoint: latest_accepted.and_then(|event| event.successor_outpoint.clone()).unwrap_or_default(),
        resume_tuple_prev_ticcmd_hex: latest_accepted.map(|event| event.ticcmd_hex.clone()).unwrap_or_default(),
        resume_tuple_prev_state_hash_hex: latest_accepted.and_then(|event| event.state_hash.clone()).unwrap_or_default(),
        resume_tuple_prev_state_bytes_hex: latest_accepted.and_then(|event| event.state_bytes_hex.clone()).unwrap_or_default(),
        resume_tuple_covenant_id: latest_accepted.and_then(|event| event.covenant_id.clone()).unwrap_or_default(),
        bridge_resume_command,
        latest_accepting_block_hash: latest_accepted.and_then(|event| event.accepting_block_hash.clone()),
        bridge_state_written,
        bridge_state_file: written_bridge_state_file,
    };

    if cli.emit_json {
        let body = serde_json::to_string_pretty(&summary).map_err(|err| format!("failed to encode JSON summary: {err}"))?;
        println!("{body}");
    } else {
        print_text_summary(&summary);
    }

    Ok(())
}

fn read_events(path: &PathBuf) -> Result<Vec<BridgeEvent>, String> {
    let file = File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line.map_err(|err| format!("failed to read {} line {}: {err}", path.display(), idx + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let event = serde_json::from_str::<BridgeEvent>(&line)
            .map_err(|err| format!("failed to parse {} line {} as bridge event: {err}", path.display(), idx + 1))?;
        events.push(event);
    }
    Ok(events)
}

fn print_text_summary(summary: &ReportSummary) {
    println!("mode={}", summary.mode);
    println!("event_log={}", summary.event_log);
    println!("target_tps={:.4}", summary.target_tps);
    println!("events={}", summary.events);
    println!("accepted_events={}", summary.accepted_events);
    println!("rejected_events={}", summary.rejected_events);
    println!("unique_txids={}", summary.unique_txids);
    println!("duplicate_txids={}", summary.duplicate_txids);
    println!("unique_wallets={}", summary.unique_wallets);
    println!("rpc_submit_ok={}", summary.rpc_submit_ok);
    println!("rpc_submit_skipped={}", summary.rpc_submit_skipped);
    println!("mempool_seen_count={}", summary.mempool_seen_count);
    println!("mempool_orphan_count={}", summary.mempool_orphan_count);
    println!("inclusion_seen_count={}", summary.inclusion_seen_count);
    println!("first_browser_tick={}", summary.first_browser_tick);
    println!("last_browser_tick={}", summary.last_browser_tick);
    println!("first_canonical_tick={}", summary.first_canonical_tick);
    println!("last_canonical_tick={}", summary.last_canonical_tick);
    println!("canonical_non_monotonic_steps={}", summary.canonical_non_monotonic_steps);
    println!("browser_non_monotonic_steps={}", summary.browser_non_monotonic_steps);
    println!("accepted_txid_verified_count={}", summary.accepted_txid_verified_count);
    println!("accepted_outpoint_link_verified_count={}", summary.accepted_outpoint_link_verified_count);
    println!("first_successor_outpoint={}", summary.first_successor_outpoint);
    println!("last_successor_outpoint={}", summary.last_successor_outpoint);
    println!("last_ticcmd_hex={}", summary.last_ticcmd_hex);
    println!("last_state_hash={}", summary.last_state_hash);
    println!("last_state_bytes_hex={}", summary.last_state_bytes_hex);
    println!("state_bytes_present_count={}", summary.state_bytes_present_count);
    println!("latest_state_bytes_len={}", summary.latest_state_bytes_len);
    println!("state_bytes_min_len={}", summary.state_bytes_min_len);
    println!("state_bytes_avg_len={:.2}", summary.state_bytes_avg_len);
    println!("state_bytes_max_len={}", summary.state_bytes_max_len);
    println!("target_state_bytes_per_second={:.2}", summary.target_state_bytes_per_second);
    print_optional_value("accepted_state_bytes_per_second", summary.accepted_state_bytes_per_second);
    print_kds4_metadata(summary.latest_kds4.as_ref());
    println!("state_hash_verified_count={}", summary.state_hash_verified_count);
    println!("state_snapshot_verified_count={}", summary.state_snapshot_verified_count);
    println!("checkpoint_count={}", summary.checkpoint_count);
    println!("checkpoint_verified_count={}", summary.checkpoint_verified_count);
    if let Some(root) = summary.latest_checkpoint_manifest_root_hex.as_deref() {
        println!("latest_checkpoint_manifest_root_hex={root}");
    }
    if let Some(state_bytes) = summary.latest_checkpoint_state_bytes {
        println!("latest_checkpoint_state_bytes={state_bytes}");
    }
    if let Some(chunk_count) = summary.latest_checkpoint_chunk_count {
        println!("latest_checkpoint_chunk_count={chunk_count}");
    }
    if let Some(value) = summary.latest_current_utxo_value_sompi {
        println!("latest_current_utxo_value_sompi={value}");
    }
    if let Some(value) = summary.latest_successor_utxo_value_sompi {
        println!("latest_successor_utxo_value_sompi={value}");
    }
    if let Some(value) = summary.latest_fee_sompi {
        println!("latest_fee_sompi={value}");
    }
    println!("submit_elapsed_avg_ms={:.2}", summary.submit_elapsed_avg_ms);
    println!("submit_elapsed_p50_ms={:.2}", summary.submit_elapsed_p50_ms);
    println!("submit_elapsed_p95_ms={:.2}", summary.submit_elapsed_p95_ms);
    println!("submit_elapsed_max_ms={:.2}", summary.submit_elapsed_max_ms);
    print_optional_value("mempool_seen_elapsed_avg_ms", summary.mempool_seen_elapsed_avg_ms);
    print_optional_value("mempool_seen_elapsed_p95_ms", summary.mempool_seen_elapsed_p95_ms);
    print_optional_value("inclusion_seen_elapsed_avg_ms", summary.inclusion_seen_elapsed_avg_ms);
    print_optional_value("inclusion_seen_elapsed_p95_ms", summary.inclusion_seen_elapsed_p95_ms);
    match summary.captured_duration_ms {
        Some(duration_ms) => println!("captured_duration_ms={duration_ms:.2}"),
        None => println!("captured_duration_ms=unavailable"),
    }
    match summary.accepted_tps {
        Some(tps) => {
            println!("accepted_tps={tps:.4}");
            println!("accepted_vs_target={:.4}", summary.accepted_vs_target.unwrap_or(0.0));
        }
        None => {
            println!("accepted_tps=unavailable");
            println!("accepted_vs_target=unavailable");
        }
    }
    for (class, count) in &summary.rejection_counts {
        println!("rejection_count[{class}]={count}");
    }
    println!("resume_tuple_prev_tick={}", summary.resume_tuple_prev_tick);
    println!("resume_tuple_input_outpoint={}", summary.resume_tuple_input_outpoint);
    println!("resume_tuple_prev_ticcmd_hex={}", summary.resume_tuple_prev_ticcmd_hex);
    println!("resume_tuple_prev_state_hash_hex={}", summary.resume_tuple_prev_state_hash_hex);
    println!("resume_tuple_prev_state_bytes_hex={}", summary.resume_tuple_prev_state_bytes_hex);
    println!("resume_tuple_covenant_id={}", summary.resume_tuple_covenant_id);
    if let Some(command) = summary.bridge_resume_command.as_deref() {
        println!("bridge_resume_command={command}");
    }
    if summary.bridge_state_written {
        println!("bridge_state_written=true");
        if let Some(path) = summary.bridge_state_file.as_deref() {
            println!("bridge_state_file={path}");
        }
    }
    if let Some(block_hash) = summary.latest_accepting_block_hash.as_deref() {
        println!("latest_accepting_block_hash={block_hash}");
    }
}

fn print_optional_value(name: &str, value: Option<f64>) {
    match value {
        Some(value) => println!("{name}={value:.2}"),
        None => println!("{name}=unavailable"),
    }
}

fn print_kds4_metadata(metadata: Option<&Kds4Metadata>) {
    let Some(metadata) = metadata else {
        println!("latest_kds4=unavailable");
        return;
    };
    println!("latest_kds4_tick={}", metadata.tick);
    println!("latest_kds4_level_time={}", metadata.level_time);
    println!("latest_kds4_prnd_index={}", metadata.prnd_index);
    println!("latest_kds4_rnd_index={}", metadata.rnd_index);
    println!("latest_kds4_active_player={}", metadata.active_player);
    println!("latest_kds4_player_mask={}", metadata.player_mask);
    println!("latest_kds4_max_players={}", metadata.max_players);
    println!("latest_kds4_live_players={}", metadata.live_players);
    println!("latest_kds4_mobj_count={}", metadata.mobj_count);
    println!("latest_kds4_player_hash_hex={}", metadata.player_hash_hex);
    println!("latest_kds4_mobj_hash_hex={}", metadata.mobj_hash_hex);
    println!("latest_kds4_world_hash_hex={}", metadata.world_hash_hex);
    println!("latest_kds4_special_hash_hex={}", metadata.special_hash_hex);
    println!("latest_kds4_special_count={}", metadata.special_count);
    println!("latest_kds4_sector_count={}", metadata.sector_count);
    println!("latest_kds4_line_count={}", metadata.line_count);
    println!("latest_kds4_side_count={}", metadata.side_count);
    println!("latest_kds4_total_kills={}", metadata.total_kills);
    println!("latest_kds4_total_items={}", metadata.total_items);
    println!("latest_kds4_total_secrets={}", metadata.total_secrets);
}

fn count_non_monotonic_steps(events: &[&BridgeEvent], value: impl Fn(&BridgeEvent) -> u32) -> usize {
    events.windows(2).filter(|pair| value(&pair[1]) != value(&pair[0]).saturating_add(1)).count()
}

fn captured_duration_ms(events: &[&BridgeEvent]) -> Option<f64> {
    let first = parse_time(events.first()?.captured_at.as_deref()?)?;
    let last = parse_time(events.last()?.captured_at.as_deref()?)?;
    Some((last - first).num_milliseconds() as f64)
}

fn rejection_counts(events: &[&BridgeEvent]) -> Vec<(String, usize)> {
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for event in events {
        let class = event.rejection_class.as_deref().unwrap_or("unknown").to_string();
        *counts.entry(class).or_insert(0) += 1;
    }
    counts.into_iter().collect()
}

fn state_byte_lengths(events: &[&BridgeEvent]) -> Result<Vec<usize>, String> {
    events
        .iter()
        .filter_map(|event| event.state_bytes_hex.as_deref().map(|hex| (event, hex)))
        .map(|(event, hex)| {
            if hex.len() % 2 != 0 {
                return Err(format!("accepted canonical tick {:?} has odd-length stateBytesHex", event.canonical_tick));
            }
            Ok(hex.len() / 2)
        })
        .collect()
}

fn hex_byte_len(hex: &str) -> usize {
    hex.len() / 2
}

fn verify_accepted_chain_links(events: &[&BridgeEvent]) -> Result<(usize, usize), String> {
    let mut txid_verified = 0usize;
    for event in events {
        if let (Some(tx_id), Some(successor_outpoint)) = (event.tx_id.as_deref(), event.successor_outpoint.as_deref()) {
            let (successor_txid, _) = parse_outpoint(successor_outpoint, "successorOutpoint")?;
            if successor_txid != tx_id {
                return Err(format!(
                    "accepted canonical tick {:?} txId {tx_id} does not match successorOutpoint txid {successor_txid}",
                    event.canonical_tick
                ));
            }
            txid_verified += 1;
        }
    }

    let mut outpoint_links_verified = 0usize;
    for pair in events.windows(2) {
        let previous = pair[0];
        let current = pair[1];
        let Some(previous_successor) = previous.successor_outpoint.as_deref() else {
            continue;
        };
        let Some(current_input) = current.canonical_outpoint_before.as_deref().filter(|value| !value.is_empty()) else {
            continue;
        };
        if current_input != previous_successor {
            return Err(format!(
                "accepted canonical tick {:?} canonicalOutpointBefore {current_input} does not match previous successorOutpoint {previous_successor}",
                current.canonical_tick
            ));
        }
        outpoint_links_verified += 1;
    }

    Ok((txid_verified, outpoint_links_verified))
}

fn bridge_state_file(event: Option<&BridgeEvent>) -> Result<BridgeStateFile, String> {
    let event = event.ok_or("--write-bridge-state requires at least one accepted event")?;
    verify_event_state_bytes(event)?;
    let outpoint =
        event.successor_outpoint.as_deref().ok_or("--write-bridge-state requires latest accepted event successorOutpoint")?;
    let (txid, index) = parse_outpoint(outpoint, "successorOutpoint")?;
    if let Some(event_txid) = event.tx_id.as_deref()
        && event_txid != txid
    {
        return Err(format!("--write-bridge-state txId {event_txid} does not match successorOutpoint txid {txid}"));
    }
    let input_index =
        index.parse::<u32>().map_err(|err| format!("--write-bridge-state cannot parse successorOutpoint index {index:?}: {err}"))?;
    let prev_tick = event.canonical_tick.ok_or("--write-bridge-state requires latest accepted event canonicalTick")?;
    let state_hash = event.state_hash.as_deref().ok_or("--write-bridge-state requires latest accepted event stateHash")?;
    let state_bytes = event.state_bytes_hex.as_deref().ok_or("--write-bridge-state requires latest accepted event stateBytesHex")?;
    Ok(BridgeStateFile {
        input_txid: txid.to_string(),
        input_index,
        utxo_value: event.successor_utxo_value_sompi,
        prev_tick,
        prev_ticcmd_hex: Some(event.ticcmd_hex.clone()),
        prev_state_hash_hex: Some(state_hash.to_string()),
        prev_state_hex: Some(state_bytes.to_string()),
        covenant_id: event.covenant_id.clone(),
        wallet_address: Some(event.wallet_address.clone()),
    })
}

fn parse_outpoint<'a>(outpoint: &'a str, name: &str) -> Result<(&'a str, &'a str), String> {
    let (txid, index) = outpoint.split_once(':').ok_or_else(|| format!("cannot parse {name} {outpoint:?} as <txid>:<index>"))?;
    if txid.is_empty() || index.is_empty() {
        return Err(format!("cannot parse {name} {outpoint:?} as <txid>:<index>"));
    }
    Ok((txid, index))
}

fn verify_event_state_bytes(event: &BridgeEvent) -> Result<(), String> {
    let state_hex = event
        .state_bytes_hex
        .as_deref()
        .ok_or_else(|| format!("accepted canonical tick {:?} has no stateBytesHex", event.canonical_tick))?;
    let state_bytes = parse_hex(state_hex, "stateBytesHex")?;
    let expected_hash = event
        .state_hash
        .as_deref()
        .ok_or_else(|| format!("accepted canonical tick {:?} has stateBytesHex but no stateHash", event.canonical_tick))?;
    let actual_hash = bytes_to_hex(Blake2bParams::new().hash_length(32).hash(&state_bytes).as_bytes());
    if actual_hash != expected_hash {
        return Err(format!(
            "accepted canonical tick {:?} stateBytesHex hashes to {actual_hash}, expected stateHash {expected_hash}",
            event.canonical_tick
        ));
    }
    let expected_tick = event
        .canonical_tick
        .ok_or_else(|| format!("accepted event with stateBytesHex has no canonicalTick: txId={:?}", event.tx_id))?;
    let expected_ticcmd = parse_fixed_hex(&event.ticcmd_hex, 8, "ticcmdHex")?;
    validate_kds4_state_snapshot(&state_bytes, expected_tick, &expected_ticcmd)
        .map_err(|err| format!("accepted canonical tick {expected_tick} has invalid stateBytesHex: {err}"))?;
    Ok(())
}

fn verify_event_checkpoint(event: &BridgeEvent) -> Result<(), String> {
    let root = event
        .checkpoint_manifest_root_hex
        .as_deref()
        .ok_or_else(|| format!("accepted canonical tick {:?} has no checkpointManifestRootHex", event.canonical_tick))?;
    parse_fixed_hex(root, 32, "checkpointManifestRootHex")?;
    let state_bytes = event
        .checkpoint_state_bytes
        .ok_or_else(|| format!("accepted canonical tick {:?} checkpoint missing checkpointStateBytes", event.canonical_tick))?;
    if state_bytes == 0 {
        return Err(format!("accepted canonical tick {:?} checkpointStateBytes must be greater than zero", event.canonical_tick));
    }
    let chunk_count = event
        .checkpoint_chunk_count
        .ok_or_else(|| format!("accepted canonical tick {:?} checkpoint missing checkpointChunkCount", event.canonical_tick))?;
    if chunk_count == 0 {
        return Err(format!("accepted canonical tick {:?} checkpointChunkCount must be greater than zero", event.canonical_tick));
    }
    Ok(())
}

fn validate_kds4_state_snapshot(state_bytes: &[u8], expected_tick: u32, expected_ticcmd: &[u8]) -> Result<(), String> {
    if state_bytes.len() != KDS4_STATE_LEN || !state_bytes.starts_with(b"KDS4") {
        let marker = state_bytes.get(0..4).map(bytes_to_hex).unwrap_or_else(|| bytes_to_hex(state_bytes));
        return Err(format!("stateBytesHex must be a 96-byte KDS4 snapshot, got len={} marker={marker}", state_bytes.len()));
    }
    let state_tick =
        u32::from_le_bytes(state_bytes[4..8].try_into().map_err(|_| "stateBytesHex KDS4 tick field is malformed".to_string())?);
    if state_tick != expected_tick {
        return Err(format!("stateBytesHex KDS4 tick {state_tick} does not match canonical tick {expected_tick}"));
    }
    let state_ticcmd = &state_bytes[KDS4_TICCMD_OFFSET..KDS4_TICCMD_OFFSET + 8];
    if state_ticcmd != expected_ticcmd {
        return Err(format!(
            "stateBytesHex KDS4 ticcmd {} does not match ticcmdHex {}",
            bytes_to_hex(state_ticcmd),
            bytes_to_hex(expected_ticcmd)
        ));
    }
    Ok(())
}

fn decode_kds4_metadata(state_bytes: &[u8]) -> Result<Kds4Metadata, String> {
    if state_bytes.len() != KDS4_STATE_LEN || !state_bytes.starts_with(b"KDS4") {
        let marker = state_bytes.get(0..4).map(bytes_to_hex).unwrap_or_else(|| bytes_to_hex(state_bytes));
        return Err(format!("latest stateBytesHex must be a 96-byte KDS4 snapshot, got len={} marker={marker}", state_bytes.len()));
    }
    Ok(Kds4Metadata {
        tick: read_u32_le(state_bytes, 4, "tick")?,
        level_time: read_u32_le(state_bytes, 8, "level_time")?,
        prnd_index: state_bytes[12],
        rnd_index: state_bytes[13],
        active_player: state_bytes[14],
        player_mask: state_bytes[15],
        max_players: read_u32_le(state_bytes, 16, "max_players")?,
        live_players: read_u32_le(state_bytes, 20, "live_players")?,
        mobj_count: read_u32_le(state_bytes, 24, "mobj_count")?,
        player_hash_hex: bytes_to_hex(&state_bytes[28..36]),
        mobj_hash_hex: bytes_to_hex(&state_bytes[36..44]),
        world_hash_hex: bytes_to_hex(&state_bytes[44..52]),
        special_hash_hex: bytes_to_hex(&state_bytes[52..60]),
        special_count: read_u32_le(state_bytes, 60, "special_count")?,
        sector_count: read_u32_le(state_bytes, 64, "sector_count")?,
        line_count: read_u32_le(state_bytes, 68, "line_count")?,
        side_count: read_u32_le(state_bytes, 72, "side_count")?,
        total_kills: read_u32_le(state_bytes, 76, "total_kills")?,
        total_items: read_u32_le(state_bytes, 80, "total_items")?,
        total_secrets: read_u32_le(state_bytes, 84, "total_secrets")?,
    })
}

fn read_u32_le(bytes: &[u8], offset: usize, name: &str) -> Result<u32, String> {
    let end = offset.checked_add(4).ok_or_else(|| format!("KDS4 {name} offset overflow"))?;
    let chunk = bytes.get(offset..end).ok_or_else(|| format!("KDS4 {name} field is truncated"))?;
    Ok(u32::from_le_bytes(chunk.try_into().map_err(|_| format!("KDS4 {name} field is malformed"))?))
}

fn bridge_resume_command(event: Option<&BridgeEvent>) -> Option<String> {
    let event = event?;
    let outpoint = event.successor_outpoint.as_deref()?;
    let (txid, index) = outpoint.split_once(':')?;
    let tick = event.canonical_tick?;
    let state_hash = event.state_hash.as_deref()?;
    let state_bytes = event.state_bytes_hex.as_deref()?;
    let covenant_arg = event.covenant_id.as_deref().map(|id| format!(" --covenant-id {id}")).unwrap_or_default();
    let utxo_value_arg = event.successor_utxo_value_sompi.map(|value| format!(" --utxo-value {value}")).unwrap_or_default();
    Some(
        format!(
            "cargo run -p silverscript-lang --bin doom_tn10_bridge -- --input-txid {txid} --input-index {index} --wallet-address {} --prev-tick {tick} --prev-ticcmd-hex {} --prev-state-hash-hex {state_hash} --prev-state-hex {state_bytes}",
            event.wallet_address, event.ticcmd_hex
        ) + &utxo_value_arg
            + &covenant_arg,
    )
}

fn accepted_status() -> String {
    "accepted".to_string()
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

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_time(value: &str) -> Option<DateTime<chrono::FixedOffset>> {
    DateTime::parse_from_rfc3339(value).ok()
}

fn average(values: &[f64]) -> f64 {
    if values.is_empty() { 0.0 } else { values.iter().sum::<f64>() / values.len() as f64 }
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((sorted.len() - 1) as f64 * percentile).ceil() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

fn optional_average(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| average(values))
}

fn optional_percentile(sorted: &[f64], percentile_value: f64) -> Option<f64> {
    (!sorted.is_empty()).then(|| percentile(sorted, percentile_value))
}
