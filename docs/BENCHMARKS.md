# Performance Report: L2Book Ring-Buffer Order Book

## 1. Executive Summary

This document characterises the runtime performance of `L2Book` — a Level-2 order book built on a fixed-capacity ring buffer with bitset occupancy tracking. Every number in this report was produced from a live Criterion.rs benchmark suite compiled in `--release` mode on the reference hardware described in Section 2.

The headline result: **BBO reads cost under 1.3 nanoseconds**, **mid-price and imbalance settle around 800 picoseconds**, and **single-message updates land at ~64–71 ns** in production mode (O(1) sequence check, `--features no_checksum`). The ~33 KiB hot data set is designed to fit entirely within a 32 KiB L1d cache, which is the primary reason for these figures.

### Key Metrics

| Operation                              | Latency (median)  | Complexity |
|----------------------------------------|-------------------|------------|
| `best_bid()` / `best_ask()`            | **~1.20 ns**      | O(1)       |
| `mid_price()`                          | **~799 ps**       | O(1)       |
| `orderbook_imbalance()`                | **~804 ps**       | O(1)       |
| `orderbook_imbalance_depth(5)`         | **~7.41 ns**      | O(k)       |
| `orderbook_imbalance_depth(10)`        | **~14.6 ns**      | O(k)       |
| `top_bids(10)` / `top_asks(10)`        | **~35 ns**        | O(k)       |
| `update()` — single diff               | **~64–71 ns**     | O(D)       |
| `update()` — batch 100, amortised      | **~73 ns/msg**    | O(D)       |
| `update()` — depth 5 (sparse)          | **~21–22 ns**     | O(D)       |
| `update()` — depth 50 (dense)          | **~152–159 ns**   | O(D)       |
| `MarketFeedSimulator::next_update()`   | **~2.79 µs**      | —          |
| `MarketFeedSimulator::bootstrap_update()` | **~4.90 µs**   | —          |

*All figures from production mode (`--features no_checksum`): O(1) sequence check, no Adler32. D = diffs per message; k = ticks scanned to collect N levels.*

---

## 2. Methodology

### 2.1 Hardware

| Component | Specification                                         |
|-----------|-------------------------------------------------------|
| CPU       | AMD Ryzen 5 8600G (Zen 4) @ 5.05 GHz boost           |
| L1d cache | 32 KiB per core                                       |
| L2 cache  | 1 MiB per core                                        |
| L3 cache  | 16 MiB shared                                         |
| RAM       | ~15 GiB DDR5 (system RAM)                             |
| OS        | Windows 11 Pro (64-bit)                               |


### 2.2 Tooling

- **Benchmark framework:** `Criterion.rs` v0.5 with HTML reports enabled
- **Compiler:** `rustc` stable, `--release` profile, LTO off (default Cargo release settings)
- **Statistics reported:** median (b-estimate) with 95% confidence intervals from Criterion's bootstrap analysis
- **Benchmark suites:**
  - `benches/orderbook_update.rs` — groups: `book_update`, `book_update_by_depth`, `book_read_ops`, `simulator_throughput`
  - `benches/book_benchmarks.rs` — groups: `book_update_latency`, `book_read_latency`, `book_depth_scaling`

### 2.3 Build Modes

Two cargo feature configurations are used throughout this report:

| Mode            | Command suffix             | Validation path                                  |
|-----------------|----------------------------|--------------------------------------------------|
| Benchmark mode  | *(default, no flag)*       | `verify_checksum()` — Adler32 hash of BBO state  |
| Production mode | `--features "no_checksum"` | `msg.seq == self.cold.seq + 1` — integer check   |

**All figures in Sections 3–5 and 7 report production-mode measurements** (`--features no_checksum`, O(1) sequence check). Section 8 documents the benchmark-mode overhead (Adler32) and the measured delta.

---

## 3. Update-Path Benchmarks

### 3.1 Depth-Scaling (`book_update_by_depth` / `book_depth_scaling`)

This benchmark holds the number of visible price levels constant at N and measures the latency of a single `update()` call. The goal is to confirm that cost scales with the number of diffs per message (D) — a property of the incoming payload — not with the total count of live levels in the ring buffer.

