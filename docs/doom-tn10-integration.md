# Doom TN10 Integration Plan

This branch targets the `goal.md` pure-state model: one authoritative Doom tic attempts one Kaspa TN10 covenant state transition.

## Source Port

The browser Doom base is tracked at `vendor/doom-wasm`, which is Cloudflare's Chocolate Doom WebAssembly port. This gives the project a real browser-capable Doom source tree while keeping the Kaspa/SilverScript prototype in this repository.

Key engine boundary:

- Doom input is represented as `ticcmd_t`.
- The game loop advances by tics.
- The WebAssembly/browser layer already exists.
- The savegame/state serialization code gives us a starting point for canonical state extraction.

The first WASM-side bridge is:

`vendor/doom-wasm/src/kaspa_state_bridge.c`

`d_loop.c` calls this bridge immediately after each committed Doom tic. The bridge currently emits the active player's packed 8-byte `ticcmd_t` plus a 96-byte compact state snapshot through stdout and, under Emscripten, calls `Module.kaspaDoomOnTic(tic, ticcmdBytes, stateBytes)` when the browser shell defines it. This gives the browser/TN10 driver a deterministic hook for one attempted Kaspa state transition per canonical tic.

The live submit path now binds that emitted `ticcmd` and compact state snapshot into the actual successor covenant state. The browser bridge passes them as `--next-ticcmd-hex` and `--next-state-hex`, and the submitter prints the committed `next_ticcmd_hex`, `next_state_len`, and `next_state_hash` for the successor UTXO. Direct submitter calls now apply the same `KDS4` guard as the browser bridge: an explicit successor state must be exactly 96 bytes, start with the `KDS4` marker, embed the expected successor tick, and embed the exact successor `ticcmd`. Those fields, together with `prev_tick` and the successor outpoint, are the minimum resume tuple for the current hash-commitment prototype.

The first browser-side queue is in:

`vendor/doom-wasm/src/index.html`

It displays the TN10 wallet target, latest canonical tic, queued tics, and event log. `Module.kaspaDoomOnTic` converts the engine callback into a payload:

```js
{
  tick,
  targetTps: 1,
  walletAddress,
  canonicalOutpoint,
  ticcmd,
  stateBytes,
  capturedAt
}
```

The page records the raw Doom engine tic separately from the authoritative canonical tic. It rate-limits captures to the configured target cadence, defaulting to 1 authoritative tic/sec, and only queues one bridge submission at a time. Each captured payload uses a sequential canonical `tick` while the UI still displays the latest local engine tic. If a custom live submitter defines `window.kaspaDoomSubmitTic(payload)`, the page calls it for each captured canonical tic. Otherwise the page posts the queued tic payloads to a configurable local HTTP bridge, defaulting to:

```text
http://127.0.0.1:8787/tic
```

Before queueing a captured tic, the browser validates the WASM-emitted state snapshot locally:

- `ticcmd` is exactly 8 bytes.
- `stateBytes` is exactly 96 bytes.
- `stateBytes` starts with the `KDS4` marker.
- embedded committed tick equals the next authoritative canonical tick.
- embedded ticcmd equals the emitted payload ticcmd.

Malformed local snapshots increment the browser rejected counter and are not sent to the bridge. The bridge still repeats the same validation before transaction construction; browser validation is only an earlier diagnostic guard.

The bridge response is expected to be JSON. The browser consumes these optional fields:

```json
{
  "canonicalTick": 1,
  "successorOutpoint": "<txid>:0",
  "txId": "<txid>",
  "ticcmdHex": "0100000000000000",
  "stateHash": "<32-byte-hash>"
}
```

The queue is serialized with at most one in-flight submit. A failed submit normally remains at the head of the queue so the canonical chain is not advanced in the browser until the bridge reports acceptance. If the bridge returns a structured error with `canonicalTick` and `canonicalOutpoint`, the browser updates its local canonical tuple. It retries only when the queued payload is still the bridge's next canonical tick, such as an outpoint-only mismatch. If the bridge has moved to a different tick or rejects the embedded `KDS4` state bytes, the browser drops that queued snapshot and waits for a fresh Doom engine capture, because the committed tick and ticcmd inside `KDS4` bytes cannot be safely retargeted.

The browser now distinguishes:

- `engine tic`: latest local Doom engine tic observed from the WASM loop
- `canonical tic`: latest accepted bridge/Kaspa tic
- `canonical outpoint`: latest accepted DoomState UTXO
- `state hash`: latest accepted state hash commitment
- `rejected tics`: submit failures seen by the browser

The browser menu now persists its wallet address, local bridge URL, and authoritative target TPS under `localStorage` key `kaspa-doom-config`. The target TPS control is bounded to 1-10, matching the current 1 TPS practical target and the longer 10 TPS measurement target. Changing wallet, bridge URL, or target TPS resets readiness to `unchecked`, stores the new config, and triggers a fresh `/ready` probe before the start button can rely on the old result.

The accepted browser tuple is also mirrored in `localStorage` under `kaspa-doom-canonical-state`, so page reloads keep the current canonical outpoint/state hash in the client. On startup the browser also calls the local bridge's state endpoint derived from the submit URL:

```text
GET http://127.0.0.1:8787/state
```

The bridge state response is:

```json
{
  "canonicalTick": 1,
  "canonicalOutpoint": "<txid>:0",
  "inputTxid": "<txid>",
  "inputIndex": 0,
  "ticcmdHex": "0102030405060708",
  "stateHash": "<32-byte-hash>",
  "stateBytesHex": "<96-byte-KDS4-snapshot-hex>",
  "submit": false,
  "bridgeUrl": null
}
```

This makes the bridge state file the local canonical source of truth after a page reload or browser restart, with `localStorage` acting only as the browser-side cache. The bridge now persists the latest compact `KDS4` state bytes as `prev_state_hex` and returns them from `/state` as `stateBytesHex`, so a resumed local client has the actual compact state commitment bytes as well as the hash.

The local bridge binary is:

`silverscript-lang/src/bin/doom_tn10_bridge.rs`

It serves the browser endpoint and shells to the already-built `doom_tn10_submitter` binary for each tic. This is not the final 10 TPS architecture, but it connects the browser queue to the current live driver without putting wallet recovery words or private keys in browser JavaScript.

For every accepted browser tic, the bridge updates its in-memory canonical tuple:

```text
prev_tick
input_txid:input_index
wallet_address
prev_ticcmd_hex
prev_state_hash_hex
prev_state_hex
```

If restarted after at least one accepted tic, pass `--prev-ticcmd-hex` and `--prev-state-hash-hex` with the current outpoint so the next spend compiles against the exact previous DoomState script rather than a synthetic placeholder state. Pass `--prev-state-hex` as well when hydrating from a report tuple so `/state`, browser cache recovery, and resume validation keep the actual compact `KDS4` bytes.

The bridge now persists this tuple automatically:

```text
.doom-tn10-bridge-state.json
.doom-tn10-bridge-events.jsonl
```

The state file stores the latest canonical outpoint, tick, committed ticcmd, state hash, and compact `KDS4` state bytes. On startup, the bridge validates non-genesis resume files before serving: the persisted state bytes must be a `KDS4` snapshot whose embedded tick and ticcmd match the persisted tuple, and whose Blake2b hash matches `prev_state_hash_hex`. The event log is append-only JSONL for cadence and failure-mode analysis. Both files are ignored by git.

The bridge can also hydrate the same tuple directly from CLI args:

```bash
cargo run -p silverscript-lang --bin doom_tn10_bridge -- \
  --submit false \
  --input-txid <resume-txid> \
  --input-index <resume-index> \
  --wallet-address <started-wallet-address> \
  --prev-tick <resume_tuple_prev_tick> \
  --prev-ticcmd-hex <resume_tuple_prev_ticcmd_hex> \
  --prev-state-hash-hex <resume_tuple_prev_state_hash_hex> \
  --prev-state-hex <resume_tuple_prev_state_bytes_hex>
```

The report tool can now materialize that tuple directly into a bridge state file:

```bash
cargo run -p silverscript-lang --bin doom_tn10_report -- \
  --event-log .doom-tn10-bridge-events.jsonl \
  --write-bridge-state .doom-tn10-bridge-state.json
```

This writes the same JSON shape that `doom_tn10_bridge --state-file ...` loads:

```json
{
  "input_txid": "<latest-successor-txid>",
  "input_index": 0,
  "prev_tick": 5,
  "prev_ticcmd_hex": "<latest-ticcmd-hex>",
  "prev_state_hash_hex": "<latest-state-hash>",
  "prev_state_hex": "<latest-KDS4-state-bytes>",
  "covenant_id": "<doom-state-covenant-id>",
  "wallet_address": "<started-wallet-address>"
}
```

This removes manual tuple transcription from the resume flow after a live or dry-run event log has been captured. The report verifies that each accepted event's `stateBytesHex` hashes back to `stateHash`, is a 96-byte `KDS4` snapshot, embeds the accepted canonical tick, and embeds the event `ticcmdHex` before it writes the bridge state file.

The end-to-end smoke runner is:

`silverscript-lang/src/bin/doom_tn10_smoke.rs`

It launches a real `doom_tn10_bridge` subprocess, posts `/start`, submits deterministic browser-shaped `/tic` payloads, optionally kills and restarts the bridge from the persisted state file, and verifies the JSONL event log. By default it launches the sibling `doom_tn10_bridge` binary next to the smoke binary. Use `--bridge-bin <path>` to test a specific bridge executable. Unless `--allow-stale-bridge-bin` is passed, the smoke runner refuses to launch a bridge executable older than `src/bin/doom_tn10_bridge.rs`, so bridge-source edits cannot accidentally be tested against a stale already-built binary.

Verified `/state` behavior in bridge dry-run mode:

```text
GET /state before tic -> canonicalTick=0, canonicalOutpoint=0101...0101:0
POST /tic -> canonicalTick=1, successorOutpoint=04155715030700233e79c80ff7e887ccf16051ddf643ee6bc82ae0d5056aaccd:0
GET /state after tic -> canonicalTick=1, stateHash=8a3d747b75288ea08fa8eb5608de7ca1699b10adebe992f67f047243af780bf4
browser_state_sync_check=ok
```

Accepted event records include:

```json
{
  "status": "accepted",
  "browserTick": 1,
  "canonicalTick": 1,
  "successorOutpoint": "<txid>:0",
  "ticcmdHex": "0100000000000000",
  "stateHash": "<32-byte-hash>",
  "stateBytesHex": "<96-byte-KDS4-snapshot-hex>",
  "canonicalOutpointBefore": "<previous-txid>:0",
  "rpcSubmit": "ok",
  "mempoolSeen": true,
  "mempoolIsOrphan": false,
  "mempoolSeenElapsedMs": 123.4,
  "inclusionSeen": true,
  "inclusionSeenElapsedMs": 456.7,
  "acceptingBlockHash": "<block-hash>"
}
```

Rejected event records keep the canonical state unchanged and include a rejection class:

```json
{
  "status": "rejected",
  "browserTick": 2,
  "canonicalTick": 1,
  "canonicalOutpointBefore": "<current-txid>:0",
  "rejectionClass": "canonical_outpoint_mismatch",
  "childOutput": ["<submitter stdout/stderr lines>"]
}
```

HTTP error responses also return the bridge's full current canonical tuple:

```json
{
  "error": "<reason>",
  "canonicalTick": 1,
  "canonicalOutpoint": "<current-txid>:0",
  "ticcmdHex": "<current-ticcmd-hex>",
  "stateHash": "<current-state-hash>",
  "stateBytesHex": "<current-KDS4-state-bytes>",
  "covenantId": "<current-covenant-id>"
}
```

The browser consumes this tuple on rejected submits, updating its cached tic, outpoint, ticcmd, state hash, and state bytes before retrying the queued payload. This keeps browser recovery aligned with bridge state after a stale/skipped submit rather than only retargeting the outpoint.

