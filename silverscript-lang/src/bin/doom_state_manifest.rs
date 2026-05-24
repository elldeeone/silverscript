use std::fs;
use std::path::PathBuf;

use blake2b_simd::Params as Blake2bParams;
use clap::Parser;
use serde::Serialize;

const DEFAULT_CHUNK_BYTES: usize = 520;

#[derive(Debug, Parser)]
#[command(
    name = "doom-state-manifest",
    about = "Chunk serialized Doom state and compute a deterministic checkpoint manifest root",
    next_line_help = true
)]
struct Cli {
    /// Serialized Doom state bytes to chunk. If omitted, --synthetic-bytes is used.
    #[arg(long = "input")]
    input: Option<PathBuf>,

    /// Generate deterministic synthetic bytes of this length for planning/tests.
    #[arg(long = "synthetic-bytes")]
    synthetic_bytes: Option<usize>,

    /// Canonical tic represented by this full-state checkpoint.
    #[arg(long = "tick")]
    tick: u32,

    /// Maximum payload bytes in each state chunk.
    #[arg(long = "chunk-bytes", default_value_t = DEFAULT_CHUNK_BYTES)]
    chunk_bytes: usize,

    /// Emit JSON instead of text key=value lines.
    #[arg(long = "emit-json", default_value_t = false)]
    emit_json: bool,

    /// Emit and verify a Merkle proof for this chunk index.
    #[arg(long = "proof-index")]
    proof_index: Option<usize>,

    /// Verify a proof against this committed manifest root instead of generating a manifest.
    #[arg(long = "verify-root-hex")]
    verify_root_hex: Option<String>,

    /// Leaf hash being proven when using --verify-root-hex.
    #[arg(long = "verify-leaf-hash-hex")]
    verify_leaf_hash_hex: Option<String>,

    /// Total serialized state bytes represented by the manifest when using --verify-root-hex.
    #[arg(long = "verify-state-bytes")]
    verify_state_bytes: Option<usize>,

    /// Total chunk count represented by the manifest when using --verify-root-hex.
    #[arg(long = "verify-chunk-count")]
    verify_chunk_count: Option<usize>,

