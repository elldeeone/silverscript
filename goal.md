# Goal: Doom State Over Kaspa TN10 Covenants

Build a Doom-on-Kaspa prototype where the authoritative Doom game state is committed to Kaspa Testnet-10 Toccata covenants and advances one on-chain state transition per game tic. The target is the strongest practical version of the concept, not a limited checkpoint-only demo.

The prototype should fork a browser-capable open-source Doom port, preferably `cloudflare/doom-wasm`, because it already runs Chocolate Doom in WebAssembly and exposes a deterministic tic-based game loop. The browser client should handle rendering, controls, audio, WAD loading, local simulation, and user experience. Kaspa should hold the canonical game-state path.

Use this TN10 Toccata-funded wallet address as the current funded test wallet target:

`kaspatest:qzma22j09zrjn5zxw8mx3epm49etfma9y9jc6z80g43mwyk0svvg6h34ars7t`

The earlier `kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz` wallet is funded on a non-Toccata TN10 view but is empty on the synced public Toccata endpoint, so it is not the active deployment wallet for covenant testing.

The wallet flow should let the user connect, import, or use a TN10 testnet wallet, verify that it has enough test KAS, create a game session key or signing flow suitable for high-frequency transactions, and avoid committing wallet recovery words or private keys into source code, logs, or config.

The on-chain model should create a covenant-backed game-state UTXO when a game starts. Each authoritative game tic should attempt to spend the current game-state UTXO and create the next game-state UTXO.

Desired transition shape:

```text
current_game_utxo + old_doom_state + player_input/ticcmd -> next_game_utxo + next_doom_state
```

The project should push toward one TN10 covenant transaction per Doom tic, targeting up to 10 authoritative tics/sec if TN10 propagation, mempool policy, chained spends, wallet signing, and block inclusion make it possible. If the network cannot sustain that cadence, the implementation should measure the bottleneck clearly and preserve the best achievable fallback path without redefining the goal.

The Doom state should be deterministic and resumable. The system should serialize or commit enough Doom state each tic to make the Kaspa UTXO chain the canonical source of truth. Ideally, the full compact Doom state is stored on-chain. If that is too large at first, the system may start with a state hash plus compact summary while measuring what full-state storage requires.

SilverScript covenants should enforce game-state progression as far as practical. Early validation can enforce game id, monotonic tick increments, successor output shape, authorized session key, state layout, and cheap state invariants. Over time, validation should move toward stronger Doom transition checks or a proof/challenge model.

Final intended outcome:

- Doom runs in-browser from an open-source WASM-capable port.
- A TN10 testnet wallet funds and authorizes the game.
- Starting a game deploys a covenant-backed Doom state UTXO.
- Each authoritative tic attempts to commit the next Doom state on-chain.
- The client follows the latest accepted UTXO as canonical state.
- The game can resume from committed Kaspa state.
- The system measures real TN10 cadence, latency, failure modes, and maximum sustainable tic rate.
- The implementation is structured to keep pushing toward the pure version: Doom state over Kaspa, one covenant transition per tic, with progressive validation rather than merely periodic checkpointing.