The bridge rejects browser payloads before transaction construction if a canonical coordinate is stale/skipped or the state snapshot is malformed:

- `canonical_outpoint_mismatch`: browser outpoint does not equal the bridge's current canonical outpoint.
- `canonical_tick_mismatch`: browser canonical `tick` does not equal `bridge_prev_tick + 1`.
- `invalid_ticcmd`: browser `ticcmd` is not exactly 8 bytes.
- `invalid_state_snapshot`: browser `stateBytes` is missing, is not exactly 96 bytes, does not start with the `KDS4` marker, has an embedded committed tick different from the next canonical tick, or has an embedded ticcmd different from the payload ticcmd.

This keeps the append-only event log aligned with the actual chain of spends: accepted events form a monotonic canonical sequence, skipped/stale browser tics are visible as explicit rejected events rather than silently advancing the covenant chain with the wrong UI state, and accepted transitions always commit an engine-supplied `KDS4` state snapshot rather than submitter-synthesized placeholder bytes.

The HTTP response for rejected `/tic` requests is JSON too, so the browser can keep the failed tic at the head of its queue without guessing the canonical state.

Run it after building the submitter:

```bash
cargo run -p silverscript-lang --bin doom_tn10_bridge -- \
  --url ws://10.0.3.26:17210 \
  --input-txid 44958358cd848ef43c91112738ef8be5ffed574982e152be98913eeb94b726c0 \
  --input-index 0 \
  --utxo-value 100000000 \
  --covenant-id b33892ffcac705a87ce7a747cf8e289c7d48d87cecf1469731f5374a13625fb0 \
  --prev-tick 0 \
  --wait-preflight \
  --preflight-timeout-ms 0 \
  --preflight-poll-ms 10000 \
  --track-mempool true \
  --track-inclusion true
```

Bridge limits:

- It serializes one tic at a time.
- It inherits the submitter's TN10 endpoint and node-sync requirements.
- It can still shell once per tic through `--submit-backend child`, preserving the original correctness path through `doom_tn10_submitter`.
- It can also use `--submit-backend in-process`. This calls the shared `silverscript_lang::doom_tn10` transition builder directly, validates the covenant spend locally, preserves the bridge event-log shape, and removes process-spawn overhead from local cadence smoke tests.
- In live mode, `--submit true --submit-backend in-process` now performs direct TN10 RPC submission from the bridge: connect, verify Toccata, optionally preflight the current DoomState UTXO, submit the built transaction, and collect mempool/inclusion metrics into the same `childOutput` lines consumed by reports.

For local browser/bridge testing before the node is synced, run the bridge in dry-run mode:

```bash
cargo run -p silverscript-lang --bin doom_tn10_bridge -- \
  --listen 127.0.0.1:18787 \
  --submit false \
  --input-txid 0101010101010101010101010101010101010101010101010101010101010101 \
  --covenant-id dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd \
  --state-file /tmp/doom-bridge-state.json \
  --event-log /tmp/doom-bridge-events.jsonl \
  --track-mempool false \
  --track-inclusion false
```

Verified dry-run HTTP sequence:

```text
tic 1 -> 04155715030700233e79c80ff7e887ccf16051ddf643ee6bc82ae0d5056aaccd:0
ticcmd=0102030405060708
state_hash=8a3d747b75288ea08fa8eb5608de7ca1699b10adebe992f67f047243af780bf4

tic 2 -> cb58cf990f6888ad2b22347ce26cf313dccacd5540962292accc155836e0f79b:0
ticcmd=0807060504030201
state_hash=24591b516f976f8c95bfa14a04c62644ea003dff9cf448b620478df503fbb566
```

Then a restarted bridge loaded `/tmp/doom-bridge-state.json` and accepted tic 3 from the persisted canonical outpoint:

```text
tic 3 -> 5018abc5a2373a6b4b3ca3a4b9e6812e0205589a1938611fe7421a557e752628:0
ticcmd=0909090909090909
state_hash=7cc5eef77c5bec9f153f2ffc7f62b5d1b7bca627e88e764eabc09e60b4f0b6de
```

## Covenant State

The first Kaspa-side state artifact is:

`silverscript-lang/tests/apps/doom/doom_state.sil`

It models the canonical game-state UTXO with:

- `game_id`
- `session_key_hash`
- monotonic canonical `tick`
- latest 8-byte Doom tic command
- 32-byte hash of the compact Doom state
- compact Doom state byte length

The covenant transition is:

```text
current_game_utxo + old_state + next_ticcmd + next_state + session_signature
  -> next_game_utxo + next_state
```

The initial validation is intentionally cheap but aligned with the final target:

- session key authorization
- stable game/session identifiers
- covenant-level monotonic tick enforcement: `next_state.tick == prev_state.tick + 1`
- binding the successor state to the emitted Doom `ticcmd`
- state hash commitment
- KDS4-size commitment: `prev_state.state_len == 96` and `next_state.state_len == 96`

The VM test suite now proves this invariant both ways: a normal KDS4 successor executes locally, and a 32-byte successor state is rejected by the covenant. This keeps the UTXO chain tied to the current compact Doom state format instead of allowing an arbitrary hash-only payload to be substituted after genesis.

The shared Rust helper module `silverscript_lang::doom_tn10` is the canonical source for synthetic ticcmd generation, KDS4 state bytes, initial DoomState compilation, and the initial DoomState script address. The local driver, fresh-genesis planner, and status probe all use that module, so changing the covenant state layout updates transaction construction and readiness probing together.

`doom_state_budget` tracks the gap between the current compact commitment and a replayable full-state target. Chocolate Doom's vanilla savegame ceiling in this port is `0x2c000` bytes, or 180,224 bytes. At a 520-byte script-element chunk size, that is 347 chunks per tic before transaction overhead. At 10 TPS, current `KDS4` is 960 bytes/sec, while a vanilla-savegame-sized full state would be about 1,802,240 bytes/sec and cannot fit as a single state element in one covenant transition. If a chunked path carried one 520-byte chunk per transition, a full-state tic would require 347 transitions, or 34.7 seconds at 10 bps for one full tic before overhead. That does not rule out full state, but it means the pure version needs chunked state commitments, compression/delta state, or a proof/challenge design rather than just swapping the 96-byte snapshot for a savegame blob.

```bash
cargo run -p silverscript-lang --bin doom_state_budget -- --target-tps 10
```

`doom_state_manifest` is the executable checkpoint artifact for that chunked path. It takes real serialized state bytes, or deterministic synthetic bytes for planning, splits them into fixed-size chunks, hashes each chunk, and computes a manifest root over the tick, state length, chunk size, chunk count, and Merkle-style chunk tree. That `manifest_root_hex` is the small value we can later bind into a per-tic `KDS4` state, a slower checkpoint covenant, or a challenge transaction while the chunk bytes live in slower on-chain data, sidecar storage, or a witness path.

```bash
cargo run -p silverscript-lang --bin doom_state_manifest -- \
  --tick 7 \
  --synthetic-bytes 1100 \
  --chunk-bytes 520 \
  --proof-index 2
```

Example output:

```text
mode=doom-state-manifest
tick=7
state_bytes=1100
chunk_bytes=520
chunk_count=3
last_chunk_bytes=60
manifest_root_hex=5d97d02f80d1445c56204aafa2cccc3d51a18735eb999d0e8b5a342eab396b25
proof_index=2
proof_sibling_count=2
proof_verified=true
```

The same binary can verify a proof without the full serialized state bytes. This is the shape a future challenge or witness path needs after `manifest_root_hex` has been committed:

```bash
cargo run -p silverscript-lang --bin doom_state_manifest -- \
  --tick 7 \
  --chunk-bytes 520 \
  --verify-root-hex 5d97d02f80d1445c56204aafa2cccc3d51a18735eb999d0e8b5a342eab396b25 \
  --verify-leaf-hash-hex 8f8f8e0931e693d8945a0bdb49020272a8f49015c5fcd5744a660811f39fab45 \
  --verify-state-bytes 1100 \
  --verify-chunk-count 3 \
  --proof-index 2 \
  --proof-sibling-hex 8f8f8e0931e693d8945a0bdb49020272a8f49015c5fcd5744a660811f39fab45 \
  --proof-sibling-hex 8269a9d9b9a37ee4500d8bb94fc0963616dc6c59707424204a22d978b67cd2ef
```

Bridge event logs can also carry checkpoint anchors beside accepted compact tics:

```json
{
  "checkpointManifestRootHex": "5d97d02f80d1445c56204aafa2cccc3d51a18735eb999d0e8b5a342eab396b25",
  "checkpointStateBytes": 1100,
  "checkpointChunkCount": 3
}
```

`doom_tn10_report` validates those optional fields when present and reports `checkpoint_count`, `checkpoint_verified_count`, and the latest checkpoint root/size/chunk count. This keeps the event-log/report path ready for a sidecar full-state checkpoint producer without changing the current per-tic `KDS4` covenant layout.

## TN10 Cadence Benchmark

The first network benchmark should measure whether TN10 accepts chained covenant spends at the target cadence.

Benchmark stages:

1. Local compile/probe of `DoomState`.
2. Local transaction construction for a chain of successor game-state UTXOs.
3. TN10 mempool submission of chained spends without waiting for finality.
4. Block inclusion tracking for attempted 10 tics/sec.
5. Resume test from the latest accepted game-state UTXO.

The benchmark must report:

- attempted tics/sec
- accepted transactions/sec
- block-included transactions/sec
- mempool rejection reason, if any
- average and p95 inclusion latency
- maximum stable chained-spend depth
- observed rollback/reorg handling requirements

The first event-log analyzer is:

`silverscript-lang/src/bin/doom_tn10_report.rs`

It consumes the bridge JSONL event log and reports the accepted canonical tic sequence, submit latency distribution, captured-time TPS, duplicate txids, monotonicity gaps, and the latest resume tuple.
For live runs it also summarizes `rpc_submit_ok`, `mempool_seen_count`, `mempool_orphan_count`, `inclusion_seen_count`, optional mempool/inclusion latency distributions, and rejection classes. When accepted events include `stateBytesHex`, it recomputes Blake2b over those exact bytes and reports how many accepted event hashes were independently verified. It also validates the `KDS4` marker, 96-byte length, embedded canonical tick, and embedded ticcmd and reports `state_snapshot_verified_count`. For bridge logs that include `canonicalOutpointBefore`, the report checks that every accepted event's `txId` matches its `successorOutpoint` txid and that each accepted tic after the first spends the previous accepted successor outpoint, reporting `accepted_txid_verified_count` and `accepted_outpoint_link_verified_count`.

Run it against the default bridge event log:

```bash
cargo run -p silverscript-lang --bin doom_tn10_report -- \
  --event-log .doom-tn10-bridge-events.jsonl \
  --target-tps 1
```

The current practical target is 1 authoritative tic/sec, matching the latest prototype direction. The longer-term goal remains measuring whether TN10 can sustain higher rates up to 10 authoritative tics/sec once live Toccata funding, mempool acceptance, and inclusion tracking are available.

Verified dry-run report from three HTTP bridge tics captured 100 ms apart:

```text
events=3
unique_txids=3
canonical_non_monotonic_steps=0
submit_elapsed_avg_ms=27.90
submit_elapsed_p95_ms=28.13
captured_duration_ms=200.00
accepted_tps=10.0000
accepted_vs_target=1.0000
resume_tuple_prev_tick=3
resume_tuple_input_outpoint=cbffb890a6325c13b1585b511d9d6f6bf410705353331dfcb50f25c386b5e4db:0
resume_tuple_prev_ticcmd_hex=0300000000000000
resume_tuple_prev_state_hash_hex=7f2cd1392f69673a93b309881efbf069ba13834412084d9b9c5cac03ca9d124e
resume_tuple_prev_state_bytes_hex=<latest-KDS4-state-bytes-if-present>
```

Verified rejected-tic report path:

