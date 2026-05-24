use std::time::Instant;

use clap::Parser;
use kaspa_consensus_core::Hash;
use kaspa_consensus_core::hashing::sighash::{SigHashReusedValuesUnsync, calc_schnorr_signature_hash};
use kaspa_consensus_core::hashing::sighash_type::SIG_HASH_ALL;
use kaspa_consensus_core::tx::{
    CovenantBinding, PopulatedTransaction, Transaction, TransactionId, TransactionInput, TransactionOutpoint, TransactionOutput,
    UtxoEntry, VerifiableTransaction,
};
use kaspa_txscript::caches::Cache;
use kaspa_txscript::covenants::CovenantsContext;
use kaspa_txscript::script_builder::ScriptBuilder;
use kaspa_txscript::{EngineCtx, EngineFlags, TxScriptEngine, parse_script, pay_to_script_hash_script};
use kaspa_txscript_errors::TxScriptError;
use secp256k1::{Keypair, Message, Secp256k1, SecretKey};
use silverscript_lang::ast::Expr;
use silverscript_lang::compiler::{CompileOptions, CompiledContract, CovenantDeclCallOptions, compile_contract, struct_object};
use silverscript_lang::doom_tn10 as doom;

const DOOM_STATE_SOURCE: &str = include_str!("../../tests/apps/doom/doom_state.sil");
const COV_DOOM: Hash = Hash::from_bytes([0xdd; 32]);

#[derive(Debug, Parser)]
#[command(
    name = "doom-state-driver",
    about = "Execute local DoomState covenant tic chains before live TN10 submission",
    next_line_help = true
)]
struct Cli {
    /// Number of chained Doom tics to execute locally.
    #[arg(long, default_value_t = 10)]
    ticks: u32,

    /// Target canonical Doom tic rate for reporting.
    #[arg(long = "target-tps", default_value_t = 10)]
    target_tps: u32,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    if cli.ticks == 0 {
        return Err("--ticks must be greater than zero".to_string());
    }
    if cli.target_tps == 0 {
        return Err("--target-tps must be greater than zero".to_string());
    }

    let mut active = doom::compile_initial_doom_state()?;
    let (instruction_count, charged_op_count) = script_op_counts(&active.script)?;
    let start = Instant::now();

    for tick in 1..=cli.ticks {
        let next_ticcmd = doom::synthetic_ticcmd_for_tick(tick);
        let next_state_chunk = doom::kds4_state_for_tick(tick);
        let next_state = doom_state_expr(i64::from(tick), next_ticcmd.clone(), next_state_chunk.clone());
        let next = compile_doom_state(doom_state_args(i64::from(tick), next_ticcmd.clone(), blake2b_bytes(&next_state_chunk)))?;

        let placeholder_sigscript = doom_advance_sigscript(&active, next_ticcmd.clone(), next_state.clone(), vec![0; 65])?;
        let outputs = vec![covenant_output(&next, 0, COV_DOOM)];
        let entries = vec![covenant_utxo(&active, COV_DOOM)];
        let mut tx = Transaction::new(1, vec![tx_input(tick - 1, placeholder_sigscript)], outputs, 0, Default::default(), 0, vec![]);
        let sig = sign_tx_input_schnorr(&tx, &entries, 0);
        tx.inputs[0].signature_script = doom_advance_sigscript(&active, next_ticcmd, next_state, sig)?;
        execute_input_with_covenants(tx, entries, 0).map_err(|err| format!("tick {tick} rejected by local VM: {err}"))?;

        active = next;
    }

    let elapsed = start.elapsed();
    let elapsed_ms = elapsed.as_secs_f64() * 1_000.0;
    let local_tps = f64::from(cli.ticks) / elapsed.as_secs_f64().max(0.001);
    let target_ms = (f64::from(cli.ticks) / f64::from(cli.target_tps)) * 1_000.0;

    println!("mode=local-vm");
    println!("ticks_attempted={}", cli.ticks);
    println!("ticks_executed={}", cli.ticks);
    println!("target_tps={}", cli.target_tps);
    println!("target_elapsed_ms={target_ms:.2}");
    println!("local_elapsed_ms={elapsed_ms:.2}");
    println!("local_vm_tps={local_tps:.2}");
    println!("script_len={}", active.script.len());
    println!("instruction_count={instruction_count}");
    println!("charged_op_count={charged_op_count}");
    println!("final_tick={}", cli.ticks);
    println!("next_required_step=replace local VM execution with TN10 RPC submission and block inclusion tracking");

    Ok(())
}

fn doom_state_args(tick: i64, ticcmd: Vec<u8>, state_hash: Vec<u8>) -> Vec<Expr<'static>> {
    vec![
        Expr::bytes(vec![0x11; 32]),
        Expr::bytes(session_key_hash()),
        Expr::int(tick),
        Expr::bytes(state_hash),
        Expr::int(doom::KDS4_STATE_LEN as i64),
        Expr::bytes(ticcmd),
    ]
}