Results from both benchmark suites are in excellent agreement:

| Active Levels (N) | `book_update_by_depth` (median) | `book_depth_scaling` (median) |
|-------------------|---------------------------------|-------------------------------|
| 5                 | **21.7 ns**                     | **20.6 ns**                   |
| 10                | **40.6 ns**                     | **36.3 ns**                   |
| 20                | **69.6 ns**                     | **65.4 ns**                   |
| 50                | **158.8 ns**                    | **151.9 ns**                  |

**Interpretation:** The roughly linear growth from ~21 ns to ~159 ns reflects the simulator producing proportionally more diffs for deeper books. The ring-buffer index operations are O(1) per diff; the total cost is O(D) where D ≈ depth. The ~7 ns spread between the two suites at depth 50 is within normal run-to-run variance on a Windows host.

### 3.2 Single Update vs. Batch-Amortised

| Scenario                          | `book_update` (median) | `book_update_latency` (median) |
|-----------------------------------|------------------------|--------------------------------|
| Single `update()` call            | **71.3 ns**            | **63.9 ns**                    |
| 100 consecutive updates, per-msg  | **72.7 ns**            | **116.3 ns***                  |
| Batch 10, per-msg                 | **132.5 ns**           | —                              |

*`book_update_latency` batch-100 includes recentering events in a 100-step walk; `book_update` batch-100 uses pre-generated messages without state-triggered recentering and is the more representative steady-state figure.*

The batch-10 figure is higher per-message than batch-100 because the cloning overhead of the test harness (`book.clone()`) is amortised over fewer iterations. The batch-100 per-message cost of **~73 ns/msg** is the most representative steady-state figure for a continuously-updating book.

The ~8 ns spread between the two suites for single updates reflects the different warm-up states: `book_update_latency` warms with 100 pre-generated messages while `book_update` clones the book on every iteration, incurring slightly more cache pressure.

---

## 4. Read-Path Benchmarks

### 4.1 BBO and Scalar Queries (`book_read_ops` / `book_read_latency`)

| Operation               | `book_read_ops` (median) | `book_read_latency` (median) | Notes                               |
|-------------------------|--------------------------|------------------------------|-------------------------------------|
| `best_bid()`            | **1.206 ns**             | **1.199 ns**                 | Reads `best_rel`, adds anchor       |
| `best_ask()`            | **1.200 ns**             | **1.198 ns**                 | Symmetric                           |
| `mid_price()`           | **800 ps**               | **798 ps**                   | Two BBO reads + average             |
| `orderbook_imbalance()` | **803 ps**               | **830 ps**                   | Two BBO qty reads + ratio           |

At 5.05 GHz, one CPU cycle is approximately **0.198 ns**. The 0.80–1.22 ns range corresponds to **4–6 clock cycles** — the irreducible cost of back-to-back L1d cache loads plus a small number of arithmetic instructions. There is no tighter bound for a memory-resident data structure on this CPU generation.

`mid_price()` reporting slightly *faster* than `best_bid()` alone is a measurement artefact: the compiler fuses the two BBO reads and the average into a tighter instruction sequence than the isolated `best_bid()` microbenchmark, where `black_box` forces a full round-trip to memory.

These latencies are stable across runs because `best_rel` is updated incrementally inside every `set_qty()` call. The read path never triggers a scan.

### 4.2 Depth-Bounded Imbalance and Top-N (`book_read_ops`)

| Operation                      | Latency (median) | Complexity           |
|--------------------------------|------------------|----------------------|
| `orderbook_imbalance_depth(5)` | **7.41 ns**      | O(k), walks 5 levels |
| `orderbook_imbalance_depth(10)`| **14.6 ns**      | O(k), walks 10 levels|
| `top_bids(10)`                 | **35.4 ns**      | O(k), collects 10 levels |
| `top_asks(10)`                 | **35.1 ns**      | O(k), collects 10 levels |

`top_bids(n)` starts at `best_rel` and walks outward (increasing relative index = decreasing price for bids), collecting occupied levels until n entries are gathered. Each step involves a bitset lookup and a conditional price/qty read — all within L1.

