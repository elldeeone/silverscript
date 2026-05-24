use std::str::FromStr;
use std::time::Duration;

use clap::Parser;
use kaspa_addresses::{Address, Prefix, Version};
use kaspa_consensus_core::constants::TX_VERSION_TOCCATA;
use kaspa_consensus_core::hashing;
use kaspa_consensus_core::hashing::sighash::{SigHashReusedValuesUnsync, calc_schnorr_signature_hash};
use kaspa_consensus_core::hashing::sighash_type::SIG_HASH_ALL;
use kaspa_consensus_core::tx::{
    CovenantBinding, PopulatedTransaction, Transaction, TransactionInput, TransactionOutput, UtxoEntry, VerifiableTransaction,
};
use kaspa_rpc_core::{RpcAddress, RpcTransaction, api::rpc::RpcApi};
use kaspa_txscript::caches::Cache;
use kaspa_txscript::{EngineCtx, EngineFlags, TxScriptEngine, pay_to_address_script, pay_to_script_hash_script};
use kaspa_txscript_errors::TxScriptError;
use kaspa_wallet_keys::derivation::gen1::WalletDerivationManager;
use kaspa_wrpc_client::{
    KaspaRpcClient, Resolver, WrpcEncoding,
    client::{ConnectOptions, ConnectStrategy},
    prelude::{NetworkId, NetworkType},
};
use secp256k1::{Keypair, Message, Secp256k1, SecretKey};
use silverscript_lang::doom_tn10 as doom;

const DEFAULT_WALLET_ADDRESS: &str = "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz";

#[derive(Debug, Parser)]
#[command(
    name = "doom-tn10-genesis-plan",
    about = "Plan the initial DoomState covenant UTXO from a funded TN10 wallet UTXO",
    next_line_help = true
)]
struct Cli {
    /// Optional explicit TN10 wRPC endpoint, for example ws://127.0.0.1:17210.
    #[arg(long)]
    url: Option<String>,

    /// Connection timeout in milliseconds.
    #[arg(long, default_value_t = 5_000)]
    timeout_ms: u64,

    /// Funded TN10 wallet address.
    #[arg(long = "wallet-address", default_value = DEFAULT_WALLET_ADDRESS)]
    wallet_address: String,

    /// Amount to lock in the initial DoomState UTXO.
    #[arg(long = "game-value", default_value_t = 100_000_000)]
    game_value: u64,

    /// Fee to leave unspent from the selected funding UTXO.
    #[arg(long = "fee", default_value_t = 20_000_000)]
    fee: u64,

    /// Environment variable containing the wallet mnemonic. The value is never printed.
    #[arg(long = "mnemonic-env", default_value = "KASPA_TN10_MNEMONIC")]
    mnemonic_env: String,

    /// Optional environment variable containing a raw private key hex. The value is never printed.
    #[arg(long = "private-key-env")]
    private_key_env: Option<String>,

    /// Receive/change keys to scan when deriving from a mnemonic.
    #[arg(long = "scan", default_value_t = 64)]
    scan: u32,

    /// Submit the signed deploy transaction to TN10. Without this flag the command is dry-run only.
    #[arg(long)]
    submit: bool,
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
    let wallet_address = RpcAddress::try_from(cli.wallet_address.as_str())
        .map_err(|err| format!("invalid --wallet-address {:?}: {err}", cli.wallet_address))?;
    let client = connect_tn10(cli.url.as_deref(), cli.timeout_ms).await?;
    if cli.submit {
        assert_toccata_endpoint(&client).await?;
    }
    let wallet_utxos = client
        .get_utxos_by_addresses(vec![wallet_address.clone()])
        .await
        .map_err(|err| format!("get_utxos_by_addresses failed for {wallet_address}: {err}"))?;
    let funding = wallet_utxos
        .iter()
        .max_by_key(|entry| entry.utxo_entry.amount)
        .ok_or_else(|| format!("wallet {wallet_address} has no spendable UTXOs"))?;

    if funding.utxo_entry.amount <= cli.game_value + cli.fee {
        return Err(format!(
            "largest wallet UTXO has {} sompi, needs more than {} sompi",
            funding.utxo_entry.amount,
            cli.game_value + cli.fee
        ));
    }