```text
HTTP status=500
error=browser canonical outpoint wrong:0 does not match bridge canonical outpoint 8d6f5e098bb69359c092f62cc908d83c7aca7bccddb347215221032bf021577f:0
accepted_events=1
rejected_events=1
rejection_count[canonical_outpoint_mismatch]=1
resume_tuple_input_outpoint=8d6f5e098bb69359c092f62cc908d83c7aca7bccddb347215221032bf021577f:0
```

Verified current live-gated bridge path against the LAN Toccata node:

```text
HTTP status=500
server_version=1.2.0-toc.2
preflight_input_visible=false
accepted_events=0
rejected_events=1
rejection_count[preflight_input_missing]=1
rpc_submit_ok=0
mempool_seen_count=0
inclusion_seen_count=0
```

For live TN10, this same report should be run after `doom_tn10_bridge` is started with `--submit true --track-mempool true --track-inclusion true`. The current event log proves browser/bridge canonical cadence and resume state; the remaining live benchmark must add RPC acceptance, mempool visibility, and accepted-block inclusion once the LAN Toccata node is synced and funded.

## Local Driver Status

`silverscript-lang/tests/doom_apps_tests.rs`, the `doom_state_driver` binary, and the guarded `doom_tn10_submitter` binary now cover the local pre-TN10 driver path:

- compiles `DoomState`
- builds a per-tic `advance` sigscript
- signs the spend with a deterministic session key
- constructs a successor game-state UTXO
- executes the covenant transaction in the Kaspa script VM with covenants enabled
- rejects non-96-byte state transitions at the covenant layer
- loops a configurable number of canonical tics and reports local throughput
- constructs the exact chained transaction shape that can be submitted to TN10 once a real current game-state UTXO exists

This is the local equivalent of one canonical Doom tic:

```text
active DoomState UTXO -> signed advance(next_ticcmd, next_state) -> successor DoomState UTXO
```

Run the local chain driver:

```bash
cargo run -p silverscript-lang --bin doom_state_driver -- --ticks 10 --target-tps 10
```

Expected report fields:

- `ticks_attempted`
- `ticks_executed`
- `target_tps`
- `local_elapsed_ms`
- `local_vm_tps`
- `script_len`
- `instruction_count`
- `charged_op_count`
- `final_tick`

The live TN10 driver should reuse the same transaction shape and replace the local VM execution step with node RPC submission plus mempool/block tracking.

Run the guarded submitter in dry-run mode:

```bash
cargo run -p silverscript-lang --bin doom_tn10_submitter -- --prev-tick 0
```

Current dry-run behavior:

- derives the active DoomState for `prev_tick`
- builds the successor DoomState for `prev_tick + 1`
- optionally binds an exact Doom browser command with `--next-ticcmd-hex <16-hex-chars>`
- optionally resumes from a non-synthetic prior state with `--prev-ticcmd-hex` and `--prev-state-hash-hex`
- signs the transition with the deterministic session key
- validates the transaction locally with covenants enabled
- prints `next_ticcmd_hex`, `next_state_hash`, and `next_state_hex` for the successor state
- prints the successor outpoint and transaction id
- can write a bridge-compatible resume state with `--wallet-address <started-wallet> --write-bridge-state .doom-tn10-bridge-state.json`
- skips RPC submission unless `--submit` is explicitly supplied

Run a local chained dry-run at the desired TN10 cadence:

```bash
cargo run -p silverscript-lang --bin doom_tn10_submitter -- \
  --prev-tick 0 \
  --ticks 3 \
  --target-tps 10 \
  --track-mempool false
```

Current local chained result:

- `ticks_attempted=3`
- `ticks_accepted=3`
- `script_len=275`
- `sigscript_len=458`
- `local_validation=ok` for each successor tic
- `final_tick=3`

Run a one-tic dry-run with an exact browser-style Doom `ticcmd`:

```bash
cargo run -p silverscript-lang --bin doom_tn10_submitter -- \
  --prev-tick 0 \
  --ticks 1 \
  --next-ticcmd-hex 0102030405060708 \
  --track-mempool false
```

Current result:

```text
next_ticcmd_hex=0102030405060708
next_state_hash=8a3d747b75288ea08fa8eb5608de7ca1699b10adebe992f67f047243af780bf4
local_validation=ok
```

Resume from that exact committed state:

```bash
cargo run -p silverscript-lang --bin doom_tn10_submitter -- \
  --prev-tick 1 \
  --prev-ticcmd-hex 0102030405060708 \
  --prev-state-hash-hex 8a3d747b75288ea08fa8eb5608de7ca1699b10adebe992f67f047243af780bf4 \
  --input-txid 04155715030700233e79c80ff7e887ccf16051ddf643ee6bc82ae0d5056aaccd \
  --ticks 1 \
  --next-ticcmd-hex 0807060504030201 \
  --track-mempool false
```

Current result:

```text
next_ticcmd_hex=0807060504030201
next_state_hash=24591b516f976f8c95bfa14a04c62644ea003dff9cf448b620478df503fbb566
local_validation=ok
```

The submitter refuses to broadcast synthetic placeholder transactions:

```bash
cargo run -p silverscript-lang --bin doom_tn10_submitter -- --submit
```

This fails before network submission with:

```text
--submit requires --input-txid so placeholder transactions are never broadcast
```

Once the initial game-state UTXO is deployed, the guarded live shape is:

```bash
cargo run -p silverscript-lang --bin doom_tn10_submitter -- \
  --input-txid <current-doom-state-txid> \
  --input-index <current-output-index> \
  --utxo-value <current-output-value-sompi> \
  --prev-tick <current-state-tick> \
  --ticks <number-of-tics> \
  --target-tps 10 \
  --track-mempool true \
  --mempool-timeout-ms 2000 \
  --track-inclusion true \
  --inclusion-timeout-ms 10000 \
  --submit
```

Current live submit evidence:

- A 512-byte state chunk version deployed, but its first tic spend was rejected by TN10 standardness: `signature script size of 2142 bytes is larger than the maximum allowed size of 1650 bytes`.
- A 128-byte state chunk version deployed, but its first tic spend was rejected by TN10 consensus limits: `element size 934 exceeds max allowed size 520`.
- A state-hash-only version deployed with a 240-byte redeem script. Its first tic spend passes local validation and standard size limits, but the default public TN10 wRPC peer rejects `OpAuthOutputCount` as an invalid opcode.
- The current submitter classifies this live rejection as `endpoint_missing_toccata_covenant_opcodes`, confirming that the next live attempt needs a different endpoint rather than another state-size reduction.
- The LAN Toccata node at `10.0.3.26` accepts the covenant opcode set and now reaches UTXO lookup. After switching live transactions to Toccata transaction version `1` and v1 input compute-budget fields, the same first tic is rejected only because the input is not yet visible on that node: `transaction ... is an orphan where orphan is disallowed`.
- Live genesis and tic submission now refuse non-Toccata endpoints before broadcast. A default public-resolver submit currently fails early with `refusing live DoomState submit through non-Toccata endpoint 1.1.0`, which prevents accidentally deploying more DoomState artifacts through an endpoint that does not enforce Toccata covenant semantics.
- The submitter now preflights the starting DoomState input by script address before live broadcast. While the LAN node is still catching up, the expected output is:

```text
preflight_input=true
preflight_state_address=kaspatest:pzl72vvus55q0u7c83kkqjy5t95cpumzfshj7dsu4a5avh6rcaf2q9j8w6l8a
preflight_state_utxo_count=0
preflight_input_visible=false
```

Once the node catches up, `preflight_input_visible=true` should be the signal to retry the real first tic broadcast.

For unattended readiness, run the first tic or browser bridge with `--wait-preflight`. The submitter will poll the active DoomState script address and only continue to local transaction construction and RPC broadcast after the exact input outpoint is visible:

```bash
cargo run -p silverscript-lang --bin doom_tn10_live
```

The higher-level live wrapper can also poll the node readiness state before it reaches the submitter. This is the preferred command while the LAN Toccata node is still syncing:

```bash
cargo run -p silverscript-lang --bin doom_tn10_live -- \
  --wait-readiness \
  --readiness-poll-ms 10000 \
  --auto-genesis \
  --write-bridge-state .doom-tn10-bridge-state.json
```

Readiness behavior:

- If the known DoomState genesis outpoint is visible on the LAN node, the wrapper submits from that outpoint.
- If the known genesis is not visible but the testnet wallet is funded and `--auto-genesis` is set, the wrapper deploys a fresh DoomState genesis through the Toccata node and then submits the first tic from that fresh outpoint.
- If neither condition is true, `--wait-readiness` keeps polling instead of broadcasting an orphan spend.
- If `--write-bridge-state` is provided, the wrapper passes the started wallet address through to the submitter and writes the final successor as a bridge-compatible resume state.

Current LAN node status:

```text
url=ws://10.0.3.26:17210
server_version=1.2.0-toc.2
is_synced=false
virtual_daa_score=471054110
reference_url=wss://electron-10.kaspa.blue/kaspa/testnet-10/wrpc/borsh
reference_virtual_daa_score=471822060
sync_virtual_daa_lag=767950
sync_virtual_daa_caught_up_percent=99.8372
wallet_funded=false
doom_expected_genesis_visible=false
```

`doom_tn10_live` wraps the current default LAN endpoint, known genesis outpoint, covenant id, status probe, waitable preflight, and one-tic submit path. For a bounded smoke test while the node is still catching up:

```bash
cargo run -p silverscript-lang --bin doom_tn10_live -- \
  --preflight-timeout-ms 2000 \
  --preflight-poll-ms 1000 \
  --track-inclusion false
```

To scan public candidates before running the live path, use endpoint auto-selection:

```bash
cargo run -p silverscript-lang --bin doom_tn10_live -- \
  --auto-select-endpoint \
  --wallet-address kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz \
  --endpoint-scan-timeout-ms 5000 \
  --preflight-timeout-ms 1 \
  --track-inclusion false
```

Current auto-select smoke result:

```text
selected_endpoint=ws://10.0.3.26:17210
selected_reason=toccata_but_not_synced_or_no_utxoindex
reference_virtual_daa_score=471762634
sync_virtual_daa_lag=854059
sync_virtual_daa_caught_up_percent=99.8190
preflight_input_visible=false
```

`doom_tn10_live --auto-select-endpoint` now passes the configured `--wallet-address` into scan mode. Endpoint selection prefers a synced Toccata node with UTXO index and visible wallet funds. If no Toccata endpoint sees the wallet funds, selection still returns the best synced Toccata endpoint but the scan output keeps the funding split explicit with `candidate_wallet_funded=false` and `candidate_funded_selectable=false`.

When a synced public Toccata endpoint becomes reachable and sees wallet funding, `--auto-select-endpoint` should switch `doom_tn10_live` to it before readiness probing or tic submission and report `selected_reason=synced_toccata_utxoindex_wallet_funded`.

To refresh the full endpoint/funding split, including the local Toccata node, run:

```bash
KASPA_TN10_MNEMONIC='<testnet mnemonic from local shell history or password manager>' \
cargo run -q -p silverscript-lang --bin doom_tn10_live -- \
  --auto-select-endpoint \
  --scan-pnn true \
  --candidate-url ws://10.0.3.26:17210 \
  --candidate-url wss://testnet10-wrpc.kasia.fyi \
  --candidate-url wss://photon-10.kaspa.red/kaspa/testnet-10/wrpc/borsh \
  --candidate-url wss://baryon-10.kaspa.green/kaspa/testnet-10/wrpc/borsh \
  --candidate-url wss://neutrino-10.kaspa.stream/kaspa/testnet-10/wrpc/borsh \
  --endpoint-scan-timeout-ms 30000 \
  --reference-url wss://neutrino-10.kaspa.stream/kaspa/testnet-10/wrpc/borsh \
  --readiness-only
```

Candidate scan output now includes `candidate_wallet_largest_utxo_txid`, `candidate_wallet_largest_utxo_index`, and `candidate_wallet_largest_utxo_amount_sompi` for any endpoint that sees wallet funds. The required live condition is a single candidate with all of `candidate_is_toccata=true`, `candidate_is_synced=true`, `candidate_has_utxo_index=true`, and `candidate_wallet_funded=true`.

