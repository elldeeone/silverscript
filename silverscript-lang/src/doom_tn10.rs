use std::str::FromStr;

use kaspa_addresses::Prefix;
use kaspa_consensus_core::Hash;
use kaspa_consensus_core::constants::TX_VERSION_TOCCATA;
use kaspa_consensus_core::hashing::sighash::{SigHashReusedValuesUnsync, calc_schnorr_signature_hash};
use kaspa_consensus_core::hashing::sighash_type::SIG_HASH_ALL;
use kaspa_consensus_core::tx::{
    CovenantBinding, PopulatedTransaction, Transaction, TransactionId, TransactionInput, TransactionOutpoint, TransactionOutput,
    UtxoEntry, VerifiableTransaction,
};
use kaspa_txscript::caches::Cache;
use kaspa_txscript::covenants::CovenantsContext;
use kaspa_txscript::script_builder::ScriptBuilder;
use kaspa_txscript::{
    EngineCtx, EngineFlags, TxScriptEngine, extract_script_pub_key_address, parse_script, pay_to_script_hash_script,
};
use kaspa_txscript_errors::TxScriptError;
use secp256k1::{Keypair, Message, Secp256k1, SecretKey};

use crate::ast::Expr;
use crate::compiler::{CompileOptions, CompiledContract, CovenantDeclCallOptions, compile_contract, struct_object};

const DOOM_STATE_SOURCE: &str = include_str!("../tests/apps/doom/doom_state.sil");
pub const COV_DOOM: Hash = Hash::from_bytes([0xdd; 32]);
pub const KDS4_STATE_LEN: usize = 96;
const KDS4_TICCMD_OFFSET: usize = 88;

pub struct BuiltTransition {
    pub tx: Transaction,
    pub entries: Vec<UtxoEntry>,
    pub fee: u64,
    pub successor_utxo_value: u64,
    pub next_tick: u32,
    pub next_ticcmd: Vec<u8>,
    pub next_state_chunk: Vec<u8>,
    pub next_state_hash: Vec<u8>,
    pub next_state_chunk_len: usize,
    pub script_len: usize,
    pub instruction_count: usize,
    pub charged_op_count: usize,
}

#[derive(Clone, Debug)]
pub struct CurrentState {
    tick: u32,
    ticcmd: Option<Vec<u8>>,
    state_hash: Option<Vec<u8>>,
}

impl CurrentState {
    pub fn from_cli(tick: u32, ticcmd_hex: Option<&str>, state_hash_hex: Option<&str>) -> Result<Self, String> {
        if tick == 0 && ticcmd_hex.is_none() && state_hash_hex.is_none() {
            return Ok(Self { tick, ticcmd: None, state_hash: None });
        }
        let ticcmd = ticcmd_hex.map(parse_ticcmd_hex).transpose()?;
        let state_hash = state_hash_hex.map(|value| parse_fixed_hex(value, 32, "--prev-state-hash-hex")).transpose()?;
        match (ticcmd, state_hash) {
            (Some(ticcmd), Some(state_hash)) => Ok(Self { tick, ticcmd: Some(ticcmd), state_hash: Some(state_hash) }),
            (None, None) => Ok(Self::synthetic(tick)),
            _ => Err("--prev-ticcmd-hex and --prev-state-hash-hex must be provided together".to_string()),
        }
    }

    pub fn synthetic(tick: u32) -> Self {
        Self { tick, ticcmd: Some(ticcmd_for_tick(tick)), state_hash: Some(state_hash_for_tick(tick)) }
    }

    pub fn from_parts(tick: u32, ticcmd: Vec<u8>, state_hash: Vec<u8>) -> Self {
        Self { tick, ticcmd: Some(ticcmd), state_hash: Some(state_hash) }
    }

    pub fn tick(&self) -> u32 {
        self.tick
    }

    fn args(&self) -> Vec<Expr<'static>> {
        if self.tick == 0 && self.ticcmd.is_none() && self.state_hash.is_none() {
            doom_constructor_args()
        } else {
            doom_state_args(
                i64::from(self.tick),
                self.ticcmd.clone().expect("non-genesis current state has ticcmd"),
                self.state_hash.clone().expect("non-genesis current state has state hash"),
            )
        }
    }
}

