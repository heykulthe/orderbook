# L2Book: Implementation Deep-Dive

A technical reference for the ring-buffer Level-2 order book engine. This document covers
module layout, data structure design, the five core mechanisms that drive performance, and
the tradeoffs inherent in the chosen architecture.

---

## 1. Introduction

`L2Book` is a single-sided-symmetric, fixed-capacity Level-2 order book implemented in Rust.
Each side (bids and asks) maintains a circular buffer of `f32` quantities indexed by a
price offset relative to a movable anchor. An occupancy bitset shadows the buffer, and a
single cached field (`best_rel`) makes BBO reads O(1) without any scan on the common path.

The design philosophy is **hardware-first**: start with the CPU's L1 cache budget, choose
data types and layouts that fit within it, and only then think about algorithms. The result
is a structure where the fundamental operations — BBO read, level update — cost between
0.8 and 121 nanoseconds on a 5.05 GHz Zen 4 core.

---

## 2. Module Overview

```
src/
├── common/          — shared primitive types and message definitions
│   ├── types.rs     — Price (i64), Qty (f64), Side (enum)
│   ├── messages.rs  — L2UpdateMsg, L2Diff, MsgType
│   └── mod.rs
│
├── engine/          — synthetic market data generation
│   ├── simulator.rs — MarketFeedSimulator, FeedConfig
│   └── mod.rs
│
└── optimized/       — the order book implementation
    ├── book.rs      — L2Book, HotData, ColdData
    └── mod.rs
```

### 2.1 `common` — Shared Types

Contains the wire-format types that flow between the simulator, the book, and the WebSocket
server. `Price` is an `i64` price-in-ticks; `Qty` is `f64` at the boundary but is
immediately narrowed to `f32` on entry into the book. `Side` is a two-variant enum.

`L2UpdateMsg` carries a vector of `L2Diff` entries (each with a side, a price tick, and a
size), a sequence number, and a checksum field used in benchmark mode.

### 2.2 `engine` — Market Feed Simulator

`MarketFeedSimulator` generates synthetic L2 feed messages that exercise the full update
path. Mid-price evolves as geometric Brownian motion (`sigma_daily` parameter). Level sizes
are drawn from a log-normal distribution. The spread between best bid and ask is sampled
from a stochastic spread model that widens under simulated volatility.

`FeedConfig` controls symbol name, tick size, lot size, book depth, step interval, and
daily volatility. These parameters allow realistic throughput testing without connecting to
a live exchange.

### 2.3 `optimized` — The Order Book

The sole book implementation in this codebase. It is not a generic container — it is
purpose-built for the specific constraint that the entire hot working set fits in a 32 KiB
L1d cache. All design decisions below flow from that constraint.

---

## 3. Core Data Structures

### 3.1 `HotData`

```rust
#[repr(align(64))]
#[derive(Clone)]
struct HotData {
    qty:      Box<[f32; CAP]>,           // CAP = 4096; ring buffer of quantities
    occupied: Box<[u64; BITSET_SIZE]>,   // BITSET_SIZE = 64; one bit per qty slot
    head:     usize,                     // physical index of rel=0 (the anchor)
    anchor:   i64,                       // reference price in ticks
    best_rel: usize,                     // cached relative index of best price
                                         // usize::MAX when the side is empty
}
```

`#[repr(align(64))]` pins the struct to a 64-byte cache-line boundary. This matters because
`L2Book` contains two `HotData` instances (bids and asks) back-to-back in memory; without
alignment, a single cache line could straddle both sides, causing a write to the ask buffer
to evict a cache line carrying bid quantities.

The `qty` and `occupied` fields are `Box`-allocated so they live on the heap at a known
address — the struct itself is kept small (fitting in a few cache lines of metadata) while
the actual 16 KiB quantity array is contiguous in memory, not interleaved with other fields.

### 3.2 `ColdData`