Latest broad scan result:

```text
candidate_url=wss://testnet10-wrpc.kasia.fyi
candidate_server_version=1.2.0-toc.2
candidate_is_synced=true
candidate_has_utxo_index=true
candidate_is_toccata=true
candidate_wallet_balance_kas=0.00000000
candidate_funded_selectable=false

candidate_url=ws://10.0.3.26:17210
candidate_server_version=1.2.0-toc.2
candidate_is_synced=false
candidate_has_utxo_index=true
candidate_is_toccata=true
candidate_wallet_balance_kas=0.00000000
candidate_funded_selectable=false

candidate_url=wss://photon-10.kaspa.red/kaspa/testnet-10/wrpc/borsh
candidate_connected=false
candidate_error=Connection timeout

candidate_url=wss://baryon-10.kaspa.green/kaspa/testnet-10/wrpc/borsh
candidate_connected=false
candidate_error=HTTP error: 308 Permanent Redirect

candidate_url=wss://neutrino-10.kaspa.stream/kaspa/testnet-10/wrpc/borsh
candidate_server_version=1.1.0
candidate_is_toccata=false
candidate_wallet_balance_kas=99895.99950727
candidate_wallet_largest_utxo_txid=44958358cd848ef43c91112738ef8be5ffed574982e152be98913eeb94b726c0
candidate_wallet_largest_utxo_index=1
candidate_wallet_largest_utxo_amount_sompi=9989599950727

selected_endpoint=wss://testnet10-wrpc.kasia.fyi
selected_reason=synced_toccata_utxoindex
readiness_result=not_ready
```

The live wrapper now passes `--compare-reference` to `tn10_status_probe` by default. That resolves a public synced TN10 reference through PNN and prints the local node's virtual-DAA lag before attempting the submitter. Use `--compare-reference false` to skip the extra reference connection, or pass `--reference-url <wss-or-ws-url>` to pin the comparison endpoint.

For a non-broadcasting live readiness check, run:

```bash
KASPA_TN10_MNEMONIC='...' cargo run -p silverscript-lang --bin doom_tn10_live -- \
  --url wss://testnet10-wrpc.kasia.fyi \
  --reference-url wss://neutrino-10.kaspa.stream/kaspa/testnet-10/wrpc/borsh \
  --readiness-only
```

This runs `tn10_status_probe` and `tn10_wallet_key_check`, verifies the configured wallet address and mnemonic match without printing secrets, reports whether the known DoomState genesis is visible, reports whether the selected Toccata endpoint sees wallet funding, and exits before genesis or tic submission. The important output fields are:

```text
readiness_old_genesis_visible=true|false
readiness_wallet_funded=true|false
readiness_wallet_balance_kas=<selected-Toccata-balance-or-unknown>
readiness_reference_wallet_funded=true|false|unknown
readiness_reference_wallet_balance_kas=<reference-balance-or-unknown>
readiness_key_available=true|false
readiness_key_matches=true|false
readiness_ready_to_submit_existing=true|false
readiness_ready_to_deploy_fresh_genesis=true|false
readiness_result=ready_existing_genesis|ready_fresh_genesis|not_ready
```

`--readiness-only --auto-genesis` is still diagnostic-only. It reports `readiness_decision=not_ready` and exits successfully when fresh genesis is not safe yet. A live auto-genesis deploy now requires all three conditions: the known genesis is not visible, the selected Toccata endpoint sees wallet funds, and the local mnemonic/private key derives the target wallet address. This prevents the wrapper from trying a fresh genesis with missing or mismatched signing material.

If the selected Toccata endpoint reports `readiness_wallet_funded=false` while the reference endpoint reports `readiness_reference_wallet_funded=true`, the wallet seed/address are not the failure. The funds are visible on another TN10 view but not on the covenant-capable Toccata view selected for submission. In that case `next_required_step` explicitly says to fund the wallet on the Toccata fork/view or use a synced Toccata node that sees the funded UTXO. The browser bridge exposes the same split as `reason=wallet_unfunded_on_toccata_reference_funded` from `/ready`.

The standalone sync-progress probe is:

```bash
cargo run -p silverscript-lang --bin tn10_status_probe -- \
  --url ws://10.0.3.26:17210 \
  --compare-reference \
  --timeout-ms 12000
```

Current public Toccata endpoint status:

```text
url=wss://testnet10-wrpc.kasia.fyi
server_version=1.2.0-toc.2
is_synced=true
has_utxo_index=true
virtual_daa_score=471903767
wallet_funded=false
wallet_balance_kas=0.00000000
doom_initial_state_address=kaspatest:pp7yce69fw36pt4c3skxz44j7mpj4as0wsjqwuehmvz077dytkzeu77xslk4v
doom_expected_genesis_visible=false
reference_url=wss://neutrino-10.kaspa.stream/kaspa/testnet-10/wrpc/borsh
reference_server_version=1.1.0
reference_wallet_funded=true
reference_wallet_balance_kas=99895.99950727
reference_wallet_largest_utxo_txid=44958358cd848ef43c91112738ef8be5ffed574982e152be98913eeb94b726c0
reference_wallet_largest_utxo_index=1
readiness_key_matches=true
readiness_ready_to_deploy_fresh_genesis=false
readiness_decision=not_ready
readiness_result=not_ready
```

This confirms the wallet seed/address are correct, but the spendable funding UTXO is still visible only on the non-Toccata TN10 reference view, not on the Toccata covenant endpoint needed for a real DoomState deploy.

Public Photon endpoint trial:

```text
url=wss://photon-10.kaspa.red/kaspa/testnet-10/wrpc/borsh
tn10_status_probe_result=failed to connect to TN10 wRPC: wRPC -> WebSocket -> Connection timeout
retry_timeout_ms=30000
tcp_443=ok
tls=ok
websocket_upgrade=no response before 15s timeout
variants_tried=wss path, wss path with trailing slash, ws path, explicit HTTP/1.1 websocket upgrade, websocket subprotocol borsh
```

This means Photon was not usable from this workspace at the time of testing. The failure happened before `get_server_info`, so it does not prove anything about whether the backend is Toccata-capable.

Endpoint scan mode:

```bash
cargo run -p silverscript-lang --bin tn10_status_probe -- \
  --scan-candidates \
  --timeout-ms 8000 \
  --candidate-url ws://10.0.3.26:17210 \
  --candidate-url wss://photon-10.kaspa.red/kaspa/testnet-10/wrpc/borsh \
  --candidate-url wss://baryon-10.kaspa.green/kaspa/testnet-10/wrpc/borsh
```

Latest candidate scan:

```text
candidate_url=ws://10.0.3.26:17210
candidate_connected=true
candidate_server_version=1.2.0-toc.2
candidate_is_synced=false
candidate_has_utxo_index=true
candidate_is_toccata=true
candidate_wallet_utxo_count=0
candidate_wallet_balance_kas=0.00000000
candidate_wallet_funded=false
candidate_selectable=false
candidate_funded_selectable=false

candidate_url=wss://photon-10.kaspa.red/kaspa/testnet-10/wrpc/borsh
candidate_connected=false
candidate_error=failed to connect to candidate TN10 wRPC: wRPC -> WebSocket -> Connection timeout

candidate_url=wss://baryon-10.kaspa.green/kaspa/testnet-10/wrpc/borsh
candidate_connected=false
candidate_error=failed to connect to candidate TN10 wRPC: wRPC -> WebSocket -> WebSocket error: HTTP error: 308 Permanent Redirect

candidate_url=<PNN public endpoint>
candidate_connected=true
candidate_server_version=1.1.0
candidate_is_synced=true
candidate_has_utxo_index=true
candidate_is_toccata=false
candidate_wallet_utxo_count=1
candidate_wallet_balance_kas=99895.99950727
candidate_wallet_funded=true
candidate_selectable=false
candidate_funded_selectable=false

selected_endpoint=ws://10.0.3.26:17210
selected_reason=toccata_but_not_synced_or_no_utxoindex
```

Scan mode now includes wallet funding for every connected candidate and prefers `selected_reason=synced_toccata_utxoindex_wallet_funded` when any synced Toccata endpoint actually sees spendable wallet UTXOs. If no Toccata endpoint sees funds, it still selects the best synced Toccata endpoint but the `candidate_wallet_funded=false` lines make the live funding gate explicit before any genesis attempt.

Baryon lower-level check:

```text
tcp_443=ok
tls=ok
websocket_upgrade_response=HTTP/1.1 308 Permanent Redirect
location=https://baryon-10.kaspa.green/kaspa/testnet-10/wrpc/borsh
path_without_borsh=HTTP 308 too
```

As with Photon, this is a transport-level failure before `get_server_info`; it does not prove the backend is non-Toccata.

If the old public-path genesis never appears on the LAN Toccata node but the Doom wallet becomes funded there, deploy a fresh Toccata genesis and immediately feed its parsed outpoint/covenant id into the first tic submitter:

```bash
cargo run -p silverscript-lang --bin doom_tn10_live -- \
  --deploy-fresh-genesis \
  --write-bridge-state .doom-tn10-bridge-state.json
```

The fresh-genesis path uses `doom_tn10_genesis_plan --submit`, so signing material still comes from an ignored environment variable, defaulting to `KASPA_TN10_MNEMONIC`. Current LAN smoke result before sync/funding visibility:

```text
server_version=1.2.0-toc.2
wallet kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz has no spendable UTXOs
doom_tn10_genesis_plan exited with exit status: 1
```

The lower-level equivalent is:

```bash
cargo run -p silverscript-lang --bin doom_tn10_submitter -- \
  --url ws://10.0.3.26:17210 \
  --prev-tick 0 \
  --ticks 1 \
  --input-txid 44958358cd848ef43c91112738ef8be5ffed574982e152be98913eeb94b726c0 \
  --input-index 0 \
  --utxo-value 100000000 \
  --covenant-id b33892ffcac705a87ce7a747cf8e289c7d48d87cecf1469731f5374a13625fb0 \
  --submit \
  --allow-orphan \
  --wait-preflight \
  --preflight-timeout-ms 0 \
  --preflight-poll-ms 10000 \
  --track-mempool true \
  --track-inclusion true
```

The last rejection indicates the currently selected public wRPC peer is not running the Toccata covenant opcode set despite reporting `network_id=testnet-10`. The next live test must use an explicitly upgraded Toccata TN10 node endpoint.

The submitter now separates these phases in output:

- `local_validation=ok`: the transaction executes in the local Kaspa script VM with covenants enabled.
- `rpc_submit=ok`: the node accepted the transaction submission.
- `mempool_seen=true`: a post-submit `get_mempool_entry` poll found the transaction in the node mempool.
- `mempool_is_orphan=true|false`: the node reports whether the accepted transaction is waiting on a parent.
- `inclusion_seen=true`: a post-submit virtual-chain scan found the transaction in accepted transaction ids.
- `accepting_block_hash=<hash>`: the block that accepted the submitted DoomState transition.

The inclusion scan uses the node's `get_virtual_chain_from_block(start_sink, include_accepted_transaction_ids=true, min_confirmation_count=None)` path. This is enough to measure first acceptance latency for the current canonical-chain view; later reorg handling should retain the accepting block and keep watching whether the successor UTXO remains canonical.

## Local Toccata TN10 Node

The branch dependencies resolve Rusty Kaspa to a Toccata build:

```text
kaspad v1.1.1-toc.1-7b1e18cc
```

Build/help command:

```bash
cargo run \
  --manifest-path /home/luke/.cargo/git/checkouts/rusty-kaspa-410e06d1fde91a92/7b1e18c/Cargo.toml \
  -p kaspad -- --help
```

Local node command used for the upgraded endpoint path:

