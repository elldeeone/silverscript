use std::str::FromStr;

use clap::Parser;
use kaspa_addresses::{Address, Prefix, Version};
use kaspa_wallet_keys::derivation::gen1::WalletDerivationManager;
use secp256k1::{Keypair, Secp256k1, SecretKey};

const DEFAULT_WALLET_ADDRESS: &str = "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz";

#[derive(Debug, Parser)]
#[command(
    name = "tn10-wallet-key-check",
    about = "Verify that local signing material derives the expected TN10 wallet address without printing secrets",
    next_line_help = true
)]
struct Cli {
    /// Expected TN10 wallet address.
    #[arg(long = "wallet-address", default_value = DEFAULT_WALLET_ADDRESS)]
    wallet_address: String,

    /// Environment variable containing the wallet mnemonic. The value is never printed.
    #[arg(long = "mnemonic-env", default_value = "KASPA_TN10_MNEMONIC")]
    mnemonic_env: String,

    /// Optional environment variable containing a raw private key hex. The value is never printed.
    #[arg(long = "private-key-env")]
    private_key_env: Option<String>,

    /// Receive/change keys to scan when deriving from a mnemonic.
    #[arg(long = "scan", default_value_t = 64)]
    scan: u32,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let wallet_address = Address::try_from(cli.wallet_address.as_str())
        .map_err(|err| format!("invalid --wallet-address {:?}: {err}", cli.wallet_address))?;
    let matched = load_wallet_key(&wallet_address, &cli)?;

    println!("mode=tn10-wallet-key-check");
    println!("wallet_address={wallet_address}");
    println!("key_available=true");
    println!("key_matches_wallet=true");
    println!("matched_key_path={}", matched.path);
    println!("derived_address={}", matched.address);
    println!("private_key_printed=false");
    Ok(())
}

struct MatchedWalletKey {
    path: String,
    address: Address,
}

fn load_wallet_key(wallet_address: &Address, cli: &Cli) -> Result<MatchedWalletKey, String> {
    if let Some(env_name) = &cli.private_key_env {
        if let Ok(private_key_hex) = std::env::var(env_name) {
            let secret = SecretKey::from_str(private_key_hex.trim())
                .map_err(|err| format!("failed to parse private key from {env_name}: {err}"))?;
            let address = address_from_secret(&secret);
            if &address != wallet_address {
                return Err(format!("private key from {env_name} derives {address}, expected {wallet_address}"));
            }
            return Ok(MatchedWalletKey { path: format!("{env_name}:direct"), address });
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
            let address = address_from_secret(&secret);
            if address == *wallet_address {
                return Ok(MatchedWalletKey { path: format!("{path}/{index}"), address });
            }
        }
    }

    Err(format!("mnemonic in {} did not derive {wallet_address} in first {} receive/change keys", cli.mnemonic_env, cli.scan))
}

fn address_from_secret(secret: &SecretKey) -> Address {
    let public_key = secp256k1::PublicKey::from_secret_key_global(secret);
    let (x_only_public_key, _) = public_key.x_only_public_key();
    Address::new(Prefix::Testnet, Version::PubKey, &x_only_public_key.serialize())
}

#[allow(dead_code)]
fn keypair_from_secret(secret: SecretKey) -> Keypair {
    let secp = Secp256k1::new();
    Keypair::from_secret_key(&secp, &secret)
}