```rust
#[derive(Clone, Serialize, Deserialize)]
struct ColdData {
    seq:         u64,
    tick_size:   f64,
    lot_size:    f64,
    initialized: bool,
}
```

Metadata that is read or written infrequently: once at construction, once per message for
sequence tracking, and once on reseed. By segregating these fields from `HotData`, the
compiler and CPU are free to keep `HotData` hot in L1 without being polluted by
serialisation-related or one-shot initialisation fields.

### 3.3 `L2Book`

```rust
pub struct L2Book {
    bids: HotData,
    asks: HotData,
    cold: ColdData,
}
```

The public-facing structure. `bids` and `asks` each own their ring buffer and bitset.
`cold` holds the shared metadata. The separation into three structs means that the 33 KiB
hot set (`bids` + `asks`) is contiguous in memory and separate from `cold`, minimising the
chance that a cache-line eviction caused by metadata access displaces a quantity array line.

### 3.4 Compile-Time Invariants

Three `const` assertions guard `CAP` properties at compile time using an array-length trick:

```rust
const fn bool_to_usize(b: bool) -> usize { b as usize }

const _ASSERT_CAP_POW2:   [(); 1] = [(); bool_to_usize(CAP.is_power_of_two())];
const _ASSERT_CAP_DIV64:  [(); 1] = [(); bool_to_usize(CAP % 64 == 0)];
const _ASSERT_BITSET_POW2:[(); 1] = [(); bool_to_usize((BITSET_SIZE & (BITSET_SIZE - 1)) == 0)];
```

If `CAP` is ever changed to a non-power-of-two value, or if the bitset arithmetic breaks,
compilation fails immediately — no runtime assertion required.

---

## 4. Five Core Mechanisms

### Mechanism 1: Ring Buffer Indexing

The quantity array is a circular buffer of `CAP = 4096` `f32` slots. Rather than storing
quantities indexed by absolute price ticks (which would require a 50,000+ element array for
most crypto instruments), each slot holds the quantity at a *relative* offset from a
movable anchor price.

**Coordinate system:**

```
For asks:
  rel = price_tick - anchor        (higher price → higher rel)

For bids:
  rel = anchor - price_tick        (lower price → higher rel, best bid is rel=0)
```

**Physical mapping:**

```rust
#[inline(always)]
fn rel_to_phys(&self, rel: usize) -> usize {
    (self.head + rel) & CAP_MASK   // CAP_MASK = 0xFFF
}
```

The bitwise AND replaces a modulo division: valid only because `CAP` is a power of two.
At 5.05 GHz, this is a single cycle. No branching, no division.

**Update and read:**

```rust
fn get_qty(&self, rel: usize) -> f32 {
    self.qty[self.rel_to_phys(rel)]
}

fn set_qty(&mut self, rel: usize, qty: f32) {
    let phys = self.rel_to_phys(rel);
    self.qty[phys] = qty;

    // Maintain the occupancy bitset in the same call
    let word = phys / 64;
    let bit  = phys % 64;
    if qty > EPS {
        self.occupied[word] |=  (1u64 << bit);
    } else {
        self.occupied[word] &= !(1u64 << bit);
    }
}
```

The bitset update is folded into every `set_qty` call at zero additional branching cost
beyond a compare and a bitwise OR/AND.

---

### Mechanism 2: BBO Caching

`best_rel` is the relative index of the current best price on a given side. It is
maintained incrementally — every call to `set_bid_level` or `set_ask_level` checks whether
the changed level is better than, equal to, or worse than the current best and updates
`best_rel` in O(1).

**BBO read (the common case):**

```rust
pub fn best_bid(&self) -> Option<(Price, Qty)> {
    if self.bids.best_rel == usize::MAX {
        return None;  // empty side
    }
    let price = self.bids.anchor - self.bids.best_rel as i64;
    let qty   = self.bids.get_qty(self.bids.best_rel);
    Some((price, qty as Qty))
}
```

This is two additions, one array index, and a return — no loop, no scan. The measured
latency of ~1.20 ns on a 5.05 GHz Zen 4 (≈ 6 CPU cycles) is consistent with two back-to-
back L1 cache loads and register moves.