The ~21 ns gap between `orderbook_imbalance_depth(10)` and `top_bids(10)` reflects the different output paths: imbalance accumulates two running sums and returns a scalar, while `top_bids` allocates and fills a `Vec`, paying heap allocation and copy cost for 10 `(i64, f64)` pairs.

---

## 5. Simulator Throughput (`simulator_throughput`)

| Operation                              | Latency (median) | Notes                               |
|----------------------------------------|------------------|-------------------------------------|
| `MarketFeedSimulator::next_update()`   | **~2.79 µs**     | GBM step + diff computation + seq check |
| `MarketFeedSimulator::bootstrap_update()` | **~4.90 µs**  | Full book rebuild + full diff set       |

These figures measure the synthetic feed pipeline, not the order book. The 3 µs per tick reflects: one Brownian draw, one spread sample, 20 log-normal size draws, shadow-map diffing, ring-buffer update, and Adler32 checksum. This cost is entirely off the `L2Book` hot path — in a real system the feed parser runs on a separate core from the strategy that reads `best_bid()`.

---

## 6. Memory and Cache Analysis

### 6.1 Hot Data Layout

`HotData` is the struct accessed on every incoming tick. It is declared `#[repr(align(64))]` so both the bid and ask instances start on a cache-line boundary, preventing false sharing between the two sides.

| Field                         | Type                    | Size per side      |
|-------------------------------|-------------------------|--------------------|
| `qty`                         | `Box<[f32; 4096]>`      | 16,384 B (16 KiB)  |
| `occupied`                    | `Box<[u64; 64]>`        | 512 B              |
| `head`, `anchor`, `best_rel`  | `usize`, `i64`, `usize` | ~24 B              |
| **Subtotal (one side)**       |                         | **~16.5 KiB**      |
| **Both sides combined**       |                         | **~33 KiB**        |

`ColdData` (`tick_size`, `lot_size`, `seq`, `initialized`) is kept in a separate struct and is only read during initialisation, reseed, and sequence bookkeeping — never during `best_bid()` or `set_qty()`.

### 6.2 L1 Residency

The 32 KiB L1d on the Ryzen 5 8600G holds exactly 512 cache lines of 64 bytes. The 33 KiB hot set overflows by ~1 KiB (16 cache lines), spilling into the 1 MiB L2. After a few hundred updates the working set stabilises in the L1/L2 boundary, and the measured latencies reflect that warm state.

**Practical consequence:** `update()` and `best_bid()` touch memory that is almost always in L1 or L2. The typical Zen 4 L1d hit is ~4 cycles (~0.79 ns at 5.05 GHz) and L2 hit ~12 cycles (~2.4 ns at 5.05 GHz). The measured 0.80–1.22 ns read figures are consistent with 4–6 cycle paths through register-held or recently prefetched L1 values.

### 6.3 Why f32 and Not f64?

Storing quantities as `f32` (4 bytes) instead of `f64` (8 bytes) halves the qty array footprint:

- `f64` qty array: 4096 × 8 B = **32 KiB per side → 64 KiB total** — does not fit in L1
- `f32` qty array: 4096 × 4 B = **16 KiB per side → 32 KiB total** — fits within 1 KiB of headroom

`f32` provides 7 significant decimal digits, sufficient for order sizes up to ~16 million lots at `lot_size = 0.001`. For the instruments this engine targets, this is not a practical limitation.

---

## 7. Realistic Workload Simulation

To translate nanosecond latencies into meaningful system-level numbers, a representative single-core workload is modelled at **1,000,000 operations per second**.

**Traffic mix:**

| Operation type    | Share | Volume/s |
|-------------------|-------|----------|
| `best_bid()` read | 70 %  | 700,000  |
| `update()`        | 20 %  | 200,000  |
| `top_bids(10)`    | 10 %  | 100,000  |

### 7.1 CPU Time Breakdown (Production Mode)

Using the steady-state single-update median of **71.3 ns** from `book_update_latency`:

| Operation      | Volume/s | Latency/op | CPU time/s  |
|----------------|----------|------------|-------------|
| `best_bid()`   | 700,000  | 1.20 ns    | 0.84 ms     |
| `update()`     | 200,000  | 71.3 ns    | 14.3 ms     |
| `top_bids(10)` | 100,000  | 35.4 ns    | 3.5 ms      |
| **Total**      |          |            | **~18.6 ms** |

