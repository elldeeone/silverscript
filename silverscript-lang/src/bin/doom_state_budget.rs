use clap::Parser;

const KDS4_STATE_BYTES: usize = 96;
const VANILLA_SAVEGAME_BYTES: usize = 0x2c000;
const DEFAULT_SCRIPT_ELEMENT_BYTES: usize = 520;

#[derive(Debug, Parser)]
#[command(
    name = "doom-state-budget",
    about = "Estimate Doom state bandwidth and script-element chunking for TN10 tic commitments",
    next_line_help = true
)]
struct Cli {
    /// Authoritative tic rate to model.
    #[arg(long = "target-tps", default_value_t = 1.0)]
    target_tps: f64,

    /// Compact state bytes committed per tic.
    #[arg(long = "state-bytes", default_value_t = KDS4_STATE_BYTES)]
    state_bytes: usize,

    /// Candidate full serialized Doom state bytes per tic.
    #[arg(long = "full-state-bytes", default_value_t = VANILLA_SAVEGAME_BYTES)]
    full_state_bytes: usize,

    /// Maximum bytes carried in one script element.
    #[arg(long = "chunk-bytes", default_value_t = DEFAULT_SCRIPT_ELEMENT_BYTES)]
    chunk_bytes: usize,

    /// Number of full-state chunks carried by one chunk-commitment transition.
    #[arg(long = "chunks-per-transition", default_value_t = 1)]
    chunks_per_transition: usize,

    /// TN10 target block rate.
    #[arg(long = "kaspa-bps", default_value_t = 10.0)]
    kaspa_bps: f64,
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
    if cli.kaspa_bps <= 0.0 {
        return Err("--kaspa-bps must be greater than zero".to_string());
    }
    if cli.state_bytes == 0 {
        return Err("--state-bytes must be greater than zero".to_string());
    }
    if cli.full_state_bytes == 0 {
        return Err("--full-state-bytes must be greater than zero".to_string());
    }
    if cli.chunk_bytes == 0 {
        return Err("--chunk-bytes must be greater than zero".to_string());
    }
    if cli.chunks_per_transition == 0 {
        return Err("--chunks-per-transition must be greater than zero".to_string());
    }

    let compact = Budget::new(cli.state_bytes, cli.target_tps, cli.chunk_bytes);
    let full = Budget::new(cli.full_state_bytes, cli.target_tps, cli.chunk_bytes);
    let tx_per_block = cli.target_tps / cli.kaspa_bps;
    let chunk_plan = ChunkPlan::new(full.chunks_per_tic, cli.chunks_per_transition, cli.target_tps, cli.kaspa_bps);

    println!("mode=doom-state-budget");
    println!("target_tps={:.4}", cli.target_tps);
    println!("kaspa_bps={:.4}", cli.kaspa_bps);
    println!("target_txs_per_block={tx_per_block:.4}");
    println!("chunk_bytes={}", cli.chunk_bytes);
    println!("chunks_per_transition={}", cli.chunks_per_transition);
    print_budget("compact", &compact);
    print_budget("full", &full);
    println!("full_vs_compact_bytes_ratio={:.4}", cli.full_state_bytes as f64 / cli.state_bytes as f64);
    println!("chunked_transitions_per_tic={}", chunk_plan.transitions_per_tic);
    println!("chunked_transition_tps={:.4}", chunk_plan.transition_tps);
    println!("chunked_transitions_per_block={:.4}", chunk_plan.transitions_per_block);
    println!("chunked_seconds_per_full_tic_at_kaspa_bps={:.4}", chunk_plan.seconds_per_full_tic_at_kaspa_bps);
    println!("one_tx_per_tic_full_state_feasible={}", cli.full_state_bytes <= cli.chunk_bytes);
    println!(
        "next_required_step={}",
        if cli.full_state_bytes <= cli.chunk_bytes {
            "test direct full-state commits in one covenant transition"
        } else if chunk_plan.transitions_per_block <= 1.0 {
            "prototype a chunked full-state commitment path and measure whether the slower full-state cadence is usable"
        } else {
            "use compact per-tic KDS4 on-chain and design full-state chunks as slower checkpoints, deltas, or challenge data"
        }
    );
    Ok(())
}

struct ChunkPlan {
    transitions_per_tic: usize,
    transition_tps: f64,
    transitions_per_block: f64,
    seconds_per_full_tic_at_kaspa_bps: f64,
}

impl ChunkPlan {
    fn new(chunks_per_tic: usize, chunks_per_transition: usize, target_tps: f64, kaspa_bps: f64) -> Self {
        let transitions_per_tic = chunks_per_tic.div_ceil(chunks_per_transition);
        let transition_tps = transitions_per_tic as f64 * target_tps;
        Self {
            transitions_per_tic,
            transition_tps,
            transitions_per_block: transition_tps / kaspa_bps,
            seconds_per_full_tic_at_kaspa_bps: transitions_per_tic as f64 / kaspa_bps,
        }
    }
}

struct Budget {
    bytes_per_tic: usize,
    chunks_per_tic: usize,
    bytes_per_second: f64,
    bytes_per_minute: f64,
}

impl Budget {
    fn new(bytes_per_tic: usize, target_tps: f64, chunk_bytes: usize) -> Self {
        let chunks_per_tic = bytes_per_tic.div_ceil(chunk_bytes);
        let bytes_per_second = bytes_per_tic as f64 * target_tps;
        Self { bytes_per_tic, chunks_per_tic, bytes_per_second, bytes_per_minute: bytes_per_second * 60.0 }
    }
}

fn print_budget(prefix: &str, budget: &Budget) {
    println!("{prefix}_state_bytes_per_tic={}", budget.bytes_per_tic);
    println!("{prefix}_chunks_per_tic={}", budget.chunks_per_tic);
    println!("{prefix}_state_bytes_per_second={:.4}", budget.bytes_per_second);
    println!("{prefix}_state_bytes_per_minute={:.4}", budget.bytes_per_minute);
}