**BBO removal (the fallback path):**

When the level at `best_rel` is set to zero, a fallback scan is needed to find the new
best. Rather than scanning the full `qty` array, `find_first_from_head` scans the 64-word
`occupied` bitset:

```rust
fn find_first_from_head(&self) -> usize {
    // Walk the 64 u64 words of the bitset
    // Use trailing_zeros to find the lowest set bit within each word
    // Return the first occupied slot's relative index, or usize::MAX
    for word_idx in 0..BITSET_SIZE {
        let phys_word_start = (self.head / 64 + word_idx) & (BITSET_SIZE - 1);
        let word = self.occupied[phys_word_start];
        if word != 0 {
            // ... compute rel from bit position and return
        }
    }
    usize::MAX  // no levels present
}
```

At most 64 iterations, each a `trailing_zeros` on a 64-bit integer — effectively O(1)
regardless of how many price levels are live. This scan is only paid when the current best
level is removed, not on every query.

---

### Mechanism 3: L1 Cache Budget

The total hot data footprint is calculated from first principles:

| Component             | Per Side          | Both Sides   |
|-----------------------|-------------------|--------------|
| `qty: [f32; 4096]`    | 4096 × 4 B = 16 KiB | 32 KiB     |
| `occupied: [u64; 64]` | 64 × 8 B = 512 B    | 1 KiB      |
| `head`, `anchor`, `best_rel` | ~24 B      | ~48 B      |
| **Total hot set**     | **~16.5 KiB**     | **~33 KiB** |

The AMD Ryzen 5 8600G (Zen 4) provides 32 KiB of L1d per core — the same constraint as the original design target. The 33 KiB hot set overflows by approximately 1 KiB (16 cache lines). On Zen 4, those overflow lines reside in the 1 MiB L2 (~12-cycle latency) and are brought back into L1 quickly once the working set stabilises after a few hundred updates.

**Why `f32` and not `f64`?**

`f64` would double the `qty` array to 32 KiB *per side* — 64 KiB total — far exceeding
the L1 budget and forcing every update to incur L2 or L3 misses. `f32` provides 7
significant decimal digits, sufficient for order sizes in the millions of lots at a
lot_size of 0.001. For the instruments this engine targets, the precision is not limiting.

**Alignment:**

`HotData` is `#[repr(align(64))]`. Both bid and ask instances therefore start on cache-line
boundaries, ensuring that a write to, say, `asks.occupied` cannot share a cache line with
`bids.qty` and cause false sharing on multi-threaded readers.

---

### Mechanism 4: Recentering (`recenter`)

When the market price drifts toward the edge of the 4096-tick window, the anchor must
move. `recenter()` is marked `#[cold]`, which hints `rustc`/LLVM to place it out of the
hot-path instruction cache and to deprioritise it during branch prediction.

**Trigger conditions:**

```rust
// Hard boundary: price is outside [0, CAP) — must recenter immediately
fn needs_recenter_hard(rel: usize) -> bool {
    rel >= CAP
}

// Soft boundary: price is within the window but near an edge
// Triggers a preemptive recenter to restore locality
fn needs_recenter_soft(rel: usize) -> bool {
    rel < RECENTER_LOW_MARGIN || rel >= RECENTER_HIGH_MARGIN
    // RECENTER_LOW_MARGIN = 64; RECENTER_HIGH_MARGIN = 4032
}
```

Soft recentering fires when the best price is within 64 ticks of either edge of the window,
giving the book room to absorb normal market movement without a hard boundary violation.

**The smart-shift algorithm:**