    let initial = doom::compile_initial_doom_state()?;
    let game_output_without_covenant =
        TransactionOutput { value: cli.game_value, script_public_key: pay_to_script_hash_script(&initial.script), covenant: None };
    let funding_outpoint = funding.outpoint.into();
    let doom_covenant_id = hashing::covenant_id::covenant_id(funding_outpoint, std::iter::once((0, &game_output_without_covenant)));
    let game_output = TransactionOutput {
        covenant: Some(CovenantBinding { authorizing_input: 0, covenant_id: doom_covenant_id }),
        ..game_output_without_covenant
    };
    let change_value = funding.utxo_entry.amount - cli.game_value - cli.fee;
    let change_output =
        TransactionOutput { value: change_value, script_public_key: pay_to_address_script(&wallet_address), covenant: None };
    let entries = vec![UtxoEntry::new(
        funding.utxo_entry.amount,
        funding.utxo_entry.script_public_key.clone(),
        funding.utxo_entry.block_daa_score,
        funding.utxo_entry.is_coinbase,
        funding.utxo_entry.covenant_id,
    )];
    let mut tx = Transaction::new(
        TX_VERSION_TOCCATA,
        vec![TransactionInput::new_with_compute_budget(funding_outpoint, vec![], 0, 1_000)],
        vec![game_output, change_output],
        0,
        Default::default(),
        0,
        vec![],
    );
    let matched_key = load_wallet_key(&wallet_address, &cli)?;
    let signature = sign_input(&tx, &entries, 0, &matched_key.keypair);
    tx.inputs[0].signature_script = p2pk_sigscript(signature);
    tx.finalize();
    execute_input(tx.clone(), entries, 0).map_err(|err| format!("signed genesis tx failed local validation: {err}"))?;

    println!("mode=tn10-genesis-plan");
    println!("network_id=testnet-10");
    println!("wallet_address={wallet_address}");
    println!("funding_outpoint=({}, {})", funding.outpoint.transaction_id, funding.outpoint.index);
    println!("funding_amount_sompi={}", funding.utxo_entry.amount);
    println!("game_value_sompi={}", cli.game_value);
    println!("fee_sompi={}", cli.fee);
    println!("change_value_sompi={change_value}");
    println!("doom_covenant_id={doom_covenant_id}");
    println!("matched_key_path={}", matched_key.path);
    println!("signed_deploy_tx_id={}", tx.id());
    println!("initial_game_outpoint=({}, 0)", tx.id());
    println!("initial_state_tick=0");
    println!("script_len={}", initial.script.len());
    println!("local_validation=ok");

    if !cli.submit {
        println!("rpc_submit=skipped");
        println!(
            "next_required_step=rerun with --submit to deploy the initial DoomState UTXO, then feed initial_game_outpoint to doom_tn10_submitter"
        );
        client.disconnect().await.map_err(|err| format!("disconnect failed: {err}"))?;
        return Ok(());
    }

    let submitted_id = client
        .submit_transaction(RpcTransaction::from(&tx), false)
        .await
        .map_err(|err| format!("TN10 submit_transaction rejected genesis deploy {}: {err}", tx.id()))?;
    println!("rpc_submit=ok");
    println!("submitted_tx_id={submitted_id}");
    println!("next_required_step=feed initial_game_outpoint to doom_tn10_submitter --submit for tick 1");

    client.disconnect().await.map_err(|err| format!("disconnect failed: {err}"))?;
    Ok(())
}

struct MatchedWalletKey {
    keypair: Keypair,
    path: String,
}