```bash
/home/luke/.cargo/git/checkouts/rusty-kaspa-410e06d1fde91a92/7b1e18c/target/debug/kaspad \
  --appdir=/tmp/kaspa-doom-tn10-node \
  --testnet \
  --netsuffix=10 \
  --utxoindex \
  --rpclisten-borsh=127.0.0.1:17210 \
  --rpclisten=127.0.0.1:16210 \
  --listen=127.0.0.1:16211 \
  --yes \
  --nologfiles \
  --ram-scale=0.3
```

Probe the local endpoint:

```bash
cargo run -p silverscript-lang --bin tn10_status_probe -- \
  --url ws://127.0.0.1:17210 \
  --timeout-ms 5000
```

Current local-node evidence:

- The node starts and exposes wRPC Borsh on `127.0.0.1:17210`.
- It reports `server_version=1.1.1-toc.1`.
- It reports `network_id=testnet-10`.
- It has `has_utxo_index=true`.
- While IBD is still running, `is_synced=false`, `virtual_daa_score=0`, and the wallet UTXO set is not available yet.
- Foreground sync attempts have reached header IBD but not a usable synced RPC state yet. A previous run reached 18% before clean shutdown; the latest resumed run reached 14% before clean shutdown. Until the node finishes IBD, RPC still reports `virtual_daa_score=0` and the wallet UTXO set is unavailable from the local endpoint.

Once this local node is synced, retry the live transition against the upgraded endpoint:

```bash
cargo run -p silverscript-lang --bin doom_tn10_submitter -- \
  --url ws://127.0.0.1:17210 \
  --prev-tick 0 \
  --ticks 1 \
  --input-txid 44958358cd848ef43c91112738ef8be5ffed574982e152be98913eeb94b726c0 \
  --input-index 0 \
  --utxo-value 100000000 \
  --covenant-id b33892ffcac705a87ce7a747cf8e289c7d48d87cecf1469731f5374a13625fb0 \
  --submit \
  --allow-orphan
```

## LAN Toccata TN10 Node

There is a LAN VM at `10.0.3.26` intended to run the upgraded TN10 Toccata node. The host was reachable by ICMP and briefly reachable over SSH as `ubuntu`.

Verified host-side facts from SSH before access dropped:

- Hostname: `dev-server`.
- Toccata binary: `/home/ubuntu/tn10-toc2/bin/kaspad-bump-r0-7ac0efdd`.
- Binary version: `kaspad v1.2.0-toc.2`.
- Existing services:
  - `kaspad-tn10-toc2.service`
  - `kaspa-miner-tn10-toc2.service`
- Service failure cause: root filesystem was full and kaspad panicked with `No space left on device` while appending RocksDB files.
- Root filesystem before VM disk expansion: `/dev/sda1` was 145G and 100% used.
- `tn10-toc2/data` used about 117G, mostly `datadir/consensus` and `datadir/utxoindex`.
- Old RocksDB/log files were removed, freeing about 1.6G, but the node still required a root filesystem expansion before restart.

The service unit originally exposed only gRPC on localhost:

```text
--rpclisten=127.0.0.1:16210
```

For this Doom submitter to use the node from the development workstation, `kaspad-tn10-toc2.service` needs wRPC Borsh exposed on the LAN:

```text
--rpclisten=0.0.0.0:16210
--rpclisten-borsh=0.0.0.0:17210
--rpclisten-json=0.0.0.0:18210
```

Current external state after the VM disk was expanded by the hypervisor:

- Root filesystem is expanded and healthy: `/dev/sda1` is about 387G with about 244G free.
- SSH is reachable as `ubuntu`.
- `kaspad-tn10-toc2.service` is active with a drop-in at `/etc/systemd/system/kaspad-tn10-toc2.service.d/doom-rpc.conf`.
- The active kaspad command exposes:
  - `0.0.0.0:16210` gRPC
  - `0.0.0.0:17210` wRPC Borsh
  - `0.0.0.0:18210` wRPC JSON
- UFW allows `17210/tcp` and `18210/tcp`.
- `tn10_status_probe --url ws://10.0.3.26:17210` connects and reports `server_version=1.2.0-toc.2`.
- The node is still catching up. Recent probe evidence: `is_synced=false`, `virtual_daa_score=471054110`, reference `virtual_daa_score=471822060`, lag `767950`, caught up `99.8372%`, and the Doom wallet has no UTXOs visible yet from this node.
- Direct genesis-state probe on the LAN node:
  - `doom_initial_state_address=kaspatest:pzl72vvus55q0u7c83kkqjy5t95cpumzfshj7dsu4a5avh6rcaf2q9j8w6l8a`
  - `doom_initial_state_utxo_count=0`
  - `doom_expected_genesis_visible=false`
- Public PNN comparison at the same time reported `server_version=1.1.0`, `is_synced=true`, and `virtual_daa_score=471734415`, so the local Toccata node still has substantial block validation to finish.
- Public PNN sees the old public-path state:
  - `wallet_largest_utxo_txid=44958358cd848ef43c91112738ef8be5ffed574982e152be98913eeb94b726c0`
  - `wallet_largest_utxo_index=1`
  - `doom_initial_state_utxo_count=1`
  - `doom_expected_genesis_visible=true`
- The miner service has a drop-in at `/etc/systemd/system/kaspa-miner-tn10-toc2.service.d/doom-wallet.conf` and now mines to the Doom test wallet address once kaspad is synced.

The service setup applied on the VM is:

```bash
sudo mkdir -p /etc/systemd/system/kaspad-tn10-toc2.service.d
sudo tee /etc/systemd/system/kaspad-tn10-toc2.service.d/doom-rpc.conf >/dev/null <<'EOF'
[Service]
ExecStart=
ExecStart=/home/ubuntu/tn10-toc2/bin/kaspad-bump-r0-7ac0efdd --yes --testnet --utxoindex --appdir=/home/ubuntu/tn10-toc2/data --rpclisten=0.0.0.0:16210 --rpclisten-borsh=0.0.0.0:17210 --rpclisten-json=0.0.0.0:18210 --ram-scale=0.3
EOF

sudo mkdir -p /etc/systemd/system/kaspa-miner-tn10-toc2.service.d
sudo tee /etc/systemd/system/kaspa-miner-tn10-toc2.service.d/doom-wallet.conf >/dev/null <<'EOF'
[Service]
ExecStart=
ExecStart=/home/ubuntu/tn10-toc2/bin/kaspa-miner-v0.2.7-linux-gnu-amd64 --testnet -t 1 -a kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz
EOF

sudo systemctl daemon-reload
sudo systemctl restart kaspad-tn10-toc2.service
sudo systemctl restart kaspa-miner-tn10-toc2.service
sudo ufw allow 17210/tcp comment "TN10 Toccata wRPC Borsh"
sudo ufw allow 18210/tcp comment "TN10 Toccata wRPC JSON"
```

Then verify from the workstation:

```bash
cargo run -p silverscript-lang --bin tn10_status_probe -- \
  --url ws://10.0.3.26:17210 \
  --timeout-ms 12000
```

Current retry command for the first tic, once the node sees the genesis game UTXO or after a fresh v1 genesis deploy:

```bash
cargo run -p silverscript-lang --bin doom_tn10_submitter -- \
  --url ws://10.0.3.26:17210 \
  --prev-tick 0 \
  --ticks 1 \
  --input-txid 44958358cd848ef43c91112738ef8be5ffed574982e152be98913eeb94b726c0 \
  --input-index 0 \
  --utxo-value 100000000 \
  --covenant-id b33892ffcac705a87ce7a747cf8e289c7d48d87cecf1469731f5374a13625fb0 \
  --submit \
  --allow-orphan \
  --preflight-input true \
  --wait-preflight \
  --preflight-timeout-ms 0 \
  --preflight-poll-ms 10000 \
  --track-mempool true \
  --track-inclusion true \
  --inclusion-timeout-ms 10000
```

## Genesis UTXO Plan

The `doom_tn10_genesis_plan` binary plans the first on-chain DoomState UTXO from the funded TN10 wallet. It does not sign or submit yet; it computes the covenant id and deploy transaction shape from the real funding outpoint returned by TN10.

Run:

```bash
cargo run -p silverscript-lang --bin doom_tn10_genesis_plan -- --timeout-ms 8000
```

The planner now also derives the wallet signing key from an ignored environment variable, signs locally, validates locally, and submits only when `--submit` is supplied.

Most recent successful live minimal-state deploy:

- `funding_outpoint=(03d971604275d0eaf083dacf12e39543972d35759c46edafde9c78ed91609af2, 1)`
- `funding_amount_sompi=9989699960727`
- `game_value_sompi=100000000`
- `fee_sompi=10000`
- `change_value_sompi=9989599950727`
- `doom_covenant_id=b33892ffcac705a87ce7a747cf8e289c7d48d87cecf1469731f5374a13625fb0`
- `signed_deploy_tx_id=44958358cd848ef43c91112738ef8be5ffed574982e152be98913eeb94b726c0`
- `initial_game_outpoint=(44958358cd848ef43c91112738ef8be5ffed574982e152be98913eeb94b726c0, 0)`
- `initial_state_tick=0`
- `script_len=240`
- `rpc_submit=ok`

That deploy was submitted through the public TN10 path before the LAN Toccata node was usable and before the covenant state added an explicit `tick` field. The current tick-bearing DoomState script is incompatible with that older genesis output, so the next live path should deploy a fresh v1 DoomState genesis through a Toccata-visible wallet UTXO rather than trying to spend the old public-path genesis.

## Live TN10 Driver Requirements

The live driver should be a separate binary or web-worker module that accepts:

- TN10 RPC endpoint
- funded wallet/session key source from env or ignored local file
- current game-state outpoint
- compact Doom state bytes
- tic command bytes
- target send rate

It should produce:

- successor transaction id
- successor game-state outpoint
- local validation result before submit
- RPC submit result
- mempool acceptance timestamp
- block inclusion timestamp and block DAA score
- retry/reorg decision if the canonical successor changes

Local signing material must stay out of git. The root `.gitignore` reserves `.env`, `.env.*`, `secrets/`, and `wallets/` for testnet recovery words, private keys, wallet databases, and generated session keys.

## Live TN10 RPC Probe

The `tn10_status_probe` binary verifies the wRPC path that the submitter will use:

```bash
cargo run -p silverscript-lang --bin tn10_status_probe -- --timeout-ms 8000
```

It also checks the configured TN10 wallet address through the node UTXO index. The default address is:

```text
kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz
```

By default it also compiles the initial DoomState script and probes the corresponding P2SH address. This gives a direct visibility check for the known genesis game-state output:

```text
doom_initial_state_address=kaspatest:pzl72vvus55q0u7c83kkqjy5t95cpumzfshj7dsu4a5avh6rcaf2q9j8w6l8a
doom_expected_genesis_outpoint=(44958358cd848ef43c91112738ef8be5ffed574982e152be98913eeb94b726c0, 0)
doom_expected_genesis_visible=true|false
```

Current successful public-PNN probe fields:

- `network_id=testnet-10`
- `is_synced=true`
- `has_utxo_index=true`
- `virtual_daa_score=471686359`
- `wallet_utxo_count=1`
- `wallet_balance_sompi=9989999990727`
- `wallet_balance_kas=99899.99990727`
- `wallet_funded=true`
- `wallet_largest_utxo_txid=6f3bb4a73f12a89f66b86251417a70b134e51ebd41f85424383ef3bb18b0b157`
- `wallet_largest_utxo_index=0`
- `wallet_largest_utxo_amount_sompi=9989999990727`

The submitter should build on this same client path and add transaction construction, `submit_transaction`, and block-inclusion tracking.

## Browser Bridge Readiness

The local browser bridge exposes readiness separately from canonical state:

```bash
cargo build -p silverscript-lang --bin doom_tn10_bridge --bin tn10_status_probe
target/debug/doom_tn10_bridge \
  --listen 127.0.0.1:8787 \
  --url ws://10.0.3.26:17210 \
  --submit false
curl 'http://127.0.0.1:8787/ready?walletAddress=kaspatest%3Aqp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz'
```