pub fn build_transition(
    current_state: &CurrentState,
    input_outpoint: TransactionOutpoint,
    covenant_id: Hash,
    utxo_value: u64,
    fee: u64,
    next_ticcmd_override: Option<Vec<u8>>,
    next_state_override: Option<Vec<u8>>,
) -> Result<BuiltTransition, String> {
    if utxo_value <= fee {
        return Err(format!("DoomState UTXO value {utxo_value} must exceed fee {fee}"));
    }
    let successor_utxo_value = utxo_value - fee;
    let active = compile_doom_state(current_state.args())?;
    let prev_tick = current_state.tick;
    let next_tick = prev_tick.checked_add(1).ok_or("--prev-tick plus --ticks is too large")?;
    let next_ticcmd = next_ticcmd_override.unwrap_or_else(|| ticcmd_for_tick(next_tick));
    let next_state_chunk = match next_state_override {
        Some(state_bytes) => {
            validate_kds4_state_snapshot(&state_bytes, next_tick, &next_ticcmd)?;
            state_bytes
        }
        None => state_chunk_for_tick_and_ticcmd(next_tick, &next_ticcmd),
    };
    let next_state_hash = blake2b_bytes(&next_state_chunk);
    let next_state = doom_state_expr(i64::from(next_tick), next_ticcmd.clone(), next_state_chunk.clone());
    let next = compile_doom_state(doom_state_args(i64::from(next_tick), next_ticcmd.clone(), next_state_hash.clone()))?;

    let placeholder_sigscript = doom_advance_sigscript(&active, next_ticcmd.clone(), next_state.clone(), vec![0; 65])?;
    let outputs = vec![covenant_output(&next, 0, covenant_id, successor_utxo_value)];
    let entries = vec![covenant_utxo(&active, covenant_id, utxo_value)];
    let mut tx = Transaction::new(
        TX_VERSION_TOCCATA,
        vec![tx_input(input_outpoint, placeholder_sigscript)],
        outputs,
        0,
        Default::default(),
        0,
        vec![],
    );
    let sig = sign_tx_input_schnorr(&tx, &entries, 0);
    tx.inputs[0].signature_script = doom_advance_sigscript(&active, next_ticcmd.clone(), next_state, sig)?;
    tx.finalize();

    let (instruction_count, charged_op_count) = script_op_counts(&active.script)?;
    Ok(BuiltTransition {
        tx,
        entries,
        fee,
        successor_utxo_value,
        next_tick,
        next_ticcmd,
        next_state_chunk: next_state_chunk.clone(),
        next_state_hash,
        next_state_chunk_len: next_state_chunk.len(),
        script_len: active.script.len(),
        instruction_count,
        charged_op_count,
    })
}

pub fn doom_state_address(current_state: &CurrentState) -> Result<kaspa_addresses::Address, String> {
    let active = compile_doom_state(current_state.args())?;
    let script_public_key = pay_to_script_hash_script(&active.script);
    extract_script_pub_key_address(&script_public_key, Prefix::Testnet)
        .map_err(|err| format!("failed to derive DoomState address for preflight: {err}"))
}