```rust
#[cold]
fn recenter(&mut self, new_anchor: i64) {
    let shift = (new_anchor - self.anchor) as isize;

    // Full reseed for catastrophic jumps (flash crash, large gap)
    if shift.unsigned_abs() >= CAP {
        self.qty.fill(0.0);
        self.occupied.fill(0);
        self.anchor  = new_anchor;
        self.head    = 0;
        self.best_rel = usize::MAX;
        return;
    }

    // Smart shift: only clear the band of slots leaving the window
    if shift > 0 {
        // Anchor moving up: slots at the low end leave the window
        // head advances by shift; clear [old_head .. old_head + shift)
        self.clear_band_phys(self.head, shift as usize);
        self.head = self.head.wrapping_add(shift as usize) & CAP_MASK;
    } else {
        // Anchor moving down: slots at the high end leave the window
        let abs_shift = (-shift) as usize;
        let clear_start = self.head.wrapping_sub(abs_shift) & CAP_MASK;
        self.clear_band_phys(clear_start, abs_shift);
        self.head = clear_start;
    }

    self.anchor = new_anchor;
    // best_rel is recomputed after the shift via find_first_from_head
}
```

`clear_band_phys` zeroes both the `qty` array slots and the corresponding bits in the
`occupied` bitset for the physical range `[start, start + count)` — O(|shift|) work, not
O(CAP). This is critical: zeroing the exiting band prevents stale quantities from being
silently re-labelled as quantities at new prices after the window slides — the "ghost
liquidity" bug that the test suite explicitly validates against.

---

### Mechanism 5: Dual-Mode Validation

The `update()` function contains a compile-time branch controlled by a Cargo feature flag:

```rust
pub fn update(&mut self, msg: &L2UpdateMsg, symbol: &str) {
    // 1. Apply all diffs — O(D) where D = msg.diffs.len()
    for diff in &msg.diffs {
        match diff.side {
            Side::Bid => self.set_bid_level(diff.price_tick, diff.size as f32),
            Side::Ask => self.set_ask_level(diff.price_tick, diff.size as f32),
        }
    }

    // 2. Validate — behaviour depends on build feature
    #[cfg(not(feature = "no_checksum"))]
    self.verify_checksum(msg, symbol);   // Benchmark mode: Adler32 hash of BBO

    #[cfg(feature = "no_checksum")]
    {
        // Production mode: O(1) sequence continuity check
        debug_assert!(
            msg.seq == self.cold.seq + 1,
            "sequence gap: expected {}, got {}", self.cold.seq + 1, msg.seq
        );
    }

    self.cold.seq = msg.seq;
}
```

**Benchmark mode (default):**

`verify_checksum()` reads the current `best_bid()` and `best_ask()`, formats both to
integer strings using `itoa::Buffer` (stack allocation only — no heap), feeds the bytes
through an Adler32 accumulator, and compares the result against `msg.checksum`. This
exercises the full BBO read path on every message, making the update latency figure
meaningful for throughput analysis. All benchmark numbers in this project use this mode.

**Production mode (`--features "no_checksum"`):**

Replaces the hash with a single integer comparison costing under 1 ns. This is the correct
HFT validation primitive: gaps in sequence numbers indicate a dropped message and trigger
recovery logic, whereas a full BBO hash on every tick would be wasted work in production.

The feature flag is a zero-cost abstraction — the unused branch generates no code and
introduces no runtime check.

---

## 5. Operation Complexity Table

| Operation                   | Complexity       | Notes                                              |
|-----------------------------|------------------|----------------------------------------------------|
| `best_bid()` / `best_ask()` | **O(1)**         | Single cache load of `best_rel`, anchor add        |
| `mid_price()`               | **O(1)**         | Two `best_rel` reads, one average                  |
| `spread_ticks()`            | **O(1)**         | Subtraction of two BBO prices                      |
| `orderbook_imbalance()`     | **O(1)**         | Reads BBO quantities, computes ratio               |
| `set_bid_level()` / `set_ask_level()` | **O(1)** amortized | `set_qty` + `best_rel` update; rare BBO fallback scan is O(64) = O(1) |
| `update()` (D diffs)        | **O(D)**         | D independent O(1) `set_level` calls              |
| `top_bids(n)` / `top_asks(n)` | **O(k)**      | k = ticks scanned from BBO outward to collect n levels |
| `bid_depth()` / `ask_depth()` | **O(1)**      | `popcount` over 64 fixed-size `u64` words         |
| `recenter()` (shift S)      | **O(S)**         | Clears S slots in qty + bitset; O(CAP) only on reseed |
| `find_first_from_head()`    | **O(1)**         | At most 64 `trailing_zeros` iterations            |