`GET /ready` shells the same `tn10_status_probe` binary used by the live submitter path and returns browser JSON for:

- endpoint version, Toccata detection, sync status, UTXO index status, and virtual DAA
- wallet UTXO count and balance
- reference-endpoint wallet UTXO count and balance, when available, to expose fork/endpoint funding mismatches
- known Doom genesis UTXO visibility
- local wallet key availability and address match
- whether the selected endpoint can submit from an existing visible genesis
- whether the selected endpoint can deploy a fresh genesis from wallet funds
- whether the browser `start` action is available and which start mode it will use
- a single readiness reason such as `ready`, `endpoint_not_synced`, `wallet_unfunded`, or `endpoint_missing_toccata`
- `nextRequiredStep`, an action string suitable for displaying directly in the Doom menu next to the disabled/enabled start button

Current LAN-node evidence from `ws://10.0.3.26:17210`:

```text
ready=false
probeOk=true
serverVersion=1.2.0-toc.2
isToccata=true
isSynced=false
hasUtxoIndex=true
virtualDaaScore=470977162
walletFunded=false
walletBalanceKas=0.00000000
doomExpectedGenesisVisible=false
reason=endpoint_not_synced
```

The Doom page now calls `/ready?walletAddress=...` on startup and every 30 seconds, displaying Toccata wallet balance, reference wallet balance, wallet key status, endpoint status, start mode, readiness reason, and next required step next to the canonical tic/outpoint state. In live submit mode, the browser start button is only enabled after `/ready` reports that fresh-genesis deployment is possible. In dry-run mode, `/ready` reports `startMode=synthetic_dry_run`, so the browser can still exercise start -> tic -> resume without pretending that anything was deployed on-chain.

Current public Toccata `/ready` evidence:

```text
endpointUrl=wss://testnet10-wrpc.kasia.fyi
serverVersion=1.2.0-toc.2
isToccata=true
isSynced=true
walletBalanceKas=0.00000000
referenceWalletBalanceKas=99895.99950727
referenceWalletUtxoCount=1
walletKeyAvailable=true
walletKeyMatches=true
walletKeyPath=m/44'/111111'/0'/0/0
readyToSubmitExisting=false
readyToDeployFreshGenesis=false
startAvailable=false
startMode=unavailable
reason=wallet_unfunded_on_toccata_reference_funded
nextRequiredStep=wallet funds are visible on the reference TN10 endpoint but not on the selected Toccata endpoint; fund this wallet on the Toccata fork/view or switch the bridge to a synced Toccata node that sees the UTXO
```

The separate key check can be run without printing secrets:

```bash
KASPA_TN10_MNEMONIC='<testnet mnemonic>' \
  cargo run -q -p silverscript-lang --bin tn10_wallet_key_check
```

Expected non-secret fields:

```text
key_available=true
key_matches_wallet=true
matched_key_path=m/44'/111111'/0'/0/0
derived_address=kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz
private_key_printed=false
```

## Browser Bridge Start Game

The local bridge also exposes `POST /start`:

```bash
curl -X POST 'http://127.0.0.1:8787/start' \
  -H 'content-type: application/json' \
  --data '{"walletAddress":"kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz"}'
```

`POST /start` runs `doom_tn10_genesis_plan` through the same bridge endpoint and wallet target. On success it persists the canonical tick-0 outpoint, generated Doom covenant id, and started wallet address into the bridge state file, so the next browser tic can spend the started game UTXO without manually passing `--covenant-id`.

When the bridge is started with `--submit true`, `POST /start` remains strict: the selected Toccata endpoint must show a spendable wallet UTXO or the request fails without writing bridge state.

When the bridge is started with `--submit false`, `POST /start` now has an explicit synthetic dry-run fallback. If the real genesis planner cannot find a spendable wallet UTXO, the bridge writes a synthetic tick-0 canonical tuple and returns `synthetic=true` plus `rpcSubmit=synthetic_dry_run`. This keeps the browser start -> tic -> canonical state workflow testable while live Toccata funding is unavailable, without pretending that anything was deployed on-chain.

After `/start`, the bridge treats the posted wallet address as part of the game session. `/state` returns `walletAddress`, rejected `/tic` responses include the same canonical wallet, and a browser tic posted with a different `walletAddress` is rejected as `rejectionClass=wallet_mismatch` before transaction construction. The report tool also writes `wallet_address` into generated bridge state files and includes `--wallet-address` in its resume command, so both state-file and explicit-tuple resume preserve this session binding.

Current strict live-style runtime result is the expected clean failure until sync/funds are visible:

```text
HTTP/1.1 500 Internal Server Error
error=genesis planner rejected start for wallet ... has no spendable UTXOs
canonicalTick=0
canonicalOutpoint=0000000000000000000000000000000000000000000000000000000000000001:0
```

No bridge state file was written on that failed start attempt.

Current dry-run start/tic evidence against the synced public Toccata endpoint:

```text
POST /start
synthetic=true
rpcSubmit=synthetic_dry_run
initialGameOutpoint=0000000000000000000000000000000000000000000000000000000000000001:0

POST /tic
canonicalTick=1
ticcmdHex=0102030405060708
next_state_len=64
stateHash=b8b255f300c1feafe4cdfc97bea167e3116c06ffb90784cbd8f66faf431ca0c4
rpc_submit=skipped
```

The same flow is available as a repeatable smoke runner:

```bash
KASPA_TN10_MNEMONIC='<testnet mnemonic>' \
  cargo run -q -p silverscript-lang --bin doom_tn10_smoke -- \
  --listen 127.0.0.1:8800 \
  --url wss://testnet10-wrpc.kasia.fyi \
  --timeout-ms 45000 \
  --ticks 5 \
  --target-tps 1 \
  --captured-tps 1 \
  --submit-backend in-process \
  --restart-after-tick 3 \
  --probe-bad-tick \
  --probe-bad-state \
  --probe-bad-state-ticcmd \
  --probe-corrupt-resume \
  --probe-cli-resume \
  --probe-live-preflight \
  --keep-artifacts
```

For a live direct bridge attempt after a Toccata-visible genesis exists, use the same bridge with `--submit true --submit-backend in-process`. The smoke runner's `--probe-live-preflight` flag verifies the current pre-funding behavior without broadcasting: it launches a separate live direct bridge, posts one browser-shaped tic, requires a structured HTTP rejection, and then checks the JSONL event log for a report-compatible rejected event. In the current public Toccata state, the direct backend builds and locally validates the tick transaction but rejects before broadcast because the expected genesis input is not visible:

```text
mode=bridge-in-process-live
local_validation=ok
server_version=1.2.0-toc.2
preflight_state_utxo_count=0
preflight_input_visible=false
rejectionClass=preflight_input_missing
live_preflight_probe_performed=true
```

Add `--probe-bad-tick` to the smoke command to submit one intentionally skipped canonical tick after the first accepted tic. The expected result is one `canonical_tick_mismatch` rejected event while the accepted sequence continues from the unchanged canonical outpoint.
Add `--probe-bad-state` to submit one legacy `KDS3` state snapshot for the next canonical tic. Add `--probe-bad-state-ticcmd` to submit a `KDS4` snapshot whose embedded ticcmd differs from the payload ticcmd. The expected result is one `invalid_state_snapshot` rejected event per bad-state probe while the accepted sequence still advances only from valid `KDS4` snapshots.

Observed smoke output:

```text
mode=doom-tn10-smoke
url=wss://testnet10-wrpc.kasia.fyi
wallet_address=kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz
start_synthetic=true
start_outpoint=0000000000000000000000000000000000000000000000000000000000000001:0
state_file=.doom-tn10-smoke-4188459.state.json
event_log=.doom-tn10-smoke-4188459.events.jsonl
artifacts_kept=true
ticks_requested=5
ticks_accepted=5
captured_tps=1.000
event_log_events=8
event_log_accepted=5
event_log_rejected=3
event_log_canonical_tick_mismatch=1
event_log_invalid_state_snapshot=2
event_log_last_canonical_tick=5
event_log_last_successor_outpoint=40d938ee420234c89a6871482dd07100e415ded36355bfd13445949bb7cfa9e1:0
event_log_last_state_hash=cc3d62a8b15783172cc4181c28bd699720cc566b9ba13cdf9b449ac999e6744a
event_log_ticcmds_verified=true
event_log_state_hashes_verified=true
state_bytes_verified=true
target_tps=1.000
bad_tick_probe_performed=true
bad_tick_probe_tick=3
bad_tick_probe_canonical_tick=1
bad_tick_probe_canonical_outpoint=2d9bd8d5879af7b6386305d7b78eae4a147341c0a5a5f4942b92b6a2477aab0b:0
bad_state_probe_performed=true
bad_state_probe_canonical_tick=1
bad_state_probe_canonical_outpoint=2d9bd8d5879af7b6386305d7b78eae4a147341c0a5a5f4942b92b6a2477aab0b:0
bad_state_ticcmd_probe_performed=true
bad_state_ticcmd_probe_canonical_tick=1
bad_state_ticcmd_probe_canonical_outpoint=2d9bd8d5879af7b6386305d7b78eae4a147341c0a5a5f4942b92b6a2477aab0b:0
restart_after_tick=3
corrupt_resume_probe_performed=true
restart_performed=true
restart_loaded_tick=3
restart_loaded_outpoint=cab6062ab9f8d2d997befe5e8935d676a79956e5d29ee1b6394f2ac2f87a014e:0
restart_loaded_state_hash=43dabaa10bb621e619325d96f0d6a22630d9292ebd795906923b64dc7ed25937
restart_loaded_state_bytes_hex=4b445333030000006900000021270001060000000100000004000000333435363738393a3b3c3d3e3f404142434445464748494a4b4c4d4e4f5051525354555603000000090f1583
smoke_elapsed_ms=210.554
smoke_tps=23.747
smoke_vs_target=23.747
last_tic_canonical_tick=5
last_tic_successor_outpoint=40d938ee420234c89a6871482dd07100e415ded36355bfd13445949bb7cfa9e1:0
last_tic_state_hash=cc3d62a8b15783172cc4181c28bd699720cc566b9ba13cdf9b449ac999e6744a
expected_final_state_hash=cc3d62a8b15783172cc4181c28bd699720cc566b9ba13cdf9b449ac999e6744a
state_canonical_tick=5
state_canonical_outpoint=40d938ee420234c89a6871482dd07100e415ded36355bfd13445949bb7cfa9e1:0
state_hash=cc3d62a8b15783172cc4181c28bd699720cc566b9ba13cdf9b449ac999e6744a
state_bytes_hex=4b44533305000000af0000003741000108000000010000000600000055565758595a5b5c5d5e5f606162636465666768696a6b6c6d6e6f707172737475767778050000000f192385
state_hash_verified=true
smoke_ok=true
```

Observed rejection probe output from the same smoke runner:

```text
event_log_events=8
event_log_accepted=5
event_log_rejected=3
event_log_canonical_tick_mismatch=1
event_log_invalid_state_snapshot=2
bad_tick_probe_performed=true
bad_tick_probe_tick=3
bad_tick_probe_canonical_tick=1
bad_tick_probe_canonical_outpoint=2d9bd8d5879af7b6386305d7b78eae4a147341c0a5a5f4942b92b6a2477aab0b:0
bad_state_probe_performed=true
bad_state_probe_canonical_tick=1
bad_state_probe_canonical_outpoint=2d9bd8d5879af7b6386305d7b78eae4a147341c0a5a5f4942b92b6a2477aab0b:0
bad_state_ticcmd_probe_performed=true
bad_state_ticcmd_probe_canonical_tick=1
bad_state_ticcmd_probe_canonical_outpoint=2d9bd8d5879af7b6386305d7b78eae4a147341c0a5a5f4942b92b6a2477aab0b:0
corrupt_resume_probe_performed=true
state_canonical_tick=5
state_canonical_outpoint=40d938ee420234c89a6871482dd07100e415ded36355bfd13445949bb7cfa9e1:0
smoke_ok=true
```