pub fn execute_input_with_covenants(tx: Transaction, entries: Vec<UtxoEntry>, input_idx: usize) -> Result<(), TxScriptError> {
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

pub fn parse_txid(txid: &str) -> Result<TransactionId, String> {
    TransactionId::from_str(txid).map_err(|err| format!("invalid --input-txid {txid:?}: {err}"))
}

pub fn parse_hash(hash: &str, name: &str) -> Result<Hash, String> {
    Hash::from_str(hash).map_err(|err| format!("invalid {name} {hash:?}: {err}"))
}

pub fn parse_ticcmd_hex(hex: &str) -> Result<Vec<u8>, String> {
    parse_fixed_hex(hex, 8, "--next-ticcmd-hex/--prev-ticcmd-hex")
}

pub fn parse_hex(hex: &str, name: &str) -> Result<Vec<u8>, String> {
    let hex = hex.trim();
    if hex.is_empty() {
        return Err(format!("{name} must not be empty"));
    }
    if hex.len() % 2 != 0 {
        return Err(format!("{name} must have an even number of hex chars, got {}", hex.len()));
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for idx in (0..hex.len()).step_by(2) {
        out.push(u8::from_str_radix(&hex[idx..idx + 2], 16).map_err(|err| format!("invalid {name} at byte {}: {err}", idx / 2))?);
    }
    Ok(out)
}

pub fn parse_fixed_hex(hex: &str, expected_bytes: usize, name: &str) -> Result<Vec<u8>, String> {
    let hex = hex.trim();
    let expected_len = expected_bytes * 2;
    if hex.len() != expected_len {
        return Err(format!("{name} must be exactly {expected_len} hex chars for {expected_bytes} bytes, got {}", hex.len()));
    }
    parse_hex(hex, name)
}

pub fn validate_kds4_state_snapshot(state_bytes: &[u8], expected_tick: u32, expected_ticcmd: &[u8]) -> Result<(), String> {
    if state_bytes.len() != KDS4_STATE_LEN || !state_bytes.starts_with(b"KDS4") {
        let marker = state_bytes.get(0..4).map(bytes_to_hex).unwrap_or_else(|| bytes_to_hex(state_bytes));
        return Err(format!("--next-state-hex must be a 96-byte KDS4 snapshot, got len={} marker={marker}", state_bytes.len()));
    }
    let state_tick =
        u32::from_le_bytes(state_bytes[4..8].try_into().map_err(|_| "--next-state-hex KDS4 tick field is malformed".to_string())?);
    if state_tick != expected_tick {
        return Err(format!("--next-state-hex KDS4 tick {state_tick} does not match expected successor tick {expected_tick}"));
    }
    let state_ticcmd = &state_bytes[KDS4_TICCMD_OFFSET..KDS4_TICCMD_OFFSET + 8];
    if state_ticcmd != expected_ticcmd {
        return Err(format!(
            "--next-state-hex KDS4 ticcmd {} does not match successor ticcmd {}",
            bytes_to_hex(state_ticcmd),
            bytes_to_hex(expected_ticcmd)
        ));
    }
    Ok(())
}

pub fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

pub fn classify_rpc_rejection(message: &str) -> &'static str {
    if message.contains("invalid opcode") || message.contains("Opcode<0xcb>") {
        "endpoint_missing_toccata_covenant_opcodes"
    } else if message.contains("has covenant field but transaction version is 0") {
        "transaction_version_missing_toccata"
    } else if message.contains("signature script size") {
        "sigscript_standardness_limit"
    } else if message.contains("element size") || message.contains("exceeds max allowed size 520") {
        "redeem_script_push_limit"
    } else if message.contains("push encoding is not minimal") {
        "non_minimal_push"
    } else if message.contains("storage mass") {
        "storage_mass_limit"
    } else if message.contains("orphan") {
        "orphan_or_chained_spend_policy"
    } else {
        "unknown"
    }
}

pub fn synthetic_outpoint(prev_tick: u32, index: u32) -> TransactionOutpoint {
    TransactionOutpoint::new(TransactionId::from_bytes([(prev_tick + 1) as u8; 32]), index)
}

pub fn synthetic_ticcmd_for_tick(tick: u32) -> Vec<u8> {
    ticcmd_for_tick(tick)
}

pub fn kds4_state_for_tick(tick: u32) -> Vec<u8> {
    state_chunk_for_tick(tick)
}

pub fn kds4_state_for_tick_and_ticcmd(tick: u32, ticcmd: &[u8]) -> Vec<u8> {
    state_chunk_for_tick_and_ticcmd(tick, ticcmd)
}

pub fn compile_initial_doom_state() -> Result<CompiledContract<'static>, String> {
    compile_doom_state(doom_constructor_args())
}

pub fn initial_doom_state_address() -> Result<kaspa_addresses::Address, String> {
    let initial = compile_initial_doom_state()?;
    let script_public_key = pay_to_script_hash_script(&initial.script);
    extract_script_pub_key_address(&script_public_key, Prefix::Testnet)
        .map_err(|err| format!("failed to derive initial DoomState address: {err}"))
}

