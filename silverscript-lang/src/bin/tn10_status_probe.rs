use std::time::Duration;

use clap::Parser;
use kaspa_rpc_core::{RpcAddress, api::rpc::RpcApi};
use kaspa_wrpc_client::{
    KaspaRpcClient, Resolver, WrpcEncoding,
    client::{ConnectOptions, ConnectStrategy},
    prelude::{NetworkId, NetworkType},
};
use silverscript_lang::doom_tn10 as doom;

const DEFAULT_WALLET_ADDRESS: &str = "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz";
const DEFAULT_DOOM_GENESIS_TXID: &str = "44958358cd848ef43c91112738ef8be5ffed574982e152be98913eeb94b726c0";

#[derive(Debug, Parser)]
#[command(name = "tn10-status-probe", about = "Probe the TN10 wRPC path for Doom covenant submission")]
struct Cli {
    /// Optional explicit wRPC endpoint, for example ws://127.0.0.1:17210.
    #[arg(long)]
    url: Option<String>,

    /// Candidate wRPC endpoint to test in scan mode. Can be repeated.
    #[arg(long = "candidate-url")]
    candidate_urls: Vec<String>,

    /// Scan candidate endpoints and print the best Toccata-ready endpoint instead of probing wallet state.
    #[arg(long = "scan-candidates", default_value_t = false)]
    scan_candidates: bool,

    /// Include one PNN-resolved public endpoint in --scan-candidates.
    #[arg(long = "scan-pnn", default_value_t = true, action = clap::ArgAction::Set)]
    scan_pnn: bool,

    /// Connection timeout in milliseconds.
    #[arg(long, default_value_t = 5_000)]
    timeout_ms: u64,

    /// Compare this endpoint's virtual DAA score to a reference TN10 endpoint.
    #[arg(long = "compare-reference", default_value_t = false)]
    compare_reference: bool,

    /// Explicit reference TN10 Borsh wRPC endpoint. If omitted with --compare-reference, the PNN resolver is used.
    #[arg(long = "reference-url")]
    reference_url: Option<String>,

    /// TN10 wallet address to verify for funding.
    #[arg(long = "wallet-address", default_value = DEFAULT_WALLET_ADDRESS)]
    wallet_address: String,

    /// Also probe the initial DoomState P2SH address used by the current genesis plan.
    #[arg(long = "probe-doom-genesis", default_value_t = true, action = clap::ArgAction::Set)]
    probe_doom_genesis: bool,

    /// Expected initial DoomState genesis transaction id, used only for visibility reporting.
    #[arg(long = "doom-genesis-txid", default_value = DEFAULT_DOOM_GENESIS_TXID)]
    doom_genesis_txid: String,

    /// Expected initial DoomState genesis output index, used only for visibility reporting.
    #[arg(long = "doom-genesis-index", default_value_t = 0)]
    doom_genesis_index: u32,
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
    let selected_network = Some(NetworkId::with_suffix(NetworkType::Testnet, 10));
    if cli.scan_candidates {
        return scan_candidates(&cli, selected_network).await;
    }
    let resolved_url = match cli.url.as_deref() {
        Some(url) => {
            println!("pnn_resolved=false");
            url.to_string()
        }
        None => {
            let url = Resolver::default()
                .get_url(WrpcEncoding::Borsh, selected_network.expect("selected network"))
                .await
                .map_err(|err| format!("PNN resolver failed for TN10 Borsh wRPC: {err}"))?;
            println!("pnn_resolved=true");
            println!("pnn_resolved_url={url}");
            url
        }
    };

    let client = connect_client(Some(resolved_url.as_str()), selected_network, cli.timeout_ms, "TN10 wRPC").await?;

