use std::{
    process::{Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

use clap::Parser;

const DEFAULT_URL: &str = "wss://testnet10-wrpc.kasia.fyi";
const DEFAULT_INPUT_TXID: &str = "44958358cd848ef43c91112738ef8be5ffed574982e152be98913eeb94b726c0";
const DEFAULT_COVENANT_ID: &str = "b33892ffcac705a87ce7a747cf8e289c7d48d87cecf1469731f5374a13625fb0";
const DEFAULT_WALLET_ADDRESS: &str = "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz";

#[derive(Debug, Parser)]
#[command(name = "doom-tn10-live", about = "Run the default DoomState TN10 Toccata live submit path", next_line_help = true)]
struct Cli {
    /// Explicit TN10 Toccata wRPC endpoint.
    #[arg(long, default_value = DEFAULT_URL)]
    url: String,

    /// Scan candidate endpoints and use the selected endpoint before probing/submitting.
    #[arg(long = "auto-select-endpoint", default_value_t = false)]
    auto_select_endpoint: bool,

    /// Candidate endpoint for --auto-select-endpoint. Can be repeated.
    #[arg(long = "candidate-url")]
    candidate_urls: Vec<String>,

    /// Include one PNN-resolved public endpoint when auto-selecting endpoints.
    #[arg(long = "scan-pnn", default_value_t = true, action = clap::ArgAction::Set)]
    scan_pnn: bool,

    /// Timeout passed to each candidate endpoint scan.
    #[arg(long = "endpoint-scan-timeout-ms", default_value_t = 8_000)]
    endpoint_scan_timeout_ms: u64,

    /// Current DoomState input transaction id.
    #[arg(long = "input-txid", default_value = DEFAULT_INPUT_TXID)]
    input_txid: String,

    /// Current DoomState input output index.
    #[arg(long = "input-index", default_value_t = 0)]
    input_index: u32,

    /// Current DoomState UTXO value in sompi.
    #[arg(long = "utxo-value", default_value_t = 100_000_000)]
    utxo_value: u64,

    /// Fee to pay for fresh genesis and each DoomState tic transition in sompi.
    #[arg(long = "fee", default_value_t = 20_000_000)]
    fee: u64,

    /// Current DoomState covenant id.
    #[arg(long = "covenant-id", default_value = DEFAULT_COVENANT_ID)]
    covenant_id: String,

    /// TN10 wallet address expected to fund and authorize the game.
    #[arg(long = "wallet-address", default_value = DEFAULT_WALLET_ADDRESS)]
    wallet_address: String,

    /// Tick currently represented by the input DoomState UTXO.
    #[arg(long = "prev-tick", default_value_t = 0)]
    prev_tick: u32,

    /// Number of tics to submit after preflight passes.
    #[arg(long, default_value_t = 1)]
    ticks: u32,

    /// Optional target tics/sec pacing passed to the submitter.
    #[arg(long = "target-tps", default_value_t = 0.0)]
    target_tps: f64,

    /// Wait for the starting DoomState input to appear before broadcasting.
    #[arg(long = "wait-preflight", default_value_t = true, action = clap::ArgAction::Set)]
    wait_preflight: bool,

    /// Maximum time to wait for preflight input visibility. Use 0 to wait indefinitely.
    #[arg(long = "preflight-timeout-ms", default_value_t = 0)]
    preflight_timeout_ms: u64,

    /// Poll interval while waiting for preflight input visibility.
    #[arg(long = "preflight-poll-ms", default_value_t = 10_000)]
    preflight_poll_ms: u64,

    /// Poll accepted transaction visibility after submit.
    #[arg(long = "track-inclusion", default_value_t = true, action = clap::ArgAction::Set)]
    track_inclusion: bool,

    /// Run a status probe first, then execute the submitter.
    #[arg(long = "probe-first", default_value_t = true, action = clap::ArgAction::Set)]
    probe_first: bool,

    /// Verify endpoint, wallet funding, known genesis visibility, and local signing material, then exit without genesis/tic submission.
    #[arg(long = "readiness-only", default_value_t = false)]
    readiness_only: bool,

    /// During status probes, compare the selected node's virtual DAA score to a reference TN10 endpoint.
    #[arg(long = "compare-reference", default_value_t = true, action = clap::ArgAction::Set)]
    compare_reference: bool,

    /// Explicit reference TN10 Borsh wRPC endpoint for sync-progress comparison.
    #[arg(long = "reference-url")]
    reference_url: Option<String>,

    /// Deploy a fresh DoomState genesis through the Toccata endpoint before submitting tics.
    #[arg(long = "deploy-fresh-genesis", default_value_t = false)]
    deploy_fresh_genesis: bool,

    /// If the old genesis is not visible but the wallet is funded, deploy a fresh genesis.
    #[arg(long = "auto-genesis", default_value_t = false)]
    auto_genesis: bool,

    /// Poll status until either the known DoomState genesis is visible or auto-genesis can deploy from wallet funds.
    #[arg(long = "wait-readiness", default_value_t = false)]
    wait_readiness: bool,

    /// Maximum time to wait for readiness. Use 0 to wait indefinitely.
    #[arg(long = "readiness-timeout-ms", default_value_t = 0)]
    readiness_timeout_ms: u64,

    /// Poll interval while waiting for readiness.
    #[arg(long = "readiness-poll-ms", default_value_t = 10_000)]
    readiness_poll_ms: u64,

    /// Environment variable containing the TN10 wallet mnemonic for fresh genesis deploy.
    #[arg(long = "mnemonic-env", default_value = "KASPA_TN10_MNEMONIC")]
    mnemonic_env: String,

    /// Write a doom_tn10_bridge-compatible state file after the final submitted tic.
    #[arg(long = "write-bridge-state")]
    write_bridge_state: Option<String>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut cli = Cli::parse();
    println!("mode=doom-tn10-live");
    if cli.auto_select_endpoint {
        if cli.candidate_urls.is_empty() {
            cli.candidate_urls.push("wss://testnet10-wrpc.kasia.fyi".to_string());
            cli.candidate_urls.push("wss://photon-10.kaspa.red/kaspa/testnet-10/wrpc/borsh".to_string());
            cli.candidate_urls.push("wss://baryon-10.kaspa.green/kaspa/testnet-10/wrpc/borsh".to_string());
        }
        let selection = select_endpoint(&cli)?;
        if !selection.endpoint.is_empty() {
            cli.url = selection.endpoint;
        }
        println!("selected_endpoint={}", cli.url);
        println!("selected_reason={}", selection.reason);
    }
    println!("url={}", cli.url);
    println!("input_outpoint=({}, {})", cli.input_txid, cli.input_index);
    println!("wallet_address={}", cli.wallet_address);
    println!("prev_tick={}", cli.prev_tick);
    println!("ticks={}", cli.ticks);
    println!("wait_preflight={}", cli.wait_preflight);

    let mut input_txid = cli.input_txid.clone();
    let mut input_index = cli.input_index;
    let mut covenant_id = cli.covenant_id.clone();
    let mut prev_tick = cli.prev_tick;
    let mut should_deploy_fresh_genesis = cli.deploy_fresh_genesis;

    if cli.probe_first || cli.auto_genesis || cli.wait_readiness || cli.readiness_only {
        let status = wait_for_readiness(&cli)?;
        println!("readiness_old_genesis_visible={}", status.doom_expected_genesis_visible);
        println!("readiness_wallet_funded={}", status.wallet_funded);
        println!("readiness_wallet_balance_kas={}", status.wallet_balance_kas.as_deref().unwrap_or("unknown"));
        println!(
            "readiness_reference_wallet_funded={}",
            status.reference_wallet_funded.map(|value| value.to_string()).unwrap_or_else(|| "unknown".to_string())
        );
        println!("readiness_reference_wallet_balance_kas={}", status.reference_wallet_balance_kas.as_deref().unwrap_or("unknown"));
        println!("readiness_key_available={}", status.key_available);
        println!("readiness_key_matches={}", status.key_matches);
        println!("readiness_ready_to_submit_existing={}", status.ready_to_submit_existing());
        println!("readiness_ready_to_deploy_fresh_genesis={}", status.ready_to_deploy_fresh_genesis());
        if should_auto_deploy_fresh_genesis(&cli, &status) {
            println!("readiness_decision=deploy_fresh_genesis");
            should_deploy_fresh_genesis = true;
        } else if status.doom_expected_genesis_visible {
            println!("readiness_decision=use_existing_genesis");
        } else if cli.readiness_only {
            println!("readiness_decision=not_ready");
        } else if cli.auto_genesis {
            return Err(format!("not ready: {}", readiness_next_step(&status)));
        }
        if cli.readiness_only {
            let decision = if status.ready_to_submit_existing() {
                "ready_existing_genesis"
            } else if status.ready_to_deploy_fresh_genesis() {
                "ready_fresh_genesis"
            } else {
                "not_ready"
            };
            println!("readiness_only=true");
            println!("readiness_result={decision}");
            println!("next_required_step={}", readiness_next_step(&status));
            return Ok(());
        }
    }

    if should_deploy_fresh_genesis {
        let genesis = deploy_fresh_genesis(&cli)?;
        input_txid = genesis.input_txid;
        input_index = genesis.input_index;
        covenant_id = genesis.covenant_id;
        prev_tick = 0;
        println!("fresh_genesis_input_outpoint=({}, {})", input_txid, input_index);
        println!("fresh_genesis_covenant_id={covenant_id}");
    }

    let mut owned_args = vec![
        "--url".to_string(),
        cli.url.clone(),
        "--prev-tick".to_string(),
        prev_tick.to_string(),
        "--ticks".to_string(),
        cli.ticks.to_string(),
        "--input-txid".to_string(),
        input_txid,
        "--input-index".to_string(),
        input_index.to_string(),
        "--utxo-value".to_string(),
        cli.utxo_value.to_string(),
        "--fee".to_string(),
        cli.fee.to_string(),
        "--covenant-id".to_string(),
        covenant_id,
        "--wallet-address".to_string(),
        cli.wallet_address.clone(),
        "--target-tps".to_string(),
        cli.target_tps.to_string(),
        "--submit".to_string(),
        "--allow-orphan".to_string(),
        "--preflight-input".to_string(),
        "true".to_string(),
        "--preflight-timeout-ms".to_string(),
        cli.preflight_timeout_ms.to_string(),
        "--preflight-poll-ms".to_string(),
        cli.preflight_poll_ms.to_string(),
        "--track-mempool".to_string(),
        "true".to_string(),
        "--track-inclusion".to_string(),
        cli.track_inclusion.to_string(),
        "--inclusion-timeout-ms".to_string(),
        "10000".to_string(),
    ];
    if cli.wait_preflight {
        owned_args.push("--wait-preflight".to_string());
    }
    if let Some(path) = &cli.write_bridge_state {
        owned_args.push("--write-bridge-state".to_string());
        owned_args.push(path.clone());
    }
    let arg_refs = owned_args.iter().map(String::as_str).collect::<Vec<_>>();
    run_cargo_bin("doom_tn10_submitter", &arg_refs)
}

struct EndpointSelection {
    endpoint: String,
    reason: String,
}

fn select_endpoint(cli: &Cli) -> Result<EndpointSelection, String> {
    println!("running_bin=tn10_status_probe");
    let mut owned_args = vec![
        "run".to_string(),
        "-q".to_string(),
        "-p".to_string(),
        "silverscript-lang".to_string(),
        "--bin".to_string(),
        "tn10_status_probe".to_string(),
        "--".to_string(),
        "--scan-candidates".to_string(),
        "--timeout-ms".to_string(),
        cli.endpoint_scan_timeout_ms.to_string(),
        "--url".to_string(),
        cli.url.clone(),
        "--scan-pnn".to_string(),
        cli.scan_pnn.to_string(),
        "--wallet-address".to_string(),
        cli.wallet_address.clone(),
    ];
    for candidate in &cli.candidate_urls {
        owned_args.push("--candidate-url".to_string());
        owned_args.push(candidate.clone());
    }
    let output = Command::new("cargo")
        .args(owned_args)
        .stdin(Stdio::inherit())
        .output()
        .map_err(|err| format!("failed to launch tn10_status_probe endpoint scan: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    print!("{stdout}");
    eprint!("{stderr}");
    if !output.status.success() {
        return Err(format!("tn10_status_probe endpoint scan exited with {}", output.status));
    }
    let endpoint = parse_string_line(&stdout, "selected_endpoint=").unwrap_or_default();
    let reason = parse_string_line(&stdout, "selected_reason=").unwrap_or_else(|| "unknown".to_string());
    Ok(EndpointSelection { endpoint, reason })
}

#[derive(Debug)]
struct StatusProbe {
    wallet_funded: bool,
    wallet_balance_kas: Option<String>,
    reference_wallet_funded: Option<bool>,
    reference_wallet_balance_kas: Option<String>,
    doom_expected_genesis_visible: bool,
    key_available: bool,
    key_matches: bool,
}

impl StatusProbe {
    fn ready_to_submit_existing(&self) -> bool {
        self.doom_expected_genesis_visible
    }

    fn ready_to_deploy_fresh_genesis(&self) -> bool {
        self.wallet_funded && self.key_available && self.key_matches
    }
}

struct FreshGenesis {
    input_txid: String,
    input_index: u32,
    covenant_id: String,
}

fn wait_for_readiness(cli: &Cli) -> Result<StatusProbe, String> {
    let started = Instant::now();
    let mut first = true;
    loop {
        let status = run_status_probe(cli, true)?;
        if status.doom_expected_genesis_visible || should_auto_deploy_fresh_genesis(cli, &status) || !cli.wait_readiness {
            return Ok(status);
        }
        if cli.readiness_timeout_ms > 0 && started.elapsed() >= Duration::from_millis(cli.readiness_timeout_ms) {
            return Err(format!(
                "readiness timeout after {}ms: old DoomState genesis is not visible and wallet_funded={}",
                cli.readiness_timeout_ms, status.wallet_funded
            ));
        }
        if first {
            println!("readiness_waiting=true");
            first = false;
        }
        sleep(Duration::from_millis(cli.readiness_poll_ms));
    }
}

fn run_status_probe(cli: &Cli, print_output: bool) -> Result<StatusProbe, String> {
    println!("running_bin=tn10_status_probe");
    let mut owned_args = vec![
        "run".to_string(),
        "-q".to_string(),
        "-p".to_string(),
        "silverscript-lang".to_string(),
        "--bin".to_string(),
        "tn10_status_probe".to_string(),
        "--".to_string(),
        "--url".to_string(),
        cli.url.clone(),
        "--timeout-ms".to_string(),
        "12000".to_string(),
        "--wallet-address".to_string(),
        cli.wallet_address.clone(),
    ];
    if cli.compare_reference {
        owned_args.push("--compare-reference".to_string());
    }
    if let Some(reference_url) = &cli.reference_url {
        owned_args.push("--reference-url".to_string());
        owned_args.push(reference_url.clone());
    }
    let output = Command::new("cargo")
        .args(owned_args)
        .stdin(Stdio::inherit())
        .output()
        .map_err(|err| format!("failed to launch tn10_status_probe: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if print_output {
        print!("{stdout}");
        eprint!("{stderr}");
    }
    if !output.status.success() {
        return Err(format!("tn10_status_probe exited with {}", output.status));
    }
    let key_check = run_wallet_key_check(cli, print_output);
    Ok(StatusProbe {
        wallet_funded: parse_bool_line(&stdout, "wallet_funded=").unwrap_or(false),
        wallet_balance_kas: parse_string_line(&stdout, "wallet_balance_kas="),
        reference_wallet_funded: parse_bool_line(&stdout, "reference_wallet_funded="),
        reference_wallet_balance_kas: parse_string_line(&stdout, "reference_wallet_balance_kas="),
        doom_expected_genesis_visible: parse_bool_line(&stdout, "doom_expected_genesis_visible=").unwrap_or(false),
        key_available: key_check.available,
        key_matches: key_check.matches,
    })
}

struct WalletKeyStatus {
    available: bool,
    matches: bool,
}

fn run_wallet_key_check(cli: &Cli, print_output: bool) -> WalletKeyStatus {
    println!("running_bin=tn10_wallet_key_check");
    let output = Command::new("cargo")
        .args([
            "run",
            "-q",
            "-p",
            "silverscript-lang",
            "--bin",
            "tn10_wallet_key_check",
            "--",
            "--wallet-address",
            &cli.wallet_address,
            "--mnemonic-env",
            &cli.mnemonic_env,
        ])
        .stdin(Stdio::inherit())
        .output();
    let Ok(output) = output else {
        if print_output {
            println!("wallet_key_available=false");
            println!("wallet_key_matches=false");
        }
        return WalletKeyStatus { available: false, matches: false };
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if print_output {
        print!("{stdout}");
        eprint!("{stderr}");
    }
    WalletKeyStatus { available: output.status.success(), matches: parse_bool_line(&stdout, "key_matches_wallet=").unwrap_or(false) }
}

fn readiness_next_step(status: &StatusProbe) -> &'static str {
    if status.ready_to_submit_existing() {
        "run doom_tn10_live without --readiness-only to submit from the visible DoomState genesis"
    } else if status.ready_to_deploy_fresh_genesis() {
        "run doom_tn10_live --auto-genesis without --readiness-only to deploy a fresh Toccata genesis and submit tick 1"
    } else if !status.key_available {
        "set KASPA_TN10_MNEMONIC or pass --mnemonic-env with local signing material for the target wallet"
    } else if !status.key_matches {
        "fix local signing material; it does not derive the target TN10 wallet address"
    } else if !status.wallet_funded && status.reference_wallet_funded == Some(true) {
        "wallet funds are visible on the reference TN10 endpoint but not on the selected Toccata endpoint; fund this wallet on the Toccata fork/view or use a synced Toccata node that sees the funded UTXO"
    } else if !status.wallet_funded {
        "fund the target wallet on the selected Toccata endpoint, then rerun readiness"
    } else {
        "wait for the known DoomState genesis to become visible or use --auto-genesis once wallet funding is visible"
    }
}

fn should_auto_deploy_fresh_genesis(cli: &Cli, status: &StatusProbe) -> bool {
    cli.auto_genesis && !status.doom_expected_genesis_visible && status.ready_to_deploy_fresh_genesis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_next_step_explains_reference_funded_toccata_empty_split() {
        let status = StatusProbe {
            wallet_funded: false,
            wallet_balance_kas: Some("0.00000000".to_string()),
            reference_wallet_funded: Some(true),
            reference_wallet_balance_kas: Some("99895.99950727".to_string()),
            doom_expected_genesis_visible: false,
            key_available: true,
            key_matches: true,
        };

        assert!(readiness_next_step(&status).contains("reference TN10 endpoint but not on the selected Toccata endpoint"));
    }

    #[test]
    fn auto_genesis_requires_matching_local_wallet_key() {
        let mut cli = Cli::parse_from(["doom_tn10_live", "--auto-genesis"]);
        cli.auto_genesis = true;
        let status = StatusProbe {
            wallet_funded: true,
            wallet_balance_kas: Some("10.00000000".to_string()),
            reference_wallet_funded: None,
            reference_wallet_balance_kas: None,
            doom_expected_genesis_visible: false,
            key_available: true,
            key_matches: false,
        };

        assert!(!should_auto_deploy_fresh_genesis(&cli, &status));
        assert_eq!(readiness_next_step(&status), "fix local signing material; it does not derive the target TN10 wallet address");
    }

    #[test]
    fn auto_genesis_allowed_when_toccata_wallet_and_key_are_ready() {
        let mut cli = Cli::parse_from(["doom_tn10_live", "--auto-genesis"]);
        cli.auto_genesis = true;
        let status = StatusProbe {
            wallet_funded: true,
            wallet_balance_kas: Some("10.00000000".to_string()),
            reference_wallet_funded: None,
            reference_wallet_balance_kas: None,
            doom_expected_genesis_visible: false,
            key_available: true,
            key_matches: true,
        };

        assert!(should_auto_deploy_fresh_genesis(&cli, &status));
        assert_eq!(
            readiness_next_step(&status),
            "run doom_tn10_live --auto-genesis without --readiness-only to deploy a fresh Toccata genesis and submit tick 1"
        );
    }
}

fn deploy_fresh_genesis(cli: &Cli) -> Result<FreshGenesis, String> {
    println!("running_bin=doom_tn10_genesis_plan");
    let game_value = cli.utxo_value.to_string();
    let fee = cli.fee.to_string();
    let output = Command::new("cargo")
        .args([
            "run",
            "-q",
            "-p",
            "silverscript-lang",
            "--bin",
            "doom_tn10_genesis_plan",
            "--",
            "--url",
            &cli.url,
            "--timeout-ms",
            "12000",
            "--wallet-address",
            &cli.wallet_address,
            "--game-value",
            &game_value,
            "--fee",
            &fee,
            "--mnemonic-env",
            &cli.mnemonic_env,
            "--submit",
        ])
        .stdin(Stdio::inherit())
        .output()
        .map_err(|err| format!("failed to launch doom_tn10_genesis_plan: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    print!("{stdout}");
    eprint!("{stderr}");
    if !output.status.success() {
        return Err(format!("doom_tn10_genesis_plan exited with {}", output.status));
    }

    let covenant_id = parse_string_line(&stdout, "doom_covenant_id=").ok_or("genesis output did not include doom_covenant_id")?;
    let input_outpoint =
        parse_initial_game_outpoint(&stdout).ok_or("genesis output did not include initial_game_outpoint=(<txid>, <index>)")?;
    Ok(FreshGenesis { input_txid: input_outpoint.0, input_index: input_outpoint.1, covenant_id })
}

fn run_cargo_bin(bin: &str, args: &[&str]) -> Result<(), String> {
    println!("running_bin={bin}");
    let status = Command::new("cargo")
        .args(["run", "-q", "-p", "silverscript-lang", "--bin", bin, "--"])
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|err| format!("failed to launch cargo run for {bin}: {err}"))?;
    if status.success() { Ok(()) } else { Err(format!("{bin} exited with {status}")) }
}

fn parse_string_line(output: &str, prefix: &str) -> Option<String> {
    output.lines().find_map(|line| line.strip_prefix(prefix).map(str::trim).map(str::to_string))
}

fn parse_bool_line(output: &str, prefix: &str) -> Option<bool> {
    match parse_string_line(output, prefix)?.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn parse_initial_game_outpoint(output: &str) -> Option<(String, u32)> {
    let value = parse_string_line(output, "initial_game_outpoint=")?;
    let trimmed = value.strip_prefix('(')?.strip_suffix(')')?;
    let (txid, index) = trimmed.split_once(',')?;
    Some((txid.trim().to_string(), index.trim().parse().ok()?))
}