fn load_wallet_key(wallet_address: &Address, cli: &Cli) -> Result<MatchedWalletKey, String> {
    if let Some(env_name) = &cli.private_key_env {
        if let Ok(private_key_hex) = std::env::var(env_name) {
            let secret = SecretKey::from_str(private_key_hex.trim())
                .map_err(|err| format!("failed to parse private key from {env_name}: {err}"))?;
            let keypair = keypair_from_secret(secret);
            let address = address_from_secret(&secret);
            if &address != wallet_address {
                return Err(format!("private key from {env_name} derives {address}, expected {wallet_address}"));
            }
            return Ok(MatchedWalletKey { keypair, path: format!("{env_name}:direct") });
        }
    }

    let mnemonic = std::env::var(&cli.mnemonic_env)
        .map_err(|_| format!("set {} to the TN10 wallet mnemonic, or pass --private-key-env", cli.mnemonic_env))?;
    let mnemonic = kaspa_bip32::Mnemonic::new(mnemonic.trim(), kaspa_bip32::Language::English)
        .map_err(|err| format!("failed to parse mnemonic from {}: {err}", cli.mnemonic_env))?;
    let master = kaspa_bip32::ExtendedPrivateKey::<kaspa_bip32::SecretKey>::new(mnemonic.to_seed(""))
        .map_err(|err| format!("failed to derive master key from {}: {err}", cli.mnemonic_env))?;

    for address_type in [kaspa_bip32::AddressType::Receive, kaspa_bip32::AddressType::Change] {
        let path = WalletDerivationManager::build_derivate_path(false, 0, None, Some(address_type))
            .map_err(|err| format!("failed to build derivation path: {err}"))?;
        let base = master.clone().derive_path(&path).map_err(|err| format!("failed to derive {path}: {err}"))?;
        for index in 0..cli.scan {
            let child = base
                .clone()
                .derive_child(kaspa_bip32::ChildNumber::new(index, false).map_err(|err| format!("invalid child index: {err}"))?)
                .map_err(|err| format!("failed to derive {path}/{index}: {err}"))?;
            let secret = *child.private_key();
            if address_from_secret(&secret) == *wallet_address {
                return Ok(MatchedWalletKey { keypair: keypair_from_secret(secret), path: format!("{path}/{index}") });
            }
        }
    }

    Err(format!("mnemonic in {} did not derive {wallet_address} in first {} receive/change keys", cli.mnemonic_env, cli.scan))
}

fn keypair_from_secret(secret: SecretKey) -> Keypair {
    let secp = Secp256k1::new();
    Keypair::from_secret_key(&secp, &secret)
}

fn address_from_secret(secret: &SecretKey) -> Address {
    let public_key = secp256k1::PublicKey::from_secret_key_global(secret);
    let (x_only_public_key, _) = public_key.x_only_public_key();
    Address::new(Prefix::Testnet, Version::PubKey, &x_only_public_key.serialize())
}

fn sign_input(tx: &Transaction, entries: &[UtxoEntry], input_idx: usize, keypair: &Keypair) -> Vec<u8> {
    let reused_values = SigHashReusedValuesUnsync::new();
    let populated = PopulatedTransaction::new(tx, entries.to_vec());
    let sig_hash = calc_schnorr_signature_hash(&populated, input_idx, SIG_HASH_ALL, &reused_values);
    let msg = Message::from_digest_slice(sig_hash.as_bytes().as_slice()).expect("valid sighash message");
    let sig = keypair.sign_schnorr(msg);
    let mut signature = Vec::new();
    signature.extend_from_slice(sig.as_ref());
    signature.push(SIG_HASH_ALL.to_u8());
    signature
}

fn p2pk_sigscript(signature: Vec<u8>) -> Vec<u8> {
    std::iter::once(signature.len() as u8).chain(signature).collect()
}

fn execute_input(tx: Transaction, entries: Vec<UtxoEntry>, input_idx: usize) -> Result<(), TxScriptError> {
    let sig_cache = Cache::new(10_000);
    let reused_values = SigHashReusedValuesUnsync::new();
    let input = tx.inputs[input_idx].clone();
    let populated = PopulatedTransaction::new(&tx, entries);
    let utxo = populated.utxo(input_idx).expect("selected input utxo");
    let mut vm = TxScriptEngine::from_transaction_input(
        &populated,
        &input,
        input_idx,
        utxo,
        EngineCtx::new(&sig_cache).with_reused(&reused_values),
        EngineFlags { covenants_enabled: true, ..Default::default() },
    );
    vm.execute()
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
            "refusing live DoomState genesis deploy through non-Toccata endpoint {}; pass an upgraded TN10 Toccata wRPC URL such as ws://10.0.3.26:17210",
            server_info.server_version
        ));
    }
    Ok(())
}