    let server_info = client.get_server_info().await.map_err(|err| format!("get_server_info failed: {err}"))?;
    let dag_info = client.get_block_dag_info().await.map_err(|err| format!("get_block_dag_info failed: {err}"))?;
    let wallet_address = RpcAddress::try_from(cli.wallet_address.as_str())
        .map_err(|err| format!("invalid --wallet-address {:?}: {err}", cli.wallet_address))?;
    let wallet_utxos = client
        .get_utxos_by_addresses(vec![wallet_address.clone()])
        .await
        .map_err(|err| format!("get_utxos_by_addresses failed for {wallet_address}: {err}"))?;
    let wallet_balance_sompi: u64 = wallet_utxos.iter().map(|entry| entry.utxo_entry.amount).sum();
    let wallet_funded = wallet_balance_sompi > 0;
    let mut doom_expected_genesis_visible = None;

    println!("mode=tn10-status-probe");
    println!("network_id={}", server_info.network_id);
    println!("server_version={}", server_info.server_version);
    println!("is_synced={}", server_info.is_synced);
    println!("has_utxo_index={}", server_info.has_utxo_index);
    println!("virtual_daa_score={}", dag_info.virtual_daa_score);
    println!("tip_count={}", dag_info.tip_hashes.len());
    println!("sink={}", dag_info.sink);
    println!("wallet_address={wallet_address}");
    println!("wallet_utxo_count={}", wallet_utxos.len());
    println!("wallet_balance_sompi={wallet_balance_sompi}");
    println!("wallet_balance_kas={:.8}", wallet_balance_sompi as f64 / 100_000_000.0);
    println!("wallet_funded={wallet_funded}");
    if let Some(largest_utxo) = wallet_utxos.iter().max_by_key(|entry| entry.utxo_entry.amount) {
        println!("wallet_largest_utxo_txid={}", largest_utxo.outpoint.transaction_id);
        println!("wallet_largest_utxo_index={}", largest_utxo.outpoint.index);
        println!("wallet_largest_utxo_amount_sompi={}", largest_utxo.utxo_entry.amount);
    }
    if cli.probe_doom_genesis {
        let doom_address = initial_doom_state_address()?;
        let doom_rpc_address = RpcAddress::try_from(doom_address.to_string().as_str())
            .map_err(|err| format!("invalid computed DoomState address {doom_address}: {err}"))?;
        let doom_utxos = client
            .get_utxos_by_addresses(vec![doom_rpc_address.clone()])
            .await
            .map_err(|err| format!("get_utxos_by_addresses failed for {doom_rpc_address}: {err}"))?;
        let known_visible = doom_utxos.iter().any(|entry| {
            entry.outpoint.transaction_id.to_string() == cli.doom_genesis_txid && entry.outpoint.index == cli.doom_genesis_index
        });
        doom_expected_genesis_visible = Some(known_visible);
        println!("doom_genesis_probe=true");
        println!("doom_initial_state_address={doom_rpc_address}");
        println!("doom_initial_state_utxo_count={}", doom_utxos.len());
        println!("doom_expected_genesis_outpoint=({}, {})", cli.doom_genesis_txid, cli.doom_genesis_index);
        println!("doom_expected_genesis_visible={known_visible}");
        if let Some(largest_utxo) = doom_utxos.iter().max_by_key(|entry| entry.utxo_entry.amount) {
            println!("doom_largest_utxo_txid={}", largest_utxo.outpoint.transaction_id);
            println!("doom_largest_utxo_index={}", largest_utxo.outpoint.index);
            println!("doom_largest_utxo_amount_sompi={}", largest_utxo.utxo_entry.amount);
            println!("doom_largest_utxo_covenant_id={:?}", largest_utxo.utxo_entry.covenant_id);
        }
    } else {
        println!("doom_genesis_probe=false");
    }
    if cli.compare_reference {
        compare_reference(&cli, selected_network, dag_info.virtual_daa_score).await?;
    }
    println!(
        "next_required_step={}",
        next_required_step(
            &server_info.server_version,
            server_info.is_synced,
            server_info.has_utxo_index,
            wallet_funded,
            doom_expected_genesis_visible,
        )
    );

    client.disconnect().await.map_err(|err| format!("disconnect failed: {err}"))?;
    Ok(())
}