    /// Repeated sibling hashes from leaf to root when using --verify-root-hex.
    #[arg(long = "proof-sibling-hex")]
    proof_sibling_hex: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    mode: &'static str,
    tick: u32,
    state_bytes: usize,
    chunk_bytes: usize,
    chunk_count: usize,
    state_hash_hex: String,
    manifest_root_hex: String,
    chunk_hashes_hex: Vec<String>,
    last_chunk_bytes: usize,
    proof: Option<ChunkProof>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChunkProof {
    index: usize,
    chunk_bytes: usize,
    leaf_hash_hex: String,
    siblings_hex: Vec<String>,
    verified: bool,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    if cli.chunk_bytes == 0 {
        return Err("--chunk-bytes must be greater than zero".to_string());
    }
    if cli.verify_root_hex.is_some() {
        return run_verify(&cli);
    }
    if cli.input.is_some() == cli.synthetic_bytes.is_some() {
        return Err("provide exactly one of --input or --synthetic-bytes".to_string());
    }

    let state = match (cli.input.as_ref(), cli.synthetic_bytes) {
        (Some(path), None) => fs::read(path).map_err(|err| format!("failed to read {}: {err}", path.display()))?,
        (None, Some(len)) => synthetic_state(len),
        _ => unreachable!("validated exactly one state source"),
    };
    if state.is_empty() {
        return Err("state bytes must not be empty".to_string());
    }

    let manifest = build_manifest(cli.tick, cli.chunk_bytes, &state, cli.proof_index)?;
    if cli.emit_json {
        let body = serde_json::to_string_pretty(&manifest).map_err(|err| format!("failed to encode manifest JSON: {err}"))?;
        println!("{body}");
    } else {
        print_manifest_text(&manifest);
    }
    Ok(())
}

fn run_verify(cli: &Cli) -> Result<(), String> {
    if cli.input.is_some() || cli.synthetic_bytes.is_some() {
        return Err("--verify-root-hex mode does not accept --input or --synthetic-bytes".to_string());
    }
    let proof_index = cli.proof_index.ok_or("--verify-root-hex requires --proof-index")?;
    let state_bytes = cli.verify_state_bytes.ok_or("--verify-root-hex requires --verify-state-bytes")?;
    let chunk_count = cli.verify_chunk_count.ok_or("--verify-root-hex requires --verify-chunk-count")?;
    if state_bytes == 0 {
        return Err("--verify-state-bytes must be greater than zero".to_string());
    }
    if chunk_count == 0 {
        return Err("--verify-chunk-count must be greater than zero".to_string());
    }
    if proof_index >= chunk_count {
        return Err(format!("--proof-index {proof_index} out of range for {chunk_count} chunks"));
    }
    let root = parse_fixed_hex(cli.verify_root_hex.as_deref().expect("verify root checked"), 32, "--verify-root-hex")?;
    let leaf = parse_fixed_hex(
        cli.verify_leaf_hash_hex.as_deref().ok_or("--verify-root-hex requires --verify-leaf-hash-hex")?,
        32,
        "--verify-leaf-hash-hex",
    )?;
    let siblings =
        cli.proof_sibling_hex.iter().map(|hex| parse_fixed_hex(hex, 32, "--proof-sibling-hex")).collect::<Result<Vec<_>, _>>()?;
    let verified = verify_proof(cli.tick, state_bytes, cli.chunk_bytes, chunk_count, proof_index, &leaf, &siblings, &root);
    println!("mode=doom-state-proof-verify");
    println!("tick={}", cli.tick);
    println!("state_bytes={state_bytes}");
    println!("chunk_bytes={}", cli.chunk_bytes);
    println!("chunk_count={chunk_count}");
    println!("proof_index={proof_index}");
    println!("proof_sibling_count={}", siblings.len());
    println!("proof_verified={verified}");
    if verified { Ok(()) } else { Err("proof did not verify against manifest root".to_string()) }
}

fn synthetic_state(len: usize) -> Vec<u8> {
    (0..len).map(|idx| ((idx * 31 + 17) & 0xff) as u8).collect()
}

fn build_manifest(tick: u32, chunk_bytes: usize, state: &[u8], proof_index: Option<usize>) -> Result<Manifest, String> {
    let chunks = state.chunks(chunk_bytes).collect::<Vec<_>>();
    let chunk_hashes = chunks.iter().enumerate().map(|(index, chunk)| chunk_hash(tick, index, chunk)).collect::<Vec<_>>();
    let root = manifest_root(tick, state.len(), chunk_bytes, &chunk_hashes);
    let proof =
        proof_index.map(|index| build_proof(tick, state.len(), chunk_bytes, &chunks, &chunk_hashes, &root, index)).transpose()?;
    Ok(Manifest {
        mode: "doom-state-manifest",
        tick,
        state_bytes: state.len(),
        chunk_bytes,
        chunk_count: chunks.len(),
        state_hash_hex: bytes_to_hex(&hash_with_domain(b"KASPA_DOOM_STATE_V1", &[state])),
        manifest_root_hex: bytes_to_hex(&root),
        chunk_hashes_hex: chunk_hashes.iter().map(|hash| bytes_to_hex(hash)).collect(),
        last_chunk_bytes: chunks.last().map(|chunk| chunk.len()).unwrap_or(0),
        proof,
    })
}

fn chunk_hash(tick: u32, index: usize, chunk: &[u8]) -> Vec<u8> {
    hash_with_domain(
        b"KASPA_DOOM_STATE_CHUNK_V1",
        &[&tick.to_le_bytes(), &(index as u64).to_le_bytes(), &(chunk.len() as u64).to_le_bytes(), chunk],
    )
}

fn manifest_root(tick: u32, state_len: usize, chunk_bytes: usize, chunk_hashes: &[Vec<u8>]) -> Vec<u8> {
    let mut layer = chunk_hashes.to_vec();
    while layer.len() > 1 {
        let mut next = Vec::with_capacity(layer.len().div_ceil(2));
        for pair in layer.chunks(2) {
            let right = pair.get(1).unwrap_or(&pair[0]);
            next.push(hash_with_domain(b"KASPA_DOOM_STATE_NODE_V1", &[&pair[0], right]));
        }
        layer = next;
    }
    hash_with_domain(
        b"KASPA_DOOM_STATE_ROOT_V1",
        &[
            &tick.to_le_bytes(),
            &(state_len as u64).to_le_bytes(),
            &(chunk_bytes as u64).to_le_bytes(),
            &(chunk_hashes.len() as u64).to_le_bytes(),
            &layer[0],
        ],
    )
}

fn build_proof(
    tick: u32,
    state_len: usize,
    chunk_bytes: usize,
    chunks: &[&[u8]],
    chunk_hashes: &[Vec<u8>],
    root: &[u8],
    index: usize,
) -> Result<ChunkProof, String> {
    let chunk = chunks.get(index).ok_or_else(|| format!("--proof-index {index} out of range for {} chunks", chunks.len()))?;
    let siblings = proof_siblings(chunk_hashes, index);
    let leaf_hash = chunk_hashes[index].clone();
    let verified = verify_proof(tick, state_len, chunk_bytes, chunk_hashes.len(), index, &leaf_hash, &siblings, root);
    Ok(ChunkProof {
        index,
        chunk_bytes: chunk.len(),
        leaf_hash_hex: bytes_to_hex(&leaf_hash),
        siblings_hex: siblings.iter().map(|hash| bytes_to_hex(hash)).collect(),
        verified,
    })
}

fn proof_siblings(chunk_hashes: &[Vec<u8>], mut index: usize) -> Vec<Vec<u8>> {
    let mut layer = chunk_hashes.to_vec();
    let mut siblings = Vec::new();
    while layer.len() > 1 {
        let sibling_index = if index % 2 == 0 { index + 1 } else { index - 1 };
        siblings.push(layer.get(sibling_index).unwrap_or(&layer[index]).clone());
        let mut next = Vec::with_capacity(layer.len().div_ceil(2));
        for pair in layer.chunks(2) {
            let right = pair.get(1).unwrap_or(&pair[0]);
            next.push(hash_with_domain(b"KASPA_DOOM_STATE_NODE_V1", &[&pair[0], right]));
        }
        index /= 2;
        layer = next;
    }
    siblings
}

fn verify_proof(
    tick: u32,
    state_len: usize,
    chunk_bytes: usize,
    chunk_count: usize,
    mut index: usize,
    leaf_hash: &[u8],
    siblings: &[Vec<u8>],
    root: &[u8],
) -> bool {
    let mut acc = leaf_hash.to_vec();
    for sibling in siblings {
        acc = if index % 2 == 0 {
            hash_with_domain(b"KASPA_DOOM_STATE_NODE_V1", &[&acc, sibling])
        } else {
            hash_with_domain(b"KASPA_DOOM_STATE_NODE_V1", &[sibling, &acc])
        };
        index /= 2;
    }
    let computed_root = hash_with_domain(
        b"KASPA_DOOM_STATE_ROOT_V1",
        &[
            &tick.to_le_bytes(),
            &(state_len as u64).to_le_bytes(),
            &(chunk_bytes as u64).to_le_bytes(),
            &(chunk_count as u64).to_le_bytes(),
            &acc,
        ],
    );
    computed_root == root
}

fn hash_with_domain(domain: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    let mut state = Blake2bParams::new().hash_length(32).to_state();
    state.update(&(domain.len() as u64).to_le_bytes());
    state.update(domain);
    for part in parts {
        state.update(&(part.len() as u64).to_le_bytes());
        state.update(part);
    }
    state.finalize().as_bytes().to_vec()
}

fn print_manifest_text(manifest: &Manifest) {
    println!("mode={}", manifest.mode);
    println!("tick={}", manifest.tick);
    println!("state_bytes={}", manifest.state_bytes);
    println!("chunk_bytes={}", manifest.chunk_bytes);
    println!("chunk_count={}", manifest.chunk_count);
    println!("last_chunk_bytes={}", manifest.last_chunk_bytes);
    println!("state_hash_hex={}", manifest.state_hash_hex);
    println!("manifest_root_hex={}", manifest.manifest_root_hex);
    for (idx, hash) in manifest.chunk_hashes_hex.iter().enumerate() {
        println!("chunk_hash[{idx}]={hash}");
    }
    if let Some(proof) = &manifest.proof {
        println!("proof_index={}", proof.index);
        println!("proof_chunk_bytes={}", proof.chunk_bytes);
        println!("proof_leaf_hash_hex={}", proof.leaf_hash_hex);
        println!("proof_sibling_count={}", proof.siblings_hex.len());
        for (idx, sibling) in proof.siblings_hex.iter().enumerate() {
            println!("proof_sibling[{idx}]={sibling}");
        }
        println!("proof_verified={}", proof.verified);
    }
    println!("next_required_step=commit manifest_root_hex from a per-tic KDS4 state or slower checkpoint covenant path");
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_fixed_hex(hex: &str, expected_bytes: usize, name: &str) -> Result<Vec<u8>, String> {
    let hex = hex.trim();
    let expected_len = expected_bytes * 2;
    if hex.len() != expected_len {
        return Err(format!("{name} must be exactly {expected_len} hex chars, got {}", hex.len()));
    }
    if hex.len() % 2 != 0 {
        return Err(format!("{name} must have an even number of hex chars"));
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for idx in (0..hex.len()).step_by(2) {
        out.push(
            u8::from_str_radix(&hex[idx..idx + 2], 16).map_err(|err| format!("{name} has invalid hex at byte {}: {err}", idx / 2))?,
        );
    }
    Ok(out)
}