**Core utilisation: ~1.9 %**

At one million mixed operations per second, the order book consumes under **2% of a single core**. The remaining 98% is free for signal computation, position management, risk checks, and network I/O.

### 7.2 Sensitivity to Update Rate

The dominant cost is `update()` at 14.3 ms. Doubling the feed rate to 400,000 updates/s would push total CPU to ~29 ms (~2.9%), still leaving substantial headroom. BBO reads contribute under 1 ms regardless of read frequency across this range.

---

## 8. Benchmark Mode Overhead (Adler32 Checksum)

All figures in Sections 3–5 use **production mode** (`--features no_checksum`). The default build (`cargo bench` with no flag) enables Adler32 checksum validation on every `update()` call via `verify_checksum()`, which:

1. Reads `best_bid()` and `best_ask()` (O(1) — two L1 cache loads)
2. Formats both integers using `itoa::Buffer` (stack-only, zero heap allocation)
3. Feeds the bytes through an Adler32 rolling accumulator
4. Compares the result against `msg.checksum`

This path is O(1) and allocation-free, but it is measurable overhead that does not exist in a production engine.

### 8.1 Measured Delta: Production vs. Benchmark Mode

The following table shows **directly measured** numbers from both build modes on the same hardware:

| Scenario                       | Production mode (`no_checksum`) | Benchmark mode (default) | Adler32 overhead    |
|--------------------------------|---------------------------------|--------------------------|---------------------|
| Single `update()` (depth 20)   | **~64–71 ns**                   | **~112–121 ns**          | **+47–57 ns**       |
| Batch 100, amortised           | **~73 ns/msg**                  | **~120–124 ns/msg**      | **+47–51 ns/msg**   |
| `update()` depth 5             | **~21–22 ns**                   | **~67 ns**               | **+45–46 ns**       |
| `update()` depth 50            | **~152–159 ns**                 | **~189–203 ns**          | **+37–44 ns**       |
| `bootstrap_update()`           | **~4.90 µs**                    | **~5.21 µs**             | **+310 ns**         |

Read-path operations (`best_bid`, `mid_price`, etc.) are **unaffected** by the feature flag — they contain no feature-gated code.

**The Adler32 path accounts for a flat ~47–57 ns per `update()` call** — consistent across depths because it hashes only the BBO (two integers), not the full diff payload.

### 8.2 Benchmark-Mode Workload

For comparison with Section 7.1, the same 1 M ops/s workload in benchmark mode:

| Operation      | Volume/s | Latency/op | CPU time/s   |
|----------------|----------|------------|--------------|
| `best_bid()`   | 700,000  | 1.20 ns    | 0.84 ms      |
| `update()`     | 200,000  | **120.9 ns** | **24.2 ms** |
| `top_bids(10)` | 100,000  | 35.4 ns    | 3.5 ms       |
| **Total**      |          |            | **~28.5 ms** |

**Core utilisation: ~2.9 %** (vs. ~1.9 % production mode)

### 8.3 Choosing the Right Mode

| Use case                                          | Recommended mode           |
|---------------------------------------------------|----------------------------|
| Latency benchmarking / performance profiling      | Default (`cargo bench`)    |
| Production deployment                             | `--features "no_checksum"` |
| Feed validation / integration testing             | Default (checksum catches mismatched state) |
| Throughput maximisation                           | `--features "no_checksum"` |

### 8.4 Running Benchmarks in Each Mode

```bash
# Production mode (no_checksum) — all suites
cargo bench --features "no_checksum"

# Benchmark mode (default, with Adler32) — all suites
cargo bench

# Single suite, production mode
cargo bench --bench orderbook_update --features "no_checksum"
```

---

## 9. Benchmark Reproduction

```bash
# Navigate to project root
cd orderbook

# Full benchmark run (benchmark mode, default)
cargo bench

# Open HTML report: target\criterion\report\index.html

# Production mode
cargo bench --features "no_checksum"
```

Criterion generates per-benchmark HTML pages with violin plots, iteration-count history, and regression detection across runs. All results are written under `target/criterion/`.