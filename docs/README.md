# L2 Order Book Engine

> A Level-2 order book for High-Frequency Trading, written in Rust.  
> Sub-nanosecond BBO reads. Sub-microsecond updates. Zero heap allocations on the hot path.

---

## Performance at a Glance

All numbers measured with `Criterion.rs` in `--release --features "no_checksum"` on an AMD Ryzen 5 8600G (Zen 4) @ 5.05 GHz.

| Operation                           | Latency             | Notes                                           |
|-------------------------------------|---------------------|-------------------------------------------------|
| `best_bid()` / `best_ask()`         | **~1.20 ns**        | O(1) — direct cached index read                 |
| `mid_price()`                       | **~798 ps**         | O(1) — two cached reads, one add                |
| `orderbook_imbalance()`             | **~804 ps**         | O(1) — two BBO qty reads, ratio                 |
| `orderbook_imbalance_depth(5)`      | **~7.41 ns**        | O(k) — walks 5 present levels                   |
| `orderbook_imbalance_depth(10)`     | **~14.6 ns**        | O(k) — walks 10 present levels                  |
| `top_bids(10)` / `top_asks(10)`     | **~35 ns**          | O(k) local scan from BBO outward                |
| `update()` — single (depth 20)      | **~64–71 ns**       | Production mode — O(1) sequence check           |
| `update()` — batch 100, amortised   | **~73 ns/msg**      | Near-flat; no per-batch overhead                |
| `update()` — depth 5 (sparse)       | **~21–22 ns**       | Fewer diffs per message                         |
| `update()` — depth 50 (dense)       | **~152–159 ns**     | More diffs per message                          |

The entire hot data set — both sides combined — fits in approximately **33 KiB**, designed to reside entirely within a 32 KiB L1d cache. All numbers are Criterion medians from a live `--release --features "no_checksum"` build on the reference hardware (AMD Ryzen 5 8600G (Zen 4) @ 5.05 GHz). Benchmark-mode figures (with Adler32 checksum) are documented in `docs/BENCHMARKS.md`.

---

## Architecture Overview

- **Ring buffer per side** — `CAP = 4096` slots of `f32` quantities, indexed by a relative offset from a movable anchor price. Physical index is `(head + rel) & CAP_MASK`, a single bitwise AND with no division.
- **Occupancy bitset** — `Box<[u64; 64]>` (512 bytes per side) tracks which price levels are live. Setting or clearing a level is a single word write; scanning for the best price walks at most 64 words.
- **Cached best price** — `best_rel: usize` stores the relative index of the current best bid or ask. `best_bid()` is literally one memory load and an anchor addition — no scan required on the common path.
- **Hot/cold data split** — `HotData` (`#[repr(align(64))]`) groups every field touched per-tick. `ColdData` holds metadata that changes rarely. Separation prevents cold fields from evicting hot cache lines.
- **Smart-shift recentering** — when the market drifts toward the edge of the window, `recenter()` (marked `#[cold]`) shifts the `head` pointer and clears only the physical slots that leave the window. Cost is O(|shift|), not O(CAP).
- **Dual-mode validation** — a feature flag (`no_checksum`) swaps Adler32 checksum logic for a single integer sequence-continuity check, giving the same codebase both a benchmarkable mode and a production mode.
- **Synthetic feed** — `MarketFeedSimulator` drives the book with geometric Brownian motion for mid-price, log-normal for level sizes, and a stochastic spread model — realistic enough for integration testing and throughput profiling.

---

## Repository Structure

```
orderbook/
├── benches/
│   ├── orderbook_update.rs          # book_update, book_update_by_depth,
│   │                                #   book_read_ops, simulator_throughput
│   └── book_benchmarks.rs           # book_update_latency, book_read_latency,
│                                    #   book_depth_scaling
├── docs/
│   ├── README.md                    # this file
│   ├── BENCHMARKS.md                # full performance report
│   ├── STRUCTURE.md                 # implementation
│   └── TESTS.md                     # test validation report
├── src/
│   ├── bin/
│   │   └── plot_orderbook.rs        # BBO time-series plotting binary
│   ├── common/                      # shared primitives
│   │   ├── messages.rs              # L2UpdateMsg, L2Diff, MsgType
│   │   ├── types.rs                 # Price, Qty, Side type aliases
│   │   └── mod.rs
│   ├── engine/                      # market data simulation
│   │   ├── simulator.rs             # MarketFeedSimulator, FeedConfig
│   │   └── mod.rs
│   ├── optimized/                   # the order book implementation
│   │   ├── book.rs                  # L2Book (ring buffer + bitset)
│   │   └── mod.rs
│   ├── lib.rs
│   └── main.rs                      # WebSocket server (axum/tokio)
├── Cargo.toml
└── Cargo.lock
```

---

## Quick Start

### Build