fn doom_constructor_args() -> Vec<Expr<'static>> {
    let genesis_ticcmd = vec![0x00; 8];
    let genesis_state = state_chunk_for_tick_and_ticcmd(0, &genesis_ticcmd);
    vec![
        Expr::bytes(vec![0x11; 32]),
        Expr::bytes(session_key_hash()),
        Expr::int(0),
        Expr::bytes(blake2b_bytes(&genesis_state)),
        Expr::int(genesis_state.len() as i64),
        Expr::bytes(genesis_ticcmd),
    ]
}

fn doom_state_args(tick: i64, ticcmd: Vec<u8>, state_hash: Vec<u8>) -> Vec<Expr<'static>> {
    vec![
        Expr::bytes(vec![0x11; 32]),
        Expr::bytes(session_key_hash()),
        Expr::int(tick),
        Expr::bytes(state_hash),
        Expr::int(KDS4_STATE_LEN as i64),
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

pub fn ticcmd_for_tick(tick: u32) -> Vec<u8> {
    let mut ticcmd = vec![0u8; 8];
    ticcmd[0] = (tick & 0xff) as u8;
    ticcmd[1] = ((tick >> 8) & 0xff) as u8;
    ticcmd
}

pub fn state_chunk_for_tick(tick: u32) -> Vec<u8> {
    state_chunk_for_tick_and_ticcmd(tick, &ticcmd_for_tick(tick))
}

pub fn state_chunk_for_tick_and_ticcmd(tick: u32, ticcmd: &[u8]) -> Vec<u8> {
    let mut state = vec![0u8; KDS4_STATE_LEN];
    state[0..4].copy_from_slice(b"KDS4");
    state[4..8].copy_from_slice(&tick.to_le_bytes());
    state[8..12].copy_from_slice(&(tick * 35).to_le_bytes());
    state[12] = tick.wrapping_mul(11) as u8;
    state[13] = tick.wrapping_mul(13) as u8;
    state[14] = 0;
    state[15] = 1;
    state[16..20].copy_from_slice(&(tick + 3).to_le_bytes());
    state[20..24].copy_from_slice(&1u32.to_le_bytes());
    state[24..28].copy_from_slice(&(tick + 1).to_le_bytes());
    for (index, byte) in state[28..60].iter_mut().enumerate() {
        *byte = tick.wrapping_mul(17).wrapping_add(index as u32) as u8;
    }
    state[60..64].copy_from_slice(&(tick + 2).to_le_bytes());
    state[64..68].copy_from_slice(&(tick + 10).to_le_bytes());
    state[68..72].copy_from_slice(&(tick + 20).to_le_bytes());
    state[72..76].copy_from_slice(&(tick + 30).to_le_bytes());
    state[76..80].copy_from_slice(&(tick + 40).to_le_bytes());
    state[80..84].copy_from_slice(&(tick + 50).to_le_bytes());
    state[84..88].copy_from_slice(&(tick + 60).to_le_bytes());
    state[KDS4_TICCMD_OFFSET..KDS4_TICCMD_OFFSET + 8].copy_from_slice(ticcmd);
    state
}

fn state_hash_for_tick(tick: u32) -> Vec<u8> {
    blake2b_bytes(&state_chunk_for_tick(tick))
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

fn tx_input(outpoint: TransactionOutpoint, signature_script: Vec<u8>) -> TransactionInput {
    TransactionInput::new_with_compute_budget(outpoint, signature_script, 0, 1_000)
}

fn covenant_output(compiled: &CompiledContract<'_>, authorizing_input: u16, covenant_id: Hash, value: u64) -> TransactionOutput {
    TransactionOutput {
        value,
        script_public_key: pay_to_script_hash_script(&compiled.script),
        covenant: Some(CovenantBinding { authorizing_input, covenant_id }),
    }
}

fn covenant_utxo(compiled: &CompiledContract<'_>, covenant_id: Hash, value: u64) -> UtxoEntry {
    UtxoEntry::new(value, pay_to_script_hash_script(&compiled.script), 0, false, Some(covenant_id))
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