This is still a dry-run bridge chain because the selected Toccata endpoint does not show a spendable wallet UTXO, but it now proves the browser-start-compatible path can advance and persist a multi-tic canonical sequence from one synthetic tick-0 state. The smoke runner kills and restarts the bridge after canonical tick 3, verifies that the reloaded `/state` tuple matches the persisted tick-3 successor, then continues to tick 5. The final `/state` tuple matches the fifth successor outpoint, state hash, and exact `KDS4` state bytes. The smoke runner also verifies that every accepted event's `ticcmdHex` and `stateHash` match the exact deterministic `ticcmd` and Blake2b hash of the 96-byte `KDS4` state snapshot sent for that tick. The bridge rejects omitted, wrong-length, non-`KDS4`, wrong-tick, or wrong-ticcmd state snapshots before transaction construction, and the corrupt-resume probe verifies the bridge refuses to start from a state file whose persisted KDS4 bytes no longer hash back to the stored state hash. Accepted dry-run tics can no longer fall back to submitter-synthesized state. `smoke_tps` is local dry-run wall-clock throughput; `captured_tps` is the authoritative game cadence encoded into the event log and consumed by `doom_tn10_report`.

For a repeatable local 10 TPS ceiling check before live Toccata funding is visible, run the smoke path without the expensive negative probes and require the bridge to beat the target:

```bash
KASPA_TN10_MNEMONIC='<testnet mnemonic from local shell history or password manager>' \
cargo run -q -p silverscript-lang --bin doom_tn10_smoke -- \
  --listen 127.0.0.1:8840 \
  --url wss://testnet10-wrpc.kasia.fyi \
  --timeout-ms 45000 \
  --ticks 20 \
  --target-tps 10 \
  --captured-tps 10 \
  --submit-backend in-process \
  --require-smoke-vs-target 1.0
```

The smoke runner prints `state_bytes_per_tick`, `target_state_bytes_per_second`, `captured_state_bytes_per_second`, and `smoke_state_bytes_per_second`. With the current compact `KDS4` state, 10 TPS means 96 bytes/tic and a 960 bytes/sec canonical state stream before transaction overhead. This is not a network claim; it proves the local browser-shaped bridge path can ingest, validate, persist, and link one exact state snapshot per tic at or above the requested cadence. The live network ceiling still has to be measured after a Toccata-visible wallet UTXO can fund the genesis and successor transactions.

For full-state planning, `doom_state_budget --target-tps 10` reports:

```text
compact_state_bytes_per_tic=96
compact_state_bytes_per_second=960.0000
full_state_bytes_per_tic=180224
full_chunks_per_tic=347
chunked_transitions_per_tic=347
chunked_transition_tps=3470.0000
chunked_transitions_per_block=347.0000
chunked_seconds_per_full_tic_at_kaspa_bps=34.7000
one_tx_per_tic_full_state_feasible=false
```

Latest local 10 TPS dry-run evidence after `KDS4`:

```text
ticks_accepted=20
state_bytes_per_tick=96
target_tps=10.000
target_state_bytes_per_second=960.000
smoke_tps=37.736
smoke_vs_target=3.774
smoke_state_bytes_per_second=3622.681
smoke_ok=true
```

The kept event log can be summarized with the live report tool:

```bash
cargo run -q -p silverscript-lang --bin doom_tn10_report -- \
  --event-log .doom-tn10-smoke-4188459.events.jsonl \
  --target-tps 1
```

For automation, the same validated report can be emitted as JSON:

```bash
cargo run -q -p silverscript-lang --bin doom_tn10_report -- \
  --event-log .doom-tn10-bridge-events.jsonl \
  --target-tps 1 \
  --emit-json
```

The JSON summary includes the text report metrics in camelCase, including `acceptedTps`, `acceptedVsTarget`, `rejectionCounts`, `stateSnapshotVerifiedCount`, `acceptedTxidVerifiedCount`, `acceptedOutpointLinkVerifiedCount`, state byte length/throughput metrics, decoded `latestKds4` metadata, the resume tuple, and `bridgeResumeCommand`. The text report prints the same decoded fields as `latest_kds4_*`, including level time, PRNG indexes, player mask, mobj count, four hash slots, special thinker count, map counts, and level totals. The report performs the same hash, KDS4, and outpoint-link validation before printing JSON.

Observed report output from the same smoke log:

```text
mode=doom-tn10-report
events=8
accepted_events=5
rejected_events=3
unique_txids=5
duplicate_txids=0
rpc_submit_skipped=5
first_browser_tick=1
last_browser_tick=5
first_canonical_tick=1
last_canonical_tick=5
canonical_non_monotonic_steps=0
browser_non_monotonic_steps=0
last_successor_outpoint=40d938ee420234c89a6871482dd07100e415ded36355bfd13445949bb7cfa9e1:0
last_ticcmd_hex=050000000f192385
last_state_hash=cc3d62a8b15783172cc4181c28bd699720cc566b9ba13cdf9b449ac999e6744a
last_state_bytes_hex=4b44533305000000af0000003741000108000000010000000600000055565758595a5b5c5d5e5f606162636465666768696a6b6c6d6e6f707172737475767778050000000f192385
state_bytes_present_count=5
latest_state_bytes_len=96
state_bytes_min_len=96
state_bytes_avg_len=96.00
state_bytes_max_len=96
target_state_bytes_per_second=96.00
accepted_state_bytes_per_second=96.00
state_hash_verified_count=5
captured_duration_ms=4000.00
accepted_tps=1.0000
accepted_vs_target=1.0000
rejection_count[canonical_tick_mismatch]=1
rejection_count[invalid_state_snapshot]=2
resume_tuple_prev_tick=5
resume_tuple_input_outpoint=40d938ee420234c89a6871482dd07100e415ded36355bfd13445949bb7cfa9e1:0
resume_tuple_prev_ticcmd_hex=050000000f192385
resume_tuple_prev_state_hash_hex=cc3d62a8b15783172cc4181c28bd699720cc566b9ba13cdf9b449ac999e6744a
resume_tuple_prev_state_bytes_hex=4b44533305000000af0000003741000108000000010000000600000055565758595a5b5c5d5e5f606162636465666768696a6b6c6d6e6f707172737475767778050000000f192385
bridge_resume_command=cargo run -p silverscript-lang --bin doom_tn10_bridge -- --input-txid 40d938ee420234c89a6871482dd07100e415ded36355bfd13445949bb7cfa9e1 --input-index 0 --wallet-address kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz --prev-tick 5 --prev-ticcmd-hex 050000000f192385 --prev-state-hash-hex cc3d62a8b15783172cc4181c28bd699720cc566b9ba13cdf9b449ac999e6744a --prev-state-hex 4b44533305000000af0000003741000108000000010000000600000055565758595a5b5c5d5e5f606162636465666768696a6b6c6d6e6f707172737475767778050000000f192385
```

Observed report output from the rejection probe log:

```text
events=8
accepted_events=5
rejected_events=3
canonical_non_monotonic_steps=0
browser_non_monotonic_steps=0
accepted_tps=1.0000
accepted_vs_target=1.0000
rejection_count[canonical_tick_mismatch]=1
rejection_count[invalid_state_snapshot]=2
resume_tuple_prev_tick=5
resume_tuple_input_outpoint=40d938ee420234c89a6871482dd07100e415ded36355bfd13445949bb7cfa9e1:0
```

Repeatable local verification for this dry-run contract is covered by:

```bash
cargo check -p silverscript-lang --bin doom_tn10_smoke --bin doom_tn10_bridge --bin doom_tn10_submitter --bin doom_tn10_genesis_plan --bin tn10_status_probe --bin tn10_wallet_key_check
cargo test -p silverscript-lang --test doom_apps_tests -- --nocapture
```

The Doom app tests now assert:

- `doom_tn10_submitter --next-state-hex ...` commits caller-provided state bytes and prints the expected `next_state_hash` and `next_state_hex`.
- `doom_tn10_submitter --write-bridge-state ...` writes the final accepted/dry-run successor as a bridge-compatible, wallet-bound resume state.
- `doom_tn10_submitter` uses the shared `silverscript_lang::doom_tn10` transition builder, keeping CLI and future in-process bridge submission on one transaction-construction path.
- `doom_tn10_report` summarizes a dry-run event log into the resume tuple needed by the browser/bridge restart path.
- `doom_tn10_report --write-bridge-state ...` writes the bridge-compatible state JSON needed for restart without manual tuple copying.
- `doom_state_budget --target-tps 10` quantifies current KDS4 bandwidth and the chunk count for a vanilla-savegame-sized full-state target.
- `doom_state_manifest` produces deterministic full-state chunk hashes, a compact manifest root, and verified chunk inclusion proofs for checkpoint/challenge designs.
- `doom_tn10_report` accepts optional checkpoint manifest roots in accepted events and validates their root/size/chunk metadata.
- `doom_tn10_report` preserves rejected payload classes such as `canonical_tick_mismatch` and `invalid_state_snapshot`.

The smoke runner also verifies the bridge `/ready` contract before posting `/start`. In dry-run bridge mode it requires:

```text
ready_start_mode=synthetic_dry_run
ready_start_available=true
ready_submit=false
ready_wallet_key_available=true|false
ready_wallet_key_matches=true|false
```

This keeps the browser start button semantics covered by the same end-to-end smoke path that verifies start -> tic -> reportable canonical state.

## Public Endpoint Scan Notes

Additional public candidate tested:

```text
wss://testnet10-wrpc.kaspa.fyi/kaspa/testnet-10/wrpc/borsh
```

Current result from this machine:

```text
failed to lookup address information: Name or service not known
nslookup: NXDOMAIN for testnet10-wrpc.kaspa.fyi
```

Working public Toccata endpoint:

```text
wss://testnet10-wrpc.kasia.fyi
```

Probe result:

```text
server_version=1.2.0-toc.2
is_synced=true
has_utxo_index=true
virtual_daa_score=471805684
wallet_utxo_count=0
wallet_balance_kas=0.00000000
doom_expected_genesis_visible=false
```

This endpoint is suitable for covenant opcode submission once the wallet/game UTXO is visible there. The paired `https://testnet10-grpc.kasia.fyi` endpoint answers as gRPC over HTTP/2 (`content-type: application/grpc`, `grpc-status: 12` with no method), but the current prototype tools use wRPC/Borsh.

`doom_tn10_live --auto-select-endpoint` now includes this endpoint in its default candidate list. Latest auto-selection evidence:

```text
candidate_url=wss://testnet10-wrpc.kasia.fyi
candidate_connected=true
candidate_server_version=1.2.0-toc.2
candidate_is_synced=true
candidate_has_utxo_index=true
candidate_is_toccata=true
candidate_wallet_funded=false
candidate_selectable=true
candidate_funded_selectable=false
selected_endpoint=wss://testnet10-wrpc.kasia.fyi
selected_reason=synced_toccata_utxoindex
```

The subsequent submit preflight still fails because neither the old genesis UTXO nor the wallet funds are visible at this endpoint:

```text
readiness_old_genesis_visible=false
readiness_wallet_funded=false
preflight_input_visible=false
```

The configured wallet address is funded on current PNN-resolved public TN10 `1.1.0` endpoints:

```text
pnn_resolved_url=wss://proton-10.kaspa.stream/kaspa/testnet-10/wrpc/borsh
server_version=1.1.0
is_synced=true
wallet_utxo_count=1
wallet_balance_kas=99895.99950727
wallet_largest_utxo_txid=44958358cd848ef43c91112738ef8be5ffed574982e152be98913eeb94b726c0
wallet_largest_utxo_index=1
```

The same address is not funded on the synced public Toccata endpoint:

```text
url=wss://testnet10-wrpc.kasia.fyi
server_version=1.2.0-toc.2
is_synced=true
wallet_utxo_count=0
wallet_balance_kas=0.00000000
```