```bash
cd orderbook

# Debug build
cargo build

# Release build (required for meaningful benchmark results)
cargo build --release
```

### Run Unit Tests

```bash
cargo test
```

### Run Benchmarks

```bash
# Full benchmark suite (takes several minutes)
cargo bench

# Benchmark suite with Adler32 checksum enabled (default mode)
cargo bench --bench orderbook_update

# Benchmark suite in production mode — O(1) sequence check instead of checksum
cargo bench --bench orderbook_update --features "no_checksum"

# HTML report
# Open: target/criterion/report/index.html
```

### Run the WebSocket Server

The server streams live synthetic orderbook state over a WebSocket connection.

```bash
cargo run --release --bin orderbook_server
# Listening on ws://127.0.0.1:3000
```

### Generate the BBO Plot

Runs the book for a fixed number of steps, records BBO to CSV, and renders a PNG chart.

```bash
cargo run --release --bin plot_orderbook
# Writes: orderbook_bbo_log.csv
#         orderbook_timeseries.png
```

---

## Usage Examples

### Creating a Book and Applying Updates

```rust
use orderbook::optimized::book::L2Book;
use orderbook::common::types::{Price, Qty, Side};
use orderbook::common::messages::{L2Diff, L2UpdateMsg, MsgType};

fn main() {
    // tick_size = 0.01 (1 cent), lot_size = 0.001
    let mut book = L2Book::new(0.01, 0.001);

    // Build an initial snapshot
    let snapshot = L2UpdateMsg {
        msg_type: MsgType::L2Update,
        symbol: "ETH-USDT".to_string(),
        ts: 1_700_000_000_000,
        seq: 1,
        diffs: vec![
            L2Diff { side: Side::Bid, price_tick: 180_000, size: 4.5 },
            L2Diff { side: Side::Bid, price_tick: 179_990, size: 12.0 },
            L2Diff { side: Side::Ask, price_tick: 180_010, size: 3.2 },
            L2Diff { side: Side::Ask, price_tick: 180_020, size: 8.7 },
        ],
        checksum: 0,
    };

    book.update(&snapshot, "ETH-USDT");

    // Best bid / ask — O(1), sub-nanosecond
    if let Some((px, qty)) = book.best_bid() {
        println!("Best bid: price_tick={px}, qty={qty:.4}");
    }
    if let Some((px, qty)) = book.best_ask() {
        println!("Best ask: price_tick={px}, qty={qty:.4}");
    }
}
```

### Mid-Price and Spread

```rust
// Both calls are O(1) reads of cached BBO state
if let Some(mid) = book.mid_price() {
    println!("Mid price: {mid:.6}");
}

if let Some(spread) = book.spread() {
    println!("Spread: {spread:.6}");
}

if let Some(spread_ticks) = book.spread_ticks() {
    println!("Spread in ticks: {spread_ticks}");
}
```

### Depth Queries

```rust
// Collect the top 5 levels on each side
// Complexity: O(k) where k = ticks scanned from BBO outward
let bids = book.top_bids(5);
let asks = book.top_asks(5);

println!("--- Depth Snapshot ---");
for (px, qty) in &bids {
    println!("  BID  {px:>10}  {qty:.4}");
}
for (px, qty) in asks.iter().rev() {
    println!("  ASK  {px:>10}  {qty:.4}");
}
```

### Order Book Imbalance

```rust
// Imbalance over the top N levels: (bid_vol - ask_vol) / (bid_vol + ask_vol)
// Returns None if either side is empty
if let Some(imb) = book.orderbook_imbalance_depth(10) {
    let pct = imb * 100.0;
    println!("10-level imbalance: {pct:+.1}%");  // positive = bid-heavy
}
```

### Incremental Diff Updates (tick-by-tick)

```rust
// Apply a small diff — only changed levels need to be sent
let tick = L2UpdateMsg {
    msg_type: MsgType::L2Update,
    symbol: "ETH-USDT".to_string(),
    ts: 1_700_000_001_000,
    seq: 2,
    diffs: vec![
        // Remove best ask entirely
        L2Diff { side: Side::Ask, price_tick: 180_010, size: 0.0 },
        // Add a new best ask tighter than before
        L2Diff { side: Side::Ask, price_tick: 180_005, size: 1.1 },
    ],
    checksum: 0,
};

book.update(&tick, "ETH-USDT");

// BBO has updated O(1)
println!("New best ask: {:?}", book.best_ask());
```

### Using the Market Feed Simulator

```rust
use orderbook::engine::{FeedConfig, MarketFeedSimulator};

let config = FeedConfig {
    symbol: "BTC-USDT".to_string(),
    tick_size: 0.1,
    lot_size: 0.001,
    depth: 20,
    dt_ms: 10,
    sigma_daily: 0.02,  // 2% daily volatility (GBM)
};

let mut sim = MarketFeedSimulator::with_config(config);
let _bootstrap = sim.bootstrap_update();  // initial snapshot

for _ in 0..1000 {
    let msg = sim.next_update();
    // process msg...
    let _ = msg;
}
```