All operations are either O(1) or O(k/D/S) where the variable is a property of the current
message or scan request — never a function of the total number of live price levels in the
book.

---

## 6. Design Tradeoffs

### Fixed Window Size

The 4096-tick window is a hard constraint. A price move exceeding 4096 ticks in a single
message triggers a full reseed, clearing the book to a clean state and re-seeding from the
message contents. On most liquid crypto and equity instruments with reasonable tick sizes
this never occurs under normal conditions. Under flash-crash or circuit-breaker scenarios
it may fire once; the book recovers immediately on the next message.

The window size can be changed by modifying `CAP`, provided it remains a power of two and
a multiple of 64 (enforced by the compile-time assertions). Doubling `CAP` to 8192 would
double the hot set to ~66 KiB, evicting it from L1 on most server-class CPUs. Halving to
2048 narrows the window and increases recenter frequency for volatile instruments.

### `f32` Quantity Precision

Seven significant decimal digits covers quantities up to ~16,777,216 with single-unit
resolution, or ~167 with 0.001 resolution — adequate for most HFT book-keeping. If the
downstream risk system requires `f64` precision (e.g., for large notional positions in
bond markets), the `qty` type alias can be changed at the cost of doubling the hot set
and losing L1 residency.

### Recenter Latency Spikes

A `recenter()` call is O(|shift|) and is marked `#[cold]`, but it still imposes a latency
spike relative to the ~121 ns steady-state update cost. On a 5.05 GHz CPU, clearing 64
slots (the hysteresis margin) costs roughly 12–18 ns. In telemetry, these spikes appear
as occasional outliers in Criterion's latency distribution. The `recenter_count` field
(present under `debug_assertions`) lets integration tests verify how often recentering
fires for a given instrument's volatility profile.

### Code Complexity

The anchor arithmetic, wrapping index calculations, and bitset management require careful
maintenance. Changing any invariant (CAP, BITSET_SIZE, EPS, margin constants) without
understanding the downstream effects can silently introduce ghost liquidity or incorrect
BBO values. The 14-test suite exists specifically to catch these classes of bugs; all tests
should be run after any change to `book.rs`.

---

## 7. Hardware Context

All performance numbers in this project were measured on the following system:

| Component   | Specification                                      |
|-------------|----------------------------------------------------|
| CPU         | AMD Ryzen 5 8600G (Zen 4), 6 cores / 12 threads  |
| Clock speed | 5.05 GHz (boost)                                  |
| L1d cache   | 32 KiB per core                                   |
| L2 cache    | 1 MiB per core                                    |
| L3 cache    | 16 MiB shared                                     |
| RAM         | ~15 GiB (system)                                  |
| GPU         | AMD Radeon RX 9060 XT — unused by benchmarks      |
| OS          | Windows 11 Pro (64-bit)                           |

The Ryzen 5 8600G's 32 KiB L1d is the same tight constraint the design was built around. Zen 4 keeps L1d at 32 KiB (matching older Intel designs) but dramatically increases L2 to 1 MiB per core, which means the 1 KiB overflow from the hot set is absorbed by an extremely fast L2 with negligible penalty. The higher clock speed (5.05 GHz vs. typical 3–4 GHz baselines) is the primary reason the measured update latencies (~111–121 ns) come in well under 200 ns even with checksum validation enabled.

The choice to tune for 32 KiB ensures that the performance claims are honest on mid-tier
hardware and improve automatically on newer platforms — the design does not require a
particular CPU generation to deliver sub-nanosecond BBO reads.