fn doom_state_expr(tick: i64, ticcmd: Vec<u8>, state_chunk: Vec<u8>) -> Expr<'static> {
    struct_object(vec![
        ("game_id", Expr::bytes(vec![0x11; 32])),
        ("session_key_hash", Expr::bytes(session_key_hash())),
        ("tick", Expr::int(tick)),
        ("state_hash", Expr::bytes(blake2b_bytes(&state_chunk))),
        ("state_len", Expr::int(state_chunk.len() as i64)),
        ("ticcmd", Expr::bytes(ticcmd)),
    ])
}

fn compile_doom_state(args: Vec<Expr<'static>>) -> Result<CompiledContract<'static>, String> {
    compile_contract(DOOM_STATE_SOURCE, &args, CompileOptions::default()).map_err(|err| format!("DoomState compile failed: {err}"))
}

fn blake2b_bytes(data: &[u8]) -> Vec<u8> {
    blake2b_simd::Params::new().hash_length(32).hash(data).as_bytes().to_vec()
}

fn session_keypair() -> Keypair {
    let secp = Secp256k1::new();
    let secret = SecretKey::from_slice(&[7u8; 32]).expect("valid deterministic session secret");
    Keypair::from_secret_key(&secp, &secret)
}

fn session_pubkey() -> Vec<u8> {
    let (x_only, _) = session_keypair().x_only_public_key();
    x_only.serialize().to_vec()
}

fn session_key_hash() -> Vec<u8> {
    blake2b_bytes(&session_pubkey())
}

fn script_op_counts(script: &[u8]) -> Result<(usize, usize), String> {
    let mut instruction_count = 0;
    let mut charged_op_count = 0;

    for opcode in parse_script::<PopulatedTransaction<'static>, SigHashReusedValuesUnsync>(script) {
        let opcode = opcode.map_err(|err| format!("compiled script should parse: {err}"))?;
        instruction_count += 1;
        if !opcode.is_push_opcode() {
            charged_op_count += 1;
        }
    }

    Ok((instruction_count, charged_op_count))
}

fn push_redeem_script(script: &[u8]) -> Vec<u8> {
    ScriptBuilder::new().add_data(script).expect("push redeem script").drain()
}

fn tx_input(index: u32, signature_script: Vec<u8>) -> TransactionInput {
    TransactionInput::new_with_compute_budget(
        TransactionOutpoint { transaction_id: TransactionId::from_bytes([(index + 1) as u8; 32]), index },
        signature_script,
        0,
        1_000,
    )
}

fn covenant_output(compiled: &CompiledContract<'_>, authorizing_input: u16, covenant_id: Hash) -> TransactionOutput {
    TransactionOutput {
        value: 1_000,
        script_public_key: pay_to_script_hash_script(&compiled.script),
        covenant: Some(CovenantBinding { authorizing_input, covenant_id }),
    }
}

fn covenant_utxo(compiled: &CompiledContract<'_>, covenant_id: Hash) -> UtxoEntry {
    UtxoEntry::new(1_000, pay_to_script_hash_script(&compiled.script), 0, false, Some(covenant_id))
}

fn execute_input_with_covenants(tx: Transaction, entries: Vec<UtxoEntry>, input_idx: usize) -> Result<(), TxScriptError> {
    let reused_values = SigHashReusedValuesUnsync::new();
    let sig_cache = Cache::new(10_000);
    let input = tx.inputs[input_idx].clone();
    let populated = PopulatedTransaction::new(&tx, entries);
    let cov_ctx = CovenantsContext::from_tx(&populated).map_err(TxScriptError::from)?;
    let utxo = populated.utxo(input_idx).expect("selected input utxo");
    let mut vm = TxScriptEngine::from_transaction_input(
        &populated,
        &input,
        input_idx,
        utxo,
        EngineCtx::new(&sig_cache).with_reused(&reused_values).with_covenants_ctx(&cov_ctx),
        EngineFlags { covenants_enabled: true, ..Default::default() },
    );
    vm.execute()
}

fn doom_advance_sigscript(
    compiled: &CompiledContract<'_>,
    next_ticcmd: Vec<u8>,
    next_state: Expr<'_>,
    signature: Vec<u8>,
) -> Result<Vec<u8>, String> {
    let mut sigscript = compiled
        .build_sig_script_for_covenant_decl(
            "advance",
            vec![Expr::bytes(next_ticcmd), next_state, Expr::bytes(session_pubkey()), Expr::bytes(signature)],
            CovenantDeclCallOptions { is_leader: false },
        )
        .map_err(|err| format!("per-tic covenant sigscript failed: {err}"))?;
    sigscript.extend_from_slice(&push_redeem_script(&compiled.script));
    Ok(sigscript)
}

fn sign_tx_input_schnorr(tx: &Transaction, entries: &[UtxoEntry], input_idx: usize) -> Vec<u8> {
    let reused_values = SigHashReusedValuesUnsync::new();
    let populated = PopulatedTransaction::new(tx, entries.to_vec());
    let sig_hash = calc_schnorr_signature_hash(&populated, input_idx, SIG_HASH_ALL, &reused_values);
    let msg = Message::from_digest_slice(sig_hash.as_bytes().as_slice()).expect("valid sighash message");
    let sig = session_keypair().sign_schnorr(msg);
    let mut signature = Vec::new();
    signature.extend_from_slice(sig.as_ref());
    signature.push(SIG_HASH_ALL.to_u8());
    signature
}