---

## Design Highlights

### 1. Ring Buffer with Anchor Indexing

Each side of the book holds a flat `Box<[f32; 4096]>` array — no heap fragmentation, no pointer chasing. A price tick is mapped to a relative offset from the current `anchor`:

```
rel = price_tick - anchor          (asks, price rises with rel)
rel = anchor - price_tick          (bids, price falls with rel)
phys = (head + rel) & 0xFFF        (O(1) via bitmask, CAP is power-of-two)
```

Storing quantities as `f32` halves the memory footprint vs `f64`, allowing twice as many levels per cache line.

### 2. Occupancy Bitset

A `Box<[u64; 64]>` (512 bytes) shadows the quantity array. Bit `i` is set when `qty[i] > EPS`. Scanning for the best price when the BBO is removed costs at most 64 iterations of `u64::trailing_zeros` — effectively a fixed-cost O(1) operation regardless of how many levels are live.

### 3. Hot / Cold Data Split

`HotData` (marked `#[repr(align(64))]`) bundles only the fields touched on every incoming tick: the quantity array pointer, the bitset pointer, `head`, `anchor`, and `best_rel`. Cold metadata (`tick_size`, `lot_size`, `seq`, `initialized`) lives in a separate `ColdData` struct. This keeps every cache line loaded during `update()` and `best_bid()` useful — no evictions from incidental metadata reads.

Combined hot footprint: **~16.5 KiB per side → ~33 KiB for both sides**, fitting snugly within the 32 KiB L1d cache of the target CPU (Ryzen 5 8600G).

### 4. Recentering Without Full Copies

When the market price approaches the edge of the 4096-tick window, `recenter()` slides the window. It is marked `#[cold]` to hint the compiler to keep it out of the hot-path instruction cache. The algorithm:

1. Computes `shift` — the number of ticks to move the anchor.
2. Adjusts `head` by `shift` (wrapping arithmetic on `usize`).
3. Calls `clear_band_phys` to zero **only** the physical slots that just left the window — O(|shift|) work, not O(4096).
4. Falls back to a full reseed (wipe and reinitialise) when `|shift| >= CAP`, covering flash-crash or extreme-gap scenarios.

This design guarantees that no stale quantity ever gets silently reassigned to a new price — a correctness property the test suite explicitly verifies.

### 5. Dual-Mode Validation via Feature Flag

The `no_checksum` cargo feature controls which validation runs inside `update()`:

- **Default (benchmark mode):** Computes an Adler32 hash of the current BBO state and compares it against `msg.checksum`. This exercises the full BBO read path on every update, making benchmark numbers directly comparable to any implementation that performs equivalent work.
- **`--features no_checksum` (production mode):** Replaces the hash with `msg.seq == self.cold.seq + 1` — a single integer comparison. This is the correct production check; it costs under 1 ns and eliminates all hash overhead from the hot path.

---

## Tradeoffs

| Concern              | Behaviour                                                                                  |
|----------------------|--------------------------------------------------------------------------------------------|
| **Price window**     | Fixed at 4096 ticks. Moves outside this range trigger `recenter`; moves >= 4096 ticks in one step trigger a full reseed. |
| **Quantity precision** | `f32` (7 significant digits). Sufficient for typical HFT lot sizes; `f64` users would need to change the type and accept 2× memory cost. |
| **Memory footprint** | Fixed ~33 KiB whether 1 or 4000 levels are live. Sparse books pay a fixed overhead; dense books get the best deal. |
| **Code complexity**  | The anchor arithmetic, bitset management, and recenter logic require careful reasoning. The 14-test suite exists precisely to guard these invariants. |
| **Recenter latency** | Rare `recenter()` calls take O(|shift|) ns — visible as occasional latency spikes in live telemetry. The `recenter_count` instrumentation field tracks frequency under debug builds. |

---

## Technologies

| Category              | Tool / Crate                                        |
|-----------------------|-----------------------------------------------------|
| Language              | Rust 1.70+                                          |
| Async runtime         | `tokio` 1.36 (full features)                        |
| WebSocket server      | `axum` 0.7                                          |
| Serialisation         | `serde` + `serde_json`                              |
| Checksum              | `adler` 1.0 (Adler32)                               |
| Integer formatting    | `itoa` 1.0 (zero-allocation int-to-str)             |
| Random / distributions | `rand` 0.8, `rand_distr` 0.4 (GBM, log-normal)   |
| Plotting              | `plotters` 0.3                                      |
| Benchmarking          | `criterion` 0.5 (HTML reports)                      |
| Build system          | Cargo                                               |