fn next_required_step(
    server_version: &str,
    is_synced: bool,
    has_utxo_index: bool,
    wallet_funded: bool,
    doom_expected_genesis_visible: Option<bool>,
) -> &'static str {
    if !server_version.to_ascii_lowercase().contains("toc") {
        return "use a TN10 Toccata endpoint before submitting covenant transactions";
    }
    if !is_synced {
        return "wait for this Toccata node to sync before relying on UTXO visibility or submitting DoomState transactions";
    }
    if !has_utxo_index {
        return "enable or use a Toccata node with UTXO index before checking wallet/game UTXOs";
    }
    if doom_expected_genesis_visible == Some(true) {
        return "submit the next DoomState tic from the visible genesis/current game UTXO";
    }
    if wallet_funded {
        return "deploy a fresh DoomState genesis through this Toccata endpoint, then submit the next tic";
    }
    "fund the target wallet on this Toccata endpoint/fork view before deploying DoomState genesis"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_step_does_not_suggest_submit_when_toccata_wallet_is_empty() {
        assert_eq!(
            next_required_step("1.2.0-toc.2", true, true, false, Some(false)),
            "fund the target wallet on this Toccata endpoint/fork view before deploying DoomState genesis"
        );
    }

    #[test]
    fn next_step_allows_fresh_genesis_when_toccata_wallet_is_funded() {
        assert_eq!(
            next_required_step("1.2.0-toc.2", true, true, true, Some(false)),
            "deploy a fresh DoomState genesis through this Toccata endpoint, then submit the next tic"
        );
    }

    #[test]
    fn next_step_prefers_visible_genesis() {
        assert_eq!(
            next_required_step("1.2.0-toc.2", true, true, false, Some(true)),
            "submit the next DoomState tic from the visible genesis/current game UTXO"
        );
    }
}

async fn scan_candidates(cli: &Cli, selected_network: Option<NetworkId>) -> Result<(), String> {
    println!("mode=tn10-endpoint-scan");
    let mut candidates = cli.candidate_urls.clone();
    if let Some(url) = &cli.url {
        candidates.insert(0, url.clone());
    }
    if cli.scan_pnn {
        match Resolver::default().get_url(WrpcEncoding::Borsh, selected_network.expect("selected network")).await {
            Ok(url) => {
                println!("scan_pnn_resolved=true");
                println!("scan_pnn_url={url}");
                candidates.push(url);
            }
            Err(err) => {
                println!("scan_pnn_resolved=false");
                println!("scan_pnn_error={err}");
            }
        }
    }

    let mut seen = std::collections::HashSet::new();
    candidates.retain(|url| seen.insert(url.clone()));
    if candidates.is_empty() {
        return Err("no endpoints to scan; pass --candidate-url or --url".to_string());
    }

    let wallet_address = RpcAddress::try_from(cli.wallet_address.as_str())
        .map_err(|err| format!("invalid --wallet-address {:?} for candidate scan: {err}", cli.wallet_address))?;
    let mut best_funded_synced_toccata: Option<String> = None;
    let mut best_synced_toccata: Option<String> = None;
    let mut best_toccata: Option<String> = None;
    for (idx, url) in candidates.iter().enumerate() {
        println!("candidate_index={idx}");
        println!("candidate_url={url}");
        match probe_endpoint_summary(url, selected_network, cli.timeout_ms, &wallet_address).await {
            Ok(summary) => {
                let is_toccata = summary.server_version.to_ascii_lowercase().contains("toc");
                let wallet_funded = summary.wallet_balance_sompi > 0;
                println!("candidate_connected=true");
                println!("candidate_server_version={}", summary.server_version);
                println!("candidate_is_synced={}", summary.is_synced);
                println!("candidate_has_utxo_index={}", summary.has_utxo_index);
                println!("candidate_virtual_daa_score={}", summary.virtual_daa_score);
                println!("candidate_is_toccata={is_toccata}");
                println!("candidate_wallet_address={wallet_address}");
                println!("candidate_wallet_utxo_count={}", summary.wallet_utxo_count);
                println!("candidate_wallet_balance_sompi={}", summary.wallet_balance_sompi);
                println!("candidate_wallet_balance_kas={:.8}", summary.wallet_balance_sompi as f64 / 100_000_000.0);
                println!("candidate_wallet_funded={wallet_funded}");
                if let Some(txid) = summary.wallet_largest_utxo_txid.as_deref() {
                    println!("candidate_wallet_largest_utxo_txid={txid}");
                    println!("candidate_wallet_largest_utxo_index={}", summary.wallet_largest_utxo_index.unwrap_or(0));
                    println!("candidate_wallet_largest_utxo_amount_sompi={}", summary.wallet_largest_utxo_amount_sompi.unwrap_or(0));
                }
                println!("candidate_selectable={}", is_toccata && summary.is_synced && summary.has_utxo_index);
                println!("candidate_funded_selectable={}", is_toccata && summary.is_synced && summary.has_utxo_index && wallet_funded);
                if is_toccata && best_toccata.is_none() {
                    best_toccata = Some(url.clone());
                }
                if is_toccata && summary.is_synced && summary.has_utxo_index && best_synced_toccata.is_none() {
                    best_synced_toccata = Some(url.clone());
                }
                if is_toccata && summary.is_synced && summary.has_utxo_index && wallet_funded && best_funded_synced_toccata.is_none() {
                    best_funded_synced_toccata = Some(url.clone());
                }
            }
            Err(err) => {
                println!("candidate_connected=false");
                println!("candidate_error={err}");
            }
        }
    }
    if let Some(url) = best_funded_synced_toccata {
        println!("selected_endpoint={url}");
        println!("selected_reason=synced_toccata_utxoindex_wallet_funded");
    } else if let Some(url) = best_synced_toccata {
        println!("selected_endpoint={url}");
        println!("selected_reason=synced_toccata_utxoindex");
    } else if let Some(url) = best_toccata {
        println!("selected_endpoint={url}");
        println!("selected_reason=toccata_but_not_synced_or_no_utxoindex");
    } else {
        println!("selected_endpoint=");
        println!("selected_reason=no_toccata_endpoint_connected");
    }
    Ok(())
}

