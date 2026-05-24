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
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

const COV_DOOM: Hash = Hash::from_bytes([0xdd; 32]);
const KDS4_STATE_LEN: usize = 96;

fn doom_state_source() -> String {
    let path = format!("{}/tests/apps/doom/doom_state.sil", env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(&path).unwrap_or_else(|err| panic!("failed to read {path}: {err}"))
}

fn doom_constructor_args() -> Vec<Expr<'static>> {
    let genesis_ticcmd = vec![0x00; 8];
    let genesis_state = kds4_state_bytes(0, &genesis_ticcmd);
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

fn compile_doom_state(args: Vec<Expr<'static>>) -> CompiledContract<'static> {
    let source = doom_state_source();
    compile_contract(Box::leak(source.into_boxed_str()), &args, CompileOptions::default()).expect("DoomState compiles")
}

fn blake2b_bytes(data: &[u8]) -> Vec<u8> {
    blake2b_simd::Params::new().hash_length(32).hash(data).as_bytes().to_vec()
}

fn hex_bytes(hex: &str) -> Vec<u8> {
    assert_eq!(hex.len() % 2, 0, "test hex must have even length");
    (0..hex.len()).step_by(2).map(|idx| u8::from_str_radix(&hex[idx..idx + 2], 16).expect("valid test hex")).collect()
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn blake2b_hex(bytes: &[u8]) -> String {
    bytes_to_hex(&blake2b_bytes(bytes))
}

fn kds4_state_bytes(tick: u32, ticcmd: &[u8]) -> Vec<u8> {
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
    bytes[88..96].copy_from_slice(ticcmd);
    bytes
}

fn wait_for_bridge_state(listen: &str, timeout: Duration) -> Result<serde_json::Value, String> {
    let started = Instant::now();
    loop {
        match get_bridge_state(listen) {
            Ok(state) => return Ok(state),
            Err(err) if started.elapsed() >= timeout => return Err(err),
            Err(_) => sleep(Duration::from_millis(25)),
        }
    }
}

fn get_bridge_state(listen: &str) -> Result<serde_json::Value, String> {
    let mut stream = TcpStream::connect(listen).map_err(|err| format!("connect {listen}: {err}"))?;
    stream
        .write_all(b"GET /state HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .map_err(|err| format!("write /state request: {err}"))?;
    let mut response = String::new();
    stream.read_to_string(&mut response).map_err(|err| format!("read /state response: {err}"))?;
    let (headers, body) = response.split_once("\r\n\r\n").ok_or_else(|| format!("malformed /state response: {response}"))?;
    if !headers.starts_with("HTTP/1.1 200") {
        return Err(format!("unexpected /state response: {response}"));
    }
    serde_json::from_str(body).map_err(|err| format!("parse /state JSON {body:?}: {err}"))
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

fn script_op_counts(script: &[u8]) -> (usize, usize) {
    let mut instruction_count = 0;
    let mut charged_op_count = 0;

    for opcode in parse_script::<PopulatedTransaction<'static>, SigHashReusedValuesUnsync>(script) {
        let opcode = opcode.expect("compiled script should parse");
        instruction_count += 1;
        if !opcode.is_push_opcode() {
            charged_op_count += 1;
        }
    }

    (instruction_count, charged_op_count)
}

fn push_redeem_script(script: &[u8]) -> Vec<u8> {
    ScriptBuilder::new().add_data(script).expect("push redeem script").drain()
}

fn tx_input(index: u32, signature_script: Vec<u8>) -> TransactionInput {
    TransactionInput::new_with_compute_budget(
        TransactionOutpoint { transaction_id: TransactionId::from_bytes([index as u8 + 1; 32]), index },
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

fn doom_advance_sigscript(compiled: &CompiledContract<'_>, next_ticcmd: Vec<u8>, next_state: Expr<'_>, signature: Vec<u8>) -> Vec<u8> {
    let mut sigscript = compiled
        .build_sig_script_for_covenant_decl(
            "advance",
            vec![Expr::bytes(next_ticcmd), next_state, Expr::bytes(session_pubkey()), Expr::bytes(signature)],
            CovenantDeclCallOptions { is_leader: false },
        )
        .expect("per-tic covenant sigscript builds");
    sigscript.extend_from_slice(&push_redeem_script(&compiled.script));
    sigscript
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

#[test]
fn doom_state_covenant_compiles_and_builds_per_tic_sigscript() {
    let source = doom_state_source();
    let compiled = compile_contract(&source, &doom_constructor_args(), CompileOptions::default()).expect("DoomState compiles");
    let (instruction_count, charged_op_count) = script_op_counts(&compiled.script);

    println!("doom_state.sil {} / {} / {}", compiled.script.len(), instruction_count, charged_op_count);

    let next_ticcmd = vec![1, 0, 0, 0, 0, 0, 0, 0];
    let next_state = doom_state_expr(1, next_ticcmd.clone(), kds4_state_bytes(1, &next_ticcmd));
    let sigscript = compiled
        .build_sig_script_for_covenant_decl(
            "advance",
            vec![Expr::bytes(next_ticcmd), next_state, Expr::bytes(session_pubkey()), Expr::bytes(vec![0; 65])],
            CovenantDeclCallOptions { is_leader: false },
        )
        .expect("per-tic covenant sigscript builds");

    assert!(!compiled.script.is_empty());
    assert!(!sigscript.is_empty());
    assert!(compiled.script.len() < 20_000, "DoomState script should remain far below TN10 sigscript cap");
    assert!(charged_op_count < 10_000, "DoomState should leave room for stronger validation");
}

#[test]
fn doom_state_covenant_executes_local_chained_tic_transition() {
    let active = compile_doom_state(doom_constructor_args());
    let next_ticcmd = vec![1, 0, 0, 0, 0, 0, 0, 0];
    let next_state_chunk = kds4_state_bytes(1, &next_ticcmd);
    let next_state = doom_state_expr(1, next_ticcmd.clone(), next_state_chunk.clone());
    let next = compile_doom_state(doom_state_args(1, next_ticcmd.clone(), blake2b_bytes(&next_state_chunk)));

    let placeholder_sigscript = doom_advance_sigscript(&active, next_ticcmd.clone(), next_state.clone(), vec![0; 65]);
    let outputs = vec![covenant_output(&next, 0, COV_DOOM)];
    let entries = vec![covenant_utxo(&active, COV_DOOM)];
    let mut tx = Transaction::new(1, vec![tx_input(0, placeholder_sigscript)], outputs, 0, Default::default(), 0, vec![]);
    let sig = sign_tx_input_schnorr(&tx, &entries, 0);
    tx.inputs[0].signature_script = doom_advance_sigscript(&active, next_ticcmd, next_state, sig);

    let result = execute_input_with_covenants(tx, entries, 0);
    assert!(result.is_ok(), "DoomState tic transition should execute locally: {}", result.unwrap_err());
}

#[test]
fn doom_state_covenant_rejects_non_kds4_state_length() {
    let active = compile_doom_state(doom_constructor_args());
    let next_ticcmd = vec![1, 0, 0, 0, 0, 0, 0, 0];
    let short_state_chunk = vec![0xbb; 32];
    let next_state = doom_state_expr(1, next_ticcmd.clone(), short_state_chunk.clone());
    let next = compile_doom_state(doom_state_args(1, next_ticcmd.clone(), blake2b_bytes(&short_state_chunk)));

    let placeholder_sigscript = doom_advance_sigscript(&active, next_ticcmd.clone(), next_state.clone(), vec![0; 65]);
    let outputs = vec![covenant_output(&next, 0, COV_DOOM)];
    let entries = vec![covenant_utxo(&active, COV_DOOM)];
    let mut tx = Transaction::new(1, vec![tx_input(0, placeholder_sigscript)], outputs, 0, Default::default(), 0, vec![]);
    let sig = sign_tx_input_schnorr(&tx, &entries, 0);
    tx.inputs[0].signature_script = doom_advance_sigscript(&active, next_ticcmd, next_state, sig);

    let result = execute_input_with_covenants(tx, entries, 0);
    assert!(result.is_err(), "DoomState transition should reject non-96-byte state length");
}

#[test]
fn doom_submitter_commits_explicit_state_bytes() {
    let ticcmd_hex = "0102030405060708";
    let ticcmd = hex_bytes(ticcmd_hex);
    let state_bytes = kds4_state_bytes(1, &ticcmd);
    let state_bytes_hex = bytes_to_hex(&state_bytes);
    let state_hash = blake2b_hex(&state_bytes);
    let output = Command::new(env!("CARGO_BIN_EXE_doom_tn10_submitter"))
        .args(["--ticks", "1", "--next-ticcmd-hex", ticcmd_hex, "--next-state-hex", &state_bytes_hex])
        .output()
        .expect("run doom_tn10_submitter");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "submitter failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("next_ticcmd_hex=0102030405060708"), "{stdout}");
    assert!(stdout.contains("next_state_len=96"), "{stdout}");
    assert!(stdout.contains(&format!("next_state_hash={state_hash}")), "{stdout}");
    assert!(stdout.contains(&format!("next_state_hex={state_bytes_hex}")), "{stdout}");
    assert!(stdout.contains("local_validation=ok"), "{stdout}");
    assert!(stdout.contains("rpc_submit=skipped"), "{stdout}");
}

#[test]
fn doom_submitter_writes_bridge_state_for_resume() {
    let path = std::env::temp_dir().join(format!("doom-tn10-submitter-state-test-{}.json", std::process::id()));
    let ticcmd_hex = "0102030405060708";
    let ticcmd = hex_bytes(ticcmd_hex);
    let state_bytes = kds4_state_bytes(1, &ticcmd);
    let state_bytes_hex = bytes_to_hex(&state_bytes);
    let state_hash = blake2b_hex(&state_bytes);
    let output = Command::new(env!("CARGO_BIN_EXE_doom_tn10_submitter"))
        .args([
            "--ticks",
            "1",
            "--next-ticcmd-hex",
            ticcmd_hex,
            "--next-state-hex",
            &state_bytes_hex,
            "--wallet-address",
            "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz",
            "--write-bridge-state",
            path.to_str().expect("utf8 temp path"),
        ])
        .output()
        .expect("run doom_tn10_submitter");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "submitter failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("bridge_state_written=true"), "{stdout}");

    let state = fs::read_to_string(&path).expect("read bridge state");
    let _ = fs::remove_file(&path);
    let state: serde_json::Value = serde_json::from_str(&state).expect("parse bridge state");
    assert_eq!(state["prev_tick"], 1);
    assert_eq!(state["started"], true);
    assert_eq!(state["wallet_address"], "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz");
    assert_eq!(state["prev_ticcmd_hex"], ticcmd_hex);
    assert_eq!(state["prev_state_hash_hex"], state_hash);
    assert_eq!(state["prev_state_hex"], state_bytes_hex);
}

#[test]
fn doom_bridge_loads_submitter_written_bridge_state() {
    let pid = std::process::id();
    let state_file = std::env::temp_dir().join(format!("doom-tn10-submitter-bridge-state-test-{pid}.json"));
    let event_log = std::env::temp_dir().join(format!("doom-tn10-submitter-bridge-events-test-{pid}.jsonl"));
    let listen = format!("127.0.0.1:{}", 18_000 + (pid % 20_000));
    let ticcmd_hex = "0102030405060708";
    let ticcmd = hex_bytes(ticcmd_hex);
    let state_bytes = kds4_state_bytes(1, &ticcmd);
    let state_bytes_hex = bytes_to_hex(&state_bytes);
    let state_hash = blake2b_hex(&state_bytes);

    let output = Command::new(env!("CARGO_BIN_EXE_doom_tn10_submitter"))
        .args([
            "--ticks",
            "1",
            "--next-ticcmd-hex",
            ticcmd_hex,
            "--next-state-hex",
            &state_bytes_hex,
            "--wallet-address",
            "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz",
            "--write-bridge-state",
            state_file.to_str().expect("utf8 temp path"),
        ])
        .output()
        .expect("run doom_tn10_submitter");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "submitter failed\nstdout:\n{stdout}\nstderr:\n{stderr}");

    let mut bridge = Command::new(env!("CARGO_BIN_EXE_doom_tn10_bridge"))
        .args([
            "--listen",
            &listen,
            "--submit",
            "false",
            "--submit-backend",
            "in-process",
            "--state-file",
            state_file.to_str().expect("utf8 temp path"),
            "--event-log",
            event_log.to_str().expect("utf8 temp path"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn doom_tn10_bridge");

    let state = match wait_for_bridge_state(&listen, Duration::from_secs(2)) {
        Ok(state) => state,
        Err(err) => {
            let _ = bridge.kill();
            let output = bridge.wait_with_output().expect("bridge output");
            panic!(
                "bridge did not load submitter-written state: {err}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    };

    assert_eq!(state["canonicalTick"], 1);
    assert_eq!(state["started"], true);
    assert_eq!(state["walletAddress"], "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz");
    assert_eq!(state["ticcmdHex"], ticcmd_hex);
    assert_eq!(state["stateHash"], state_hash);
    assert_eq!(state["stateBytesHex"], state_bytes_hex);

    let _ = bridge.kill();
    let _ = bridge.wait();
    let _ = fs::remove_file(&state_file);
    let _ = fs::remove_file(&event_log);
}

#[test]
fn doom_state_budget_reports_full_savegame_chunking() {
    let output =
        Command::new(env!("CARGO_BIN_EXE_doom_state_budget")).args(["--target-tps", "10"]).output().expect("run doom_state_budget");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "budget failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("compact_state_bytes_per_tic=96"), "{stdout}");
    assert!(stdout.contains("compact_state_bytes_per_second=960.0000"), "{stdout}");
    assert!(stdout.contains("full_state_bytes_per_tic=180224"), "{stdout}");
    assert!(stdout.contains("full_chunks_per_tic=347"), "{stdout}");
    assert!(stdout.contains("chunked_transitions_per_tic=347"), "{stdout}");
    assert!(stdout.contains("chunked_transition_tps=3470.0000"), "{stdout}");
    assert!(stdout.contains("chunked_transitions_per_block=347.0000"), "{stdout}");
    assert!(stdout.contains("chunked_seconds_per_full_tic_at_kaspa_bps=34.7000"), "{stdout}");
    assert!(stdout.contains("one_tx_per_tic_full_state_feasible=false"), "{stdout}");
    assert!(stdout.contains("compact per-tic KDS4"), "{stdout}");
}

#[test]
fn doom_state_budget_models_multi_chunk_transitions() {
    let output = Command::new(env!("CARGO_BIN_EXE_doom_state_budget"))
        .args(["--target-tps", "1", "--chunks-per-transition", "16"])
        .output()
        .expect("run doom_state_budget");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "budget failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("chunks_per_transition=16"), "{stdout}");
    assert!(stdout.contains("chunked_transitions_per_tic=22"), "{stdout}");
    assert!(stdout.contains("chunked_transition_tps=22.0000"), "{stdout}");
    assert!(stdout.contains("chunked_transitions_per_block=2.2000"), "{stdout}");
}

#[test]
fn doom_state_manifest_chunks_and_roots_state_bytes() {
    let output = Command::new(env!("CARGO_BIN_EXE_doom_state_manifest"))
        .args(["--tick", "7", "--synthetic-bytes", "1100", "--chunk-bytes", "520"])
        .output()
        .expect("run doom_state_manifest");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "manifest failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("mode=doom-state-manifest"), "{stdout}");
    assert!(stdout.contains("tick=7"), "{stdout}");
    assert!(stdout.contains("state_bytes=1100"), "{stdout}");
    assert!(stdout.contains("chunk_bytes=520"), "{stdout}");
    assert!(stdout.contains("chunk_count=3"), "{stdout}");
    assert!(stdout.contains("last_chunk_bytes=60"), "{stdout}");
    assert!(stdout.contains("state_hash_hex=049f8c528b1ec5a87d9cba1d842e416ceba0e6fb8f4a08c8925627930df1953d"), "{stdout}");
    assert!(stdout.contains("manifest_root_hex=5d97d02f80d1445c56204aafa2cccc3d51a18735eb999d0e8b5a342eab396b25"), "{stdout}");
    assert!(stdout.contains("chunk_hash[0]=d7e659d5093b30b424cd7f12bec236e0ea8251b470ca871948d22391120a606d"), "{stdout}");
    assert!(stdout.contains("chunk_hash[2]=8f8f8e0931e693d8945a0bdb49020272a8f49015c5fcd5744a660811f39fab45"), "{stdout}");
}

#[test]
fn doom_state_manifest_emits_verified_chunk_proof() {
    let output = Command::new(env!("CARGO_BIN_EXE_doom_state_manifest"))
        .args(["--tick", "7", "--synthetic-bytes", "1100", "--chunk-bytes", "520", "--proof-index", "2"])
        .output()
        .expect("run doom_state_manifest");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "manifest failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("proof_index=2"), "{stdout}");
    assert!(stdout.contains("proof_chunk_bytes=60"), "{stdout}");
    assert!(stdout.contains("proof_leaf_hash_hex=8f8f8e0931e693d8945a0bdb49020272a8f49015c5fcd5744a660811f39fab45"), "{stdout}");
    assert!(stdout.contains("proof_sibling_count=2"), "{stdout}");
    assert!(stdout.contains("proof_sibling[1]=8269a9d9b9a37ee4500d8bb94fc0963616dc6c59707424204a22d978b67cd2ef"), "{stdout}");
    assert!(stdout.contains("proof_verified=true"), "{stdout}");
}

#[test]
fn doom_state_manifest_verifies_chunk_proof_without_state_bytes() {
    let output = Command::new(env!("CARGO_BIN_EXE_doom_state_manifest"))
        .args([
            "--tick",
            "7",
            "--chunk-bytes",
            "520",
            "--verify-root-hex",
            "5d97d02f80d1445c56204aafa2cccc3d51a18735eb999d0e8b5a342eab396b25",
            "--verify-leaf-hash-hex",
            "8f8f8e0931e693d8945a0bdb49020272a8f49015c5fcd5744a660811f39fab45",
            "--verify-state-bytes",
            "1100",
            "--verify-chunk-count",
            "3",
            "--proof-index",
            "2",
            "--proof-sibling-hex",
            "8f8f8e0931e693d8945a0bdb49020272a8f49015c5fcd5744a660811f39fab45",
            "--proof-sibling-hex",
            "8269a9d9b9a37ee4500d8bb94fc0963616dc6c59707424204a22d978b67cd2ef",
        ])
        .output()
        .expect("run doom_state_manifest verifier");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "manifest verifier failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("mode=doom-state-proof-verify"), "{stdout}");
    assert!(stdout.contains("proof_verified=true"), "{stdout}");
}

#[test]
fn doom_state_manifest_rejects_bad_chunk_proof() {
    let output = Command::new(env!("CARGO_BIN_EXE_doom_state_manifest"))
        .args([
            "--tick",
            "7",
            "--chunk-bytes",
            "520",
            "--verify-root-hex",
            "5d97d02f80d1445c56204aafa2cccc3d51a18735eb999d0e8b5a342eab396b25",
            "--verify-leaf-hash-hex",
            "8f8f8e0931e693d8945a0bdb49020272a8f49015c5fcd5744a660811f39fab45",
            "--verify-state-bytes",
            "1100",
            "--verify-chunk-count",
            "3",
            "--proof-index",
            "1",
            "--proof-sibling-hex",
            "8f8f8e0931e693d8945a0bdb49020272a8f49015c5fcd5744a660811f39fab45",
            "--proof-sibling-hex",
            "8269a9d9b9a37ee4500d8bb94fc0963616dc6c59707424204a22d978b67cd2ef",
        ])
        .output()
        .expect("run doom_state_manifest verifier");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "bad proof unexpectedly verified\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("proof_verified=false"), "{stdout}");
    assert!(stderr.contains("proof did not verify"), "stderr:\n{stderr}");
}

#[test]
fn doom_state_manifest_emits_json() {
    let output = Command::new(env!("CARGO_BIN_EXE_doom_state_manifest"))
        .args(["--tick", "2", "--synthetic-bytes", "8", "--chunk-bytes", "4", "--proof-index", "1", "--emit-json"])
        .output()
        .expect("run doom_state_manifest");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "manifest failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("parse manifest JSON");
    assert_eq!(json["mode"], "doom-state-manifest");
    assert_eq!(json["tick"], 2);
    assert_eq!(json["stateBytes"], 8);
    assert_eq!(json["chunkBytes"], 4);
    assert_eq!(json["chunkCount"], 2);
    assert_eq!(json["chunkHashesHex"].as_array().expect("chunk hash array").len(), 2);
    assert_eq!(json["proof"]["index"], 1);
    assert_eq!(json["proof"]["verified"], true);
}

#[test]
fn doom_submitter_rejects_malformed_explicit_state_bytes() {
    let output = Command::new(env!("CARGO_BIN_EXE_doom_tn10_submitter"))
        .args([
            "--ticks",
            "1",
            "--next-ticcmd-hex",
            "0102030405060708",
            "--next-state-hex",
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        ])
        .output()
        .expect("run doom_tn10_submitter");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "submitter unexpectedly succeeded\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stderr.contains("--next-state-hex must be a 96-byte KDS4 snapshot"), "stderr:\n{stderr}");
}

#[test]
fn doom_submitter_rejects_explicit_state_with_wrong_ticcmd() {
    let ticcmd_hex = "0102030405060708";
    let mut state_bytes = kds4_state_bytes(1, &hex_bytes(ticcmd_hex));
    state_bytes[88] ^= 0x80;
    let state_bytes_hex = bytes_to_hex(&state_bytes);
    let output = Command::new(env!("CARGO_BIN_EXE_doom_tn10_submitter"))
        .args(["--ticks", "1", "--next-ticcmd-hex", ticcmd_hex, "--next-state-hex", &state_bytes_hex])
        .output()
        .expect("run doom_tn10_submitter");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "submitter unexpectedly succeeded\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stderr.contains("--next-state-hex KDS4 ticcmd"), "stderr:\n{stderr}");
}

#[test]
fn doom_report_summarizes_dry_run_resume_tuple() {
    let path = std::env::temp_dir().join(format!("doom-tn10-report-test-{}.jsonl", std::process::id()));
    let ticcmd_hex = "0102030405060708";
    let state_bytes = kds4_state_bytes(1, &hex_bytes(ticcmd_hex));
    let state_bytes_hex = bytes_to_hex(&state_bytes);
    let state_hash = blake2b_hex(&state_bytes);
    let event = serde_json::json!({
        "status": "accepted",
        "browserTick": 1,
        "canonicalTick": 1,
        "txId": "35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c",
        "successorOutpoint": "35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c:0",
        "ticcmdHex": ticcmd_hex,
        "stateHash": state_hash,
        "stateBytesHex": state_bytes_hex,
        "currentUtxoValueSompi": 5000000000u64,
        "feeSompi": 20000000u64,
        "successorUtxoValueSompi": 4980000000u64,
        "covenantId": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        "submitElapsedMs": 28.15,
        "capturedAt": "2026-05-23T12:45:00.000Z",
        "walletAddress": "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz",
        "rpcSubmit": "skipped",
        "mempoolSeen": null,
        "mempoolIsOrphan": null,
        "inclusionSeen": null
    });
    fs::write(&path, format!("{event}\n")).expect("write report event log");

    let output = Command::new(env!("CARGO_BIN_EXE_doom_tn10_report"))
        .args(["--event-log", path.to_str().expect("utf8 temp path"), "--target-tps", "10"])
        .output()
        .expect("run doom_tn10_report");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = fs::remove_file(&path);

    assert!(output.status.success(), "report failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("accepted_events=1"), "{stdout}");
    assert!(stdout.contains("rpc_submit_skipped=1"), "{stdout}");
    assert!(stdout.contains("resume_tuple_prev_tick=1"), "{stdout}");
    assert!(
        stdout.contains("resume_tuple_input_outpoint=35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c:0"),
        "{stdout}"
    );
    assert!(stdout.contains("resume_tuple_prev_ticcmd_hex=0102030405060708"), "{stdout}");
    assert!(stdout.contains(&format!("resume_tuple_prev_state_hash_hex={state_hash}")), "{stdout}");
    assert!(stdout.contains(&format!("resume_tuple_prev_state_bytes_hex={state_bytes_hex}")), "{stdout}");
    assert!(stdout.contains("resume_tuple_covenant_id=dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "bridge_resume_command=cargo run -p silverscript-lang --bin doom_tn10_bridge -- --input-txid 35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c --input-index 0 --wallet-address kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz --prev-tick 1 --prev-ticcmd-hex 0102030405060708 --prev-state-hash-hex {state_hash} --prev-state-hex {state_bytes_hex} --utxo-value 4980000000 --covenant-id dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
        )),
        "{stdout}"
    );
    assert!(stdout.contains("state_bytes_present_count=1"), "{stdout}");
    assert!(stdout.contains("latest_kds4_tick=1"), "{stdout}");
    assert!(stdout.contains("latest_kds4_level_time=35"), "{stdout}");
    assert!(stdout.contains("latest_kds4_special_count=3"), "{stdout}");
    assert!(stdout.contains("latest_kds4_sector_count=11"), "{stdout}");
    assert!(stdout.contains("latest_kds4_line_count=21"), "{stdout}");
    assert!(stdout.contains("latest_kds4_side_count=31"), "{stdout}");
    assert!(stdout.contains("latest_kds4_total_kills=41"), "{stdout}");
    assert!(stdout.contains("latest_kds4_total_items=51"), "{stdout}");
    assert!(stdout.contains("latest_kds4_total_secrets=61"), "{stdout}");
    assert!(stdout.contains("state_hash_verified_count=1"), "{stdout}");
    assert!(stdout.contains("state_snapshot_verified_count=1"), "{stdout}");
    assert!(stdout.contains("accepted_txid_verified_count=1"), "{stdout}");
    assert!(stdout.contains("accepted_outpoint_link_verified_count=0"), "{stdout}");
}

#[test]
fn doom_report_summarizes_checkpoint_manifest_root() {
    let path = std::env::temp_dir().join(format!("doom-tn10-report-checkpoint-test-{}.jsonl", std::process::id()));
    let ticcmd_hex = "0102030405060708";
    let state_bytes = kds4_state_bytes(1, &hex_bytes(ticcmd_hex));
    let state_bytes_hex = bytes_to_hex(&state_bytes);
    let state_hash = blake2b_hex(&state_bytes);
    let checkpoint_root = "5d97d02f80d1445c56204aafa2cccc3d51a18735eb999d0e8b5a342eab396b25";
    let event = serde_json::json!({
        "status": "accepted",
        "browserTick": 1,
        "canonicalTick": 1,
        "txId": "35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c",
        "successorOutpoint": "35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c:0",
        "ticcmdHex": ticcmd_hex,
        "stateHash": state_hash,
        "stateBytesHex": state_bytes_hex,
        "checkpointManifestRootHex": checkpoint_root,
        "checkpointStateBytes": 1100,
        "checkpointChunkCount": 3,
        "submitElapsedMs": 28.15,
        "capturedAt": "2026-05-23T12:45:00.000Z",
        "walletAddress": "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz",
        "rpcSubmit": "skipped"
    });
    fs::write(&path, format!("{event}\n")).expect("write report event log");

    let output = Command::new(env!("CARGO_BIN_EXE_doom_tn10_report"))
        .args(["--event-log", path.to_str().expect("utf8 temp path"), "--target-tps", "1"])
        .output()
        .expect("run doom_tn10_report");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = fs::remove_file(&path);

    assert!(output.status.success(), "report failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("checkpoint_count=1"), "{stdout}");
    assert!(stdout.contains("checkpoint_verified_count=1"), "{stdout}");
    assert!(stdout.contains(&format!("latest_checkpoint_manifest_root_hex={checkpoint_root}")), "{stdout}");
    assert!(stdout.contains("latest_checkpoint_state_bytes=1100"), "{stdout}");
    assert!(stdout.contains("latest_checkpoint_chunk_count=3"), "{stdout}");
}

#[test]
fn doom_report_rejects_bad_checkpoint_manifest_root() {
    let path = std::env::temp_dir().join(format!("doom-tn10-report-bad-checkpoint-test-{}.jsonl", std::process::id()));
    let ticcmd_hex = "0102030405060708";
    let state_bytes = kds4_state_bytes(1, &hex_bytes(ticcmd_hex));
    let state_bytes_hex = bytes_to_hex(&state_bytes);
    let state_hash = blake2b_hex(&state_bytes);
    let event = serde_json::json!({
        "status": "accepted",
        "browserTick": 1,
        "canonicalTick": 1,
        "txId": "35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c",
        "successorOutpoint": "35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c:0",
        "ticcmdHex": ticcmd_hex,
        "stateHash": state_hash,
        "stateBytesHex": state_bytes_hex,
        "checkpointManifestRootHex": "abcd",
        "checkpointStateBytes": 1100,
        "checkpointChunkCount": 3,
        "submitElapsedMs": 28.15,
        "capturedAt": "2026-05-23T12:45:00.000Z",
        "walletAddress": "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz",
        "rpcSubmit": "skipped"
    });
    fs::write(&path, format!("{event}\n")).expect("write report event log");

    let output = Command::new(env!("CARGO_BIN_EXE_doom_tn10_report"))
        .args(["--event-log", path.to_str().expect("utf8 temp path"), "--target-tps", "1"])
        .output()
        .expect("run doom_tn10_report");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = fs::remove_file(&path);

    assert!(!output.status.success(), "bad checkpoint unexpectedly passed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stderr.contains("checkpointManifestRootHex must be exactly 64 hex chars"), "stderr:\n{stderr}");
}

#[test]
fn doom_report_writes_bridge_state_file_from_latest_accepted_event() {
    let event_log = std::env::temp_dir().join(format!("doom-tn10-report-state-test-{}.jsonl", std::process::id()));
    let state_file = std::env::temp_dir().join(format!("doom-tn10-report-state-test-{}.json", std::process::id()));
    let ticcmd_hex = "0102030405060708";
    let state_bytes = kds4_state_bytes(1, &hex_bytes(ticcmd_hex));
    let state_bytes_hex = bytes_to_hex(&state_bytes);
    let state_hash = blake2b_hex(&state_bytes);
    let event = serde_json::json!({
        "status": "accepted",
        "browserTick": 1,
        "canonicalTick": 1,
        "txId": "35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c",
        "successorOutpoint": "35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c:0",
        "ticcmdHex": ticcmd_hex,
        "stateHash": state_hash,
        "stateBytesHex": state_bytes_hex,
        "currentUtxoValueSompi": 5000000000u64,
        "feeSompi": 20000000u64,
        "successorUtxoValueSompi": 4980000000u64,
        "covenantId": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        "submitElapsedMs": 28.15,
        "capturedAt": "2026-05-23T12:45:00.000Z",
        "walletAddress": "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz",
        "rpcSubmit": "skipped"
    });
    fs::write(&event_log, format!("{event}\n")).expect("write report event log");

    let output = Command::new(env!("CARGO_BIN_EXE_doom_tn10_report"))
        .args([
            "--event-log",
            event_log.to_str().expect("utf8 temp path"),
            "--target-tps",
            "1",
            "--write-bridge-state",
            state_file.to_str().expect("utf8 temp path"),
        ])
        .output()
        .expect("run doom_tn10_report");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "report failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("latest_current_utxo_value_sompi=5000000000"), "{stdout}");
    assert!(stdout.contains("latest_successor_utxo_value_sompi=4980000000"), "{stdout}");
    assert!(stdout.contains("latest_fee_sompi=20000000"), "{stdout}");
    assert!(stdout.contains("bridge_state_written=true"), "{stdout}");
    let state_json = fs::read_to_string(&state_file).expect("read written bridge state");
    let _ = fs::remove_file(&event_log);
    let _ = fs::remove_file(&state_file);
    let state: serde_json::Value = serde_json::from_str(&state_json).expect("parse bridge state json");
    assert_eq!(state["input_txid"], "35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c");
    assert_eq!(state["input_index"], 0);
    assert_eq!(state["utxo_value"], 4980000000u64);
    assert_eq!(state["prev_tick"], 1);
    assert_eq!(state["prev_ticcmd_hex"], ticcmd_hex);
    assert_eq!(state["prev_state_hash_hex"], state_hash);
    assert_eq!(state["prev_state_hex"], state_bytes_hex);
    assert_eq!(state["covenant_id"], "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd");
    assert_eq!(state["wallet_address"], "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz");
}

#[test]
fn doom_report_emits_json_summary() {
    let path = std::env::temp_dir().join(format!("doom-tn10-report-json-test-{}.jsonl", std::process::id()));
    let ticcmd_hex = "0102030405060708";
    let state_bytes = kds4_state_bytes(1, &hex_bytes(ticcmd_hex));
    let state_bytes_hex = bytes_to_hex(&state_bytes);
    let state_hash = blake2b_hex(&state_bytes);
    let event = serde_json::json!({
        "status": "accepted",
        "browserTick": 1,
        "canonicalTick": 1,
        "txId": "35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c",
        "successorOutpoint": "35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c:0",
        "ticcmdHex": ticcmd_hex,
        "stateHash": state_hash,
        "stateBytesHex": state_bytes_hex,
        "covenantId": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        "submitElapsedMs": 28.15,
        "capturedAt": "2026-05-23T12:45:00.000Z",
        "walletAddress": "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz",
        "rpcSubmit": "skipped"
    });
    fs::write(&path, format!("{event}\n")).expect("write report event log");

    let output = Command::new(env!("CARGO_BIN_EXE_doom_tn10_report"))
        .args(["--event-log", path.to_str().expect("utf8 temp path"), "--target-tps", "1", "--emit-json"])
        .output()
        .expect("run doom_tn10_report");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = fs::remove_file(&path);

    assert!(output.status.success(), "report failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    let summary: serde_json::Value = serde_json::from_str(&stdout).expect("parse JSON report summary");
    assert_eq!(summary["mode"], "doom-tn10-report");
    assert_eq!(summary["acceptedEvents"], 1);
    assert_eq!(summary["stateSnapshotVerifiedCount"], 1);
    assert_eq!(summary["latestStateBytesLen"], 96);
    assert_eq!(summary["stateBytesMinLen"], 96);
    assert_eq!(summary["stateBytesAvgLen"], 96.0);
    assert_eq!(summary["stateBytesMaxLen"], 96);
    assert_eq!(summary["targetStateBytesPerSecond"], 96.0);
    assert_eq!(summary["latestKds4"]["tick"], 1);
    assert_eq!(summary["latestKds4"]["levelTime"], 35);
    assert_eq!(summary["latestKds4"]["specialCount"], 3);
    assert_eq!(summary["latestKds4"]["sectorCount"], 11);
    assert_eq!(summary["latestKds4"]["lineCount"], 21);
    assert_eq!(summary["latestKds4"]["sideCount"], 31);
    assert_eq!(summary["latestKds4"]["totalKills"], 41);
    assert_eq!(summary["latestKds4"]["totalItems"], 51);
    assert_eq!(summary["latestKds4"]["totalSecrets"], 61);
    assert_eq!(summary["resumeTuplePrevTick"], 1);
    assert_eq!(summary["resumeTupleInputOutpoint"], "35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c:0");
    assert_eq!(summary["resumeTuplePrevStateBytesHex"], state_bytes_hex);
    assert_eq!(summary["rejectionCounts"], serde_json::json!({}));
    assert!(summary["bridgeResumeCommand"].as_str().expect("bridge resume command").contains("--prev-state-hex"));
    assert!(summary["bridgeResumeCommand"].as_str().expect("bridge resume command").contains("--wallet-address"));
}

#[test]
fn doom_report_verifies_accepted_outpoint_chain_links() {
    let path = std::env::temp_dir().join(format!("doom-tn10-report-link-test-{}.jsonl", std::process::id()));
    let first_txid = "1111111111111111111111111111111111111111111111111111111111111111";
    let second_txid = "2222222222222222222222222222222222222222222222222222222222222222";
    let first_ticcmd = "0100000003050781";
    let second_ticcmd = "02000000060a0e82";
    let first_state = kds4_state_bytes(1, &hex_bytes(first_ticcmd));
    let second_state = kds4_state_bytes(2, &hex_bytes(second_ticcmd));
    let first = serde_json::json!({
        "status": "accepted",
        "browserTick": 1,
        "canonicalTick": 1,
        "txId": first_txid,
        "successorOutpoint": format!("{first_txid}:0"),
        "ticcmdHex": first_ticcmd,
        "stateHash": blake2b_hex(&first_state),
        "stateBytesHex": bytes_to_hex(&first_state),
        "covenantId": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        "submitElapsedMs": 28.15,
        "capturedAt": "2026-05-23T12:45:00.000Z",
        "walletAddress": "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz",
        "canonicalOutpointBefore": "0000000000000000000000000000000000000000000000000000000000000001:0",
        "rpcSubmit": "skipped"
    });
    let second = serde_json::json!({
        "status": "accepted",
        "browserTick": 2,
        "canonicalTick": 2,
        "txId": second_txid,
        "successorOutpoint": format!("{second_txid}:0"),
        "ticcmdHex": second_ticcmd,
        "stateHash": blake2b_hex(&second_state),
        "stateBytesHex": bytes_to_hex(&second_state),
        "covenantId": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        "submitElapsedMs": 29.15,
        "capturedAt": "2026-05-23T12:45:01.000Z",
        "walletAddress": "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz",
        "canonicalOutpointBefore": format!("{first_txid}:0"),
        "rpcSubmit": "skipped"
    });
    fs::write(&path, format!("{first}\n{second}\n")).expect("write report event log");

    let output = Command::new(env!("CARGO_BIN_EXE_doom_tn10_report"))
        .args(["--event-log", path.to_str().expect("utf8 temp path"), "--target-tps", "1"])
        .output()
        .expect("run doom_tn10_report");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = fs::remove_file(&path);

    assert!(output.status.success(), "report failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("accepted_txid_verified_count=2"), "{stdout}");
    assert!(stdout.contains("accepted_outpoint_link_verified_count=1"), "{stdout}");
    assert!(stdout.contains("state_snapshot_verified_count=2"), "{stdout}");
}

#[test]
fn doom_report_rejects_broken_accepted_outpoint_link() {
    let path = std::env::temp_dir().join(format!("doom-tn10-report-bad-link-test-{}.jsonl", std::process::id()));
    let first_txid = "1111111111111111111111111111111111111111111111111111111111111111";
    let second_txid = "2222222222222222222222222222222222222222222222222222222222222222";
    let wrong_txid = "3333333333333333333333333333333333333333333333333333333333333333";
    let first_ticcmd = "0100000003050781";
    let second_ticcmd = "02000000060a0e82";
    let first_state = kds4_state_bytes(1, &hex_bytes(first_ticcmd));
    let second_state = kds4_state_bytes(2, &hex_bytes(second_ticcmd));
    let first = serde_json::json!({
        "status": "accepted",
        "browserTick": 1,
        "canonicalTick": 1,
        "txId": first_txid,
        "successorOutpoint": format!("{first_txid}:0"),
        "ticcmdHex": first_ticcmd,
        "stateHash": blake2b_hex(&first_state),
        "stateBytesHex": bytes_to_hex(&first_state),
        "submitElapsedMs": 28.15,
        "capturedAt": "2026-05-23T12:45:00.000Z",
        "walletAddress": "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz",
        "canonicalOutpointBefore": "0000000000000000000000000000000000000000000000000000000000000001:0",
        "rpcSubmit": "skipped"
    });
    let second = serde_json::json!({
        "status": "accepted",
        "browserTick": 2,
        "canonicalTick": 2,
        "txId": second_txid,
        "successorOutpoint": format!("{second_txid}:0"),
        "ticcmdHex": second_ticcmd,
        "stateHash": blake2b_hex(&second_state),
        "stateBytesHex": bytes_to_hex(&second_state),
        "submitElapsedMs": 29.15,
        "capturedAt": "2026-05-23T12:45:01.000Z",
        "walletAddress": "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz",
        "canonicalOutpointBefore": format!("{wrong_txid}:0"),
        "rpcSubmit": "skipped"
    });
    fs::write(&path, format!("{first}\n{second}\n")).expect("write report event log");

    let output = Command::new(env!("CARGO_BIN_EXE_doom_tn10_report"))
        .args(["--event-log", path.to_str().expect("utf8 temp path"), "--target-tps", "1"])
        .output()
        .expect("run doom_tn10_report");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = fs::remove_file(&path);

    assert!(!output.status.success(), "report unexpectedly succeeded\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stderr.contains("canonicalOutpointBefore"), "stderr:\n{stderr}");
    assert!(stderr.contains("previous successorOutpoint"), "stderr:\n{stderr}");
}

#[test]
fn doom_report_summarizes_rejected_tick_class() {
    let path = std::env::temp_dir().join(format!("doom-tn10-report-rejected-test-{}.jsonl", std::process::id()));
    let ticcmd_hex = "0102030405060708";
    let state_bytes = kds4_state_bytes(1, &hex_bytes(ticcmd_hex));
    let state_bytes_hex = bytes_to_hex(&state_bytes);
    let state_hash = blake2b_hex(&state_bytes);
    let accepted = serde_json::json!({
        "status": "accepted",
        "browserTick": 1,
        "canonicalTick": 1,
        "txId": "35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c",
        "successorOutpoint": "35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c:0",
        "ticcmdHex": ticcmd_hex,
        "stateHash": state_hash,
        "stateBytesHex": state_bytes_hex,
        "submitElapsedMs": 28.15,
        "capturedAt": "2026-05-23T12:45:00.000Z",
        "walletAddress": "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz",
        "rpcSubmit": "skipped"
    });
    let rejected = serde_json::json!({
        "status": "rejected",
        "browserTick": 3,
        "canonicalTick": 1,
        "txId": null,
        "successorOutpoint": null,
        "ticcmdHex": "03000000090f1583",
        "stateHash": null,
        "submitElapsedMs": 0.15,
        "capturedAt": "2026-05-23T12:45:01.000Z",
        "walletAddress": "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz",
        "rejectionClass": "canonical_tick_mismatch",
        "error": "browser canonical tick 3 does not match bridge next canonical tick 2"
    });
    let rejected_state = serde_json::json!({
        "status": "rejected",
        "browserTick": 2,
        "canonicalTick": 1,
        "txId": null,
        "successorOutpoint": null,
        "ticcmdHex": "02000000060a0e82",
        "stateHash": null,
        "submitElapsedMs": 0.12,
        "capturedAt": "2026-05-23T12:45:02.000Z",
        "walletAddress": "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz",
        "rejectionClass": "invalid_state_snapshot",
        "error": "stateBytes must be a 96-byte KDS4 snapshot"
    });
    fs::write(&path, format!("{accepted}\n{rejected}\n{rejected_state}\n")).expect("write report event log");

    let output = Command::new(env!("CARGO_BIN_EXE_doom_tn10_report"))
        .args(["--event-log", path.to_str().expect("utf8 temp path"), "--target-tps", "1"])
        .output()
        .expect("run doom_tn10_report");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = fs::remove_file(&path);

    assert!(output.status.success(), "report failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("accepted_events=1"), "{stdout}");
    assert!(stdout.contains("rejected_events=2"), "{stdout}");
    assert!(stdout.contains("rejection_count[canonical_tick_mismatch]=1"), "{stdout}");
    assert!(stdout.contains("rejection_count[invalid_state_snapshot]=1"), "{stdout}");
    assert!(stdout.contains("resume_tuple_prev_tick=1"), "{stdout}");
    assert!(stdout.contains(&format!("resume_tuple_prev_state_bytes_hex={state_bytes_hex}")), "{stdout}");
    assert!(stdout.contains("state_hash_verified_count=1"), "{stdout}");
    assert!(stdout.contains("state_snapshot_verified_count=1"), "{stdout}");
    assert!(stdout.contains("accepted_txid_verified_count=1"), "{stdout}");
}

#[test]
fn doom_report_rejects_state_bytes_hash_mismatch() {
    let path = std::env::temp_dir().join(format!("doom-tn10-report-bad-state-hash-test-{}.jsonl", std::process::id()));
    let event = serde_json::json!({
        "status": "accepted",
        "browserTick": 1,
        "canonicalTick": 1,
        "txId": "35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c",
        "successorOutpoint": "35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c:0",
        "ticcmdHex": "0102030405060708",
        "stateHash": "0000000000000000000000000000000000000000000000000000000000000000",
        "stateBytesHex": "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        "submitElapsedMs": 28.15,
        "capturedAt": "2026-05-23T12:45:00.000Z",
        "walletAddress": "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz",
        "rpcSubmit": "skipped"
    });
    fs::write(&path, format!("{event}\n")).expect("write report event log");

    let output = Command::new(env!("CARGO_BIN_EXE_doom_tn10_report"))
        .args(["--event-log", path.to_str().expect("utf8 temp path"), "--target-tps", "1"])
        .output()
        .expect("run doom_tn10_report");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = fs::remove_file(&path);

    assert!(!output.status.success(), "report unexpectedly succeeded\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stderr.contains("stateBytesHex hashes to"), "stderr:\n{stderr}");
}

#[test]
fn doom_report_rejects_state_bytes_wrong_kds4_ticcmd() {
    let path = std::env::temp_dir().join(format!("doom-tn10-report-bad-kds4-test-{}.jsonl", std::process::id()));
    let ticcmd_hex = "0102030405060708";
    let mut state_bytes = kds4_state_bytes(1, &hex_bytes(ticcmd_hex));
    state_bytes[88] ^= 0x80;
    let state_bytes_hex = bytes_to_hex(&state_bytes);
    let state_hash = blake2b_hex(&state_bytes);
    let event = serde_json::json!({
        "status": "accepted",
        "browserTick": 1,
        "canonicalTick": 1,
        "txId": "35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c",
        "successorOutpoint": "35abc2356846ee92c3f6815fbd4724c752ae9a61c6637fd71fee9fb25b8abe3c:0",
        "ticcmdHex": ticcmd_hex,
        "stateHash": state_hash,
        "stateBytesHex": state_bytes_hex,
        "submitElapsedMs": 28.15,
        "capturedAt": "2026-05-23T12:45:00.000Z",
        "walletAddress": "kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz",
        "rpcSubmit": "skipped"
    });
    fs::write(&path, format!("{event}\n")).expect("write report event log");

    let output = Command::new(env!("CARGO_BIN_EXE_doom_tn10_report"))
        .args(["--event-log", path.to_str().expect("utf8 temp path"), "--target-tps", "1"])
        .output()
        .expect("run doom_tn10_report");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = fs::remove_file(&path);

    assert!(!output.status.success(), "report unexpectedly succeeded\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stderr.contains("stateBytesHex KDS4 ticcmd"), "stderr:\n{stderr}");
}