This explains the apparent wallet mismatch: the seed/address can control the visible `1.1.0` UTXO, but Toccata covenant transactions must be funded from UTXOs visible to a Toccata node. Signing material alone cannot spend an output that the selected Toccata endpoint does not have in its UTXO set.

## Compact State Snapshot Evidence

The current browser/bridge/submitter path can now commit caller-provided compact `KDS4` state bytes instead of having the Rust submitter invent unrelated state:

The browser page now treats wallet/start as an explicit session gate. Doom can render and emit engine tics immediately, but `kaspaDoom.onTic` will not queue a canonical Kaspa tic until `/start` succeeds or `/state` resumes a non-genesis canonical bridge state. Before that, the panel reports `waiting_start` and logs `waiting for Kaspa game start before committing Doom state`. This keeps the intended flow aligned with the target UX: enter or connect a TN10 wallet, refresh bridge readiness, start/deploy or resume the game-state UTXO, then commit Doom state snapshots.

The HTTP bridge enforces the same gate. `/state` exposes `started=false` before `/start` and `started=true` after `/start` or resume. A pre-start `POST /tic` is rejected with `rejectionClass=session_not_started`, and the smoke runner verifies that rejection before calling `/start`. The latest smoke run reported `prestart_state_started=false`, `poststart_state_started=true`, `restart_loaded_started=true`, `state_started=true`, `event_log_session_not_started=1`, and `event_log_wallet_mismatch=1`, then accepted the three post-start canonical tics and preserved the existing bad-tick, bad-state, corrupt-resume, CLI-resume, and live-preflight probes.

```bash
cargo run -q -p silverscript-lang --bin doom_tn10_submitter -- \
  --ticks 1 \
  --next-ticcmd-hex 0102030405060708 \
  --next-state-hex 4b44533301000000230000000b0d00010400000001000000020000001112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f30313233340102030405060708
```

Observed dry-run fields:

```text
next_ticcmd_hex=0102030405060708
next_state_len=96
next_state_hash=1cd66f1b45d9ae5ecc75001efbd61771d5507f7db3a188ed3a9fcfa421ee5104
next_state_hex=4b44533301000000230000000b0d00010400000001000000020000001112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f30313233340102030405060708
local_validation=ok
rpc_submit=skipped
```

The submitter rejects wrong-length, non-`KDS4`, wrong-tick, or wrong-ticcmd explicit successor states before transaction construction. The same snapshot sent through `POST /tic` as JSON `stateBytes` reaches the child submitter and returns the same `next_state_hash`. This proves the browser payload, bridge, and covenant transaction builder are aligned around engine-supplied state bytes.

The WASM-side snapshot format has since been expanded to 96 bytes. It now uses a `KDS4` marker and includes committed tic, `leveltime`, `prndindex`, `rndindex`, active player, player mask, max/live player counts, mobj count, an FNV-1a digest over deterministic player fields, an FNV-1a digest over the mobj thinker list, an FNV-1a digest over the same world sector/line/side fields archived by `P_ArchiveWorld`, an FNV-1a digest over active special thinkers such as ceilings, doors, floors, plats, flashes, strobes, and glows, explicit special thinker count, sector count, line count, side count, total kills, total items, total secrets, plus the exact emitted 8-byte ticcmd at offset 88. The bridge verifies the embedded committed tic and embedded ticcmd before hashing the state into the successor covenant transaction. This is still a compact commitment rather than full replayable Doom RAM, but its coverage now follows the major Chocolate Doom savegame archive categories: players, world, thinkers, and specials while making key map totals explicit. The next step is replacing or supplementing these category hashes with real serialized bytes from Chocolate Doom savegame/state code.

## Current Live TN10 Toccata State

The active public endpoint is:

```text
wss://testnet10-wrpc.kasia.fyi
```

The current funded Toccata wallet is:

```text
kaspatest:qzma22j09zrjn5zxw8mx3epm49etfma9y9jc6z80g43mwyk0svvg6h34ars7t
```

The earlier `kaspatest:qp6v08cef4v2eyn9lxsrnvjplytemr53gdmwuyjc2gprj6lfl22jx03u0fefz` wallet remains useful evidence for the fork/view split, but it is not the active Toccata deployment wallet because the synced public Toccata endpoint sees it as empty.

Fresh live deployment evidence from the funded wallet:

```text
genesis_tx_id=797795b6264268cccf8ece9ebc834b3b097f391beade52513ac9f034eb512aa7
initial_game_outpoint=797795b6264268cccf8ece9ebc834b3b097f391beade52513ac9f034eb512aa7:0
covenant_id=e2ea758bb259f59f38ab1c4ef847d5ca4f0a3445849e350c10a7f0a5e2d7dcab
initial_utxo_value_sompi=5000000000
fee_per_transition_sompi=20000000
```

The first tic was submitted from that fresh genesis and accepted:

```text
tick=1
tx_id=8f74fbad27463f904e0daa4ff28f0dab37c63dc958f61cce5d80c2c8c88ed190
successor_outpoint=8f74fbad27463f904e0daa4ff28f0dab37c63dc958f61cce5d80c2c8c88ed190:0
successor_utxo_value_sompi=4980000000
accepting_block_hash=8b96b1eb99b2b96ee32381eb1a1c1a156a39ce40513986373a7e1c76c3f02212
```

The chain was then resumed from tick 1 and advanced through tick 6 at a 1 TPS target:

```text
ticks_requested=5
ticks_accepted=5
start_prev_tick=1
final_tick=6
final_successor_outpoint=0a8cdb154cd404518d5ffb80579a7e27a1936ba2c6be2d1b5c27dbb1fdb25035:0
final_successor_utxo_value_sompi=4880000000
observed_submit_tps=0.5179
```

The local bridge was started from the tick-6 state file and a browser-shaped tic payload was submitted through HTTP `POST /tic` using:

```bash
cargo run -q -p silverscript-lang --bin doom_browser_tic_harness -- \
  --bridge-url http://127.0.0.1:8787 \
  --wallet-address kaspatest:qzma22j09zrjn5zxw8mx3epm49etfma9y9jc6z80g43mwyk0svvg6h34ars7t \
  --ticks 1 \
  --target-tps 1
```

This harness follows the same browser contract as `vendor/doom-wasm/src/index.html`: `GET /state`, send `tick`, `walletAddress`, `canonicalOutpoint`, `ticcmd`, `stateBytes`, and wait for the bridge's accepted canonical tuple. It is not a replacement for the compiled WASM Doom client, but it proves the browser/bridge HTTP payload shape against the live covenant chain.

Accepted browser-shaped tick evidence:

```text
tick=7
tx_id=800b6e93153b941a0a2c658c83c9f666baf489cc5acdcdea8b525b540cba0f46
successor_outpoint=800b6e93153b941a0a2c658c83c9f666baf489cc5acdcdea8b525b540cba0f46:0
successor_utxo_value_sompi=4860000000
state_hash=feeded616eb73e0caed95b592bfa36a2a608173add1072eada3306ed3eac3de1
mempool_seen=true
inclusion_seen=true
accepting_block_hash=46925de1e48cff63ecc620e2ffc73b6adba04d45395fd76959a4e53e860dc216

tick=8
tx_id=6b09057dc6fec9dfdf1fd39fa8ea8307122d965993e504525aedfd3b976776b5
successor_outpoint=6b09057dc6fec9dfdf1fd39fa8ea8307122d965993e504525aedfd3b976776b5:0
successor_utxo_value_sompi=4840000000
state_hash=7c1ccf19e1fa7742b689897df362046bca07d00d4e42dca8971000998896f5d6
mempool_seen=true
inclusion_seen=true
accepting_block_hash=20bb2af335b7ca4f32f77e10f02f995a19a77f9d7cd21102f94a11bf8c0138af
```

The latest local bridge state is written to `.doom-tn10-bridge-state.json` and now includes `utxo_value`. That value is required for correct resume after fee-burning covenant spends:

```json
{
  "input_txid": "6b09057dc6fec9dfdf1fd39fa8ea8307122d965993e504525aedfd3b976776b5",
  "input_index": 0,
  "utxo_value": 4840000000,
  "prev_tick": 8,
  "wallet_address": "kaspatest:qzma22j09zrjn5zxw8mx3epm49etfma9y9jc6z80g43mwyk0svvg6h34ars7t",
  "prev_ticcmd_hex": "0800000000000000",
  "prev_state_hash_hex": "7c1ccf19e1fa7742b689897df362046bca07d00d4e42dca8971000998896f5d6",
  "covenant_id": "e2ea758bb259f59f38ab1c4ef847d5ca4f0a3445849e350c10a7f0a5e2d7dcab"
}
```

Resume command shape:

```bash
cargo run -p silverscript-lang --bin doom_tn10_bridge -- \
  --url wss://testnet10-wrpc.kasia.fyi \
  --state-file .doom-tn10-bridge-state.json \
  --event-log .doom-tn10-bridge-events.jsonl \
  --submit true \
  --wait-preflight \
  --preflight-timeout-ms 30000 \
  --preflight-poll-ms 1000 \
  --track-inclusion true \
  --rpc-timeout-ms 12000
```

The current browser play-test path is:

```bash
python3 -m http.server 8790 --directory vendor/doom-wasm/src
```

Then open:

```text
http://127.0.0.1:8790/index.html
```

The Docker/Emscripten build command that produced the current browser artifacts is:

```bash
docker run --rm \
  -v /home/luke/projects/silverscript/vendor/doom-wasm:/src \
  -w /src \
  emscripten/emsdk:latest \
  bash -lc 'apt-get update >/dev/null && apt-get install -y autoconf automake libtool pkg-config make python3 >/dev/null && ./scripts/build.sh'
```

The current generated artifacts are `vendor/doom-wasm/src/websockets-doom.js`, `vendor/doom-wasm/src/websockets-doom.wasm`, `vendor/doom-wasm/src/websockets-doom.wasm.map`, and `vendor/doom-wasm/src/websockets-doom.html`. Local play-testing currently uses `freedoom1.wad` copied to `vendor/doom-wasm/src/doom1.wad`; WAD files are ignored by git. Replacing that file with a legitimate official Doom IWAD should work with the same page.

The WASM build had to move from the removed Emscripten `EXTRA_EXPORTED_RUNTIME_METHODS` setting to `EXPORTED_RUNTIME_METHODS`, add `src/doom` to the C include path for the Kaspa bridge, and disable `SAFE_HEAP`. With `SAFE_HEAP=1`, headless Chrome loaded the WAD but aborted during startup with `Aborted(alignment fault)` after `NET_Init`. With `SAFE_HEAP=0`, headless Chrome reaches:

```text
doom: 10, game started
Running emscripten_set_main_loop()
```

Headless Chrome does not prove the canvas renders real gameplay frames because its WebGL texture limit reported `0x0`, but the generated browser page loads the WAD, starts the engine, and the Kaspa panel resumes the live tick-8 bridge state. The remaining live browser proof is to run a normal browser session, verify visible gameplay, and confirm `kaspa-doom:tic` callbacks from the real WASM loop submit tick 9 and later to TN10.

## Next Implementation Work

1. Run a normal browser session at `http://127.0.0.1:8790/index.html`, verify visible gameplay, and commit browser-emitted `KDS4` tics from tick 9 onward through the live bridge.
2. Add browser/runtime diagnostics for missing or stalled `kaspaDoomOnTic` callbacks so the UI can distinguish render startup from authoritative tic emission.
3. Measure sustained cadence at 1 TPS for a longer run, then probe 2, 5, and 10 TPS to identify chained-spend, mempool, inclusion, and bridge overhead bottlenecks.
4. Replace the compact category-hash snapshot with real serialized Doom state from Chocolate Doom savegame/state code, or add slower full-state chunk manifests as checkpoints alongside every-tic `KDS4` commitments.
5. Strengthen `DoomState.advance` beyond layout and monotonic checks toward transition proofs, challenge data, or additional cheap invariants.
6. Keep wallet secrets out of source/config/log files; mnemonics should only be passed through environment variables for testnet tooling.