struct EndpointSummary {
    server_version: String,
    is_synced: bool,
    has_utxo_index: bool,
    virtual_daa_score: u64,
    wallet_utxo_count: usize,
    wallet_balance_sompi: u64,
    wallet_largest_utxo_txid: Option<String>,
    wallet_largest_utxo_index: Option<u32>,
    wallet_largest_utxo_amount_sompi: Option<u64>,
}

async fn probe_endpoint_summary(
    url: &str,
    selected_network: Option<NetworkId>,
    timeout_ms: u64,
    wallet_address: &RpcAddress,
) -> Result<EndpointSummary, String> {
    let client = connect_client(Some(url), selected_network, timeout_ms, "candidate TN10 wRPC").await?;
    let server_info = client.get_server_info().await.map_err(|err| format!("candidate get_server_info failed for {url}: {err}"))?;
    let dag_info = client.get_block_dag_info().await.map_err(|err| format!("candidate get_block_dag_info failed for {url}: {err}"))?;
    let wallet_utxos = client
        .get_utxos_by_addresses(vec![wallet_address.clone()])
        .await
        .map_err(|err| format!("candidate get_utxos_by_addresses failed for {wallet_address} at {url}: {err}"))?;
    let wallet_balance_sompi = wallet_utxos.iter().map(|entry| entry.utxo_entry.amount).sum();
    let largest_utxo = wallet_utxos.iter().max_by_key(|entry| entry.utxo_entry.amount);
    client.disconnect().await.map_err(|err| format!("candidate disconnect failed for {url}: {err}"))?;
    Ok(EndpointSummary {
        server_version: server_info.server_version,
        is_synced: server_info.is_synced,
        has_utxo_index: server_info.has_utxo_index,
        virtual_daa_score: dag_info.virtual_daa_score,
        wallet_utxo_count: wallet_utxos.len(),
        wallet_balance_sompi,
        wallet_largest_utxo_txid: largest_utxo.map(|entry| entry.outpoint.transaction_id.to_string()),
        wallet_largest_utxo_index: largest_utxo.map(|entry| entry.outpoint.index),
        wallet_largest_utxo_amount_sompi: largest_utxo.map(|entry| entry.utxo_entry.amount),
    })
}

async fn compare_reference(cli: &Cli, selected_network: Option<NetworkId>, local_virtual_daa_score: u64) -> Result<(), String> {
    let reference_url = match cli.reference_url.as_deref() {
        Some(url) => {
            println!("reference_pnn_resolved=false");
            url.to_string()
        }
        None => {
            let url = Resolver::default()
                .get_url(WrpcEncoding::Borsh, selected_network.expect("selected network"))
                .await
                .map_err(|err| format!("reference PNN resolver failed for TN10 Borsh wRPC: {err}"))?;
            println!("reference_pnn_resolved=true");
            println!("reference_url={url}");
            url
        }
    };
    let reference = connect_client(Some(reference_url.as_str()), selected_network, cli.timeout_ms, "reference TN10 wRPC").await?;
    let server_info = reference.get_server_info().await.map_err(|err| format!("reference get_server_info failed: {err}"))?;
    let dag_info = reference.get_block_dag_info().await.map_err(|err| format!("reference get_block_dag_info failed: {err}"))?;
    let wallet_address = RpcAddress::try_from(cli.wallet_address.as_str())
        .map_err(|err| format!("invalid --wallet-address {:?} for reference probe: {err}", cli.wallet_address))?;
    let wallet_utxos = reference
        .get_utxos_by_addresses(vec![wallet_address.clone()])
        .await
        .map_err(|err| format!("reference get_utxos_by_addresses failed for {wallet_address}: {err}"))?;
    let wallet_balance_sompi: u64 = wallet_utxos.iter().map(|entry| entry.utxo_entry.amount).sum();
    let reference_score = dag_info.virtual_daa_score;
    let lag = reference_score.saturating_sub(local_virtual_daa_score);
    println!("reference_server_version={}", server_info.server_version);
    println!("reference_is_synced={}", server_info.is_synced);
    println!("reference_virtual_daa_score={reference_score}");
    println!("reference_wallet_address={wallet_address}");
    println!("reference_wallet_utxo_count={}", wallet_utxos.len());
    println!("reference_wallet_balance_sompi={wallet_balance_sompi}");
    println!("reference_wallet_balance_kas={:.8}", wallet_balance_sompi as f64 / 100_000_000.0);
    println!("reference_wallet_funded={}", wallet_balance_sompi > 0);
    if let Some(largest_utxo) = wallet_utxos.iter().max_by_key(|entry| entry.utxo_entry.amount) {
        println!("reference_wallet_largest_utxo_txid={}", largest_utxo.outpoint.transaction_id);
        println!("reference_wallet_largest_utxo_index={}", largest_utxo.outpoint.index);
        println!("reference_wallet_largest_utxo_amount_sompi={}", largest_utxo.utxo_entry.amount);
    }
    println!("sync_virtual_daa_lag={lag}");
    if reference_score > 0 {
        let caught_up = (local_virtual_daa_score as f64 / reference_score as f64) * 100.0;
        println!("sync_virtual_daa_caught_up_percent={caught_up:.4}");
    } else {
        println!("sync_virtual_daa_caught_up_percent=unavailable");
    }
    reference.disconnect().await.map_err(|err| format!("reference disconnect failed: {err}"))?;
    Ok(())
}

async fn connect_client(
    url: Option<&str>,
    selected_network: Option<NetworkId>,
    timeout_ms: u64,
    label: &str,
) -> Result<KaspaRpcClient, String> {
    let client = KaspaRpcClient::new(WrpcEncoding::Borsh, url, None, selected_network, None)
        .map_err(|err| format!("failed to create {label} client: {err}"))?;
    let options = ConnectOptions {
        block_async_connect: true,
        connect_timeout: Some(Duration::from_millis(timeout_ms)),
        strategy: ConnectStrategy::Fallback,
        ..Default::default()
    };
    client.connect(Some(options)).await.map_err(|err| format!("failed to connect to {label}: {err}"))?;
    Ok(client)
}

fn initial_doom_state_address() -> Result<kaspa_addresses::Address, String> {
    doom::initial_doom_state_address()
}
