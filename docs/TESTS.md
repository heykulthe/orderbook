# Test Validation Report: L2Book Unit Tests

All 14 unit tests pass. This document describes what each test covers, why it exists,
and how to run the suite.

---

## 1. Summary

| Metric           | Value                        |
|------------------|------------------------------|
| Total tests      | 14                           |
| Passed           | 14                           |
| Failed           | 0                            |
| Ignored          | 0                            |
| Test location    | `src/optimized/book.rs` (`mod tests`) |
| Run time         | < 0.01 s                     |

The test suite is focused entirely on `L2Book` — the ring-buffer order book. Tests are
concentrated on the logic that is both performance-critical and subtly dangerous: anchor
arithmetic, bitset maintenance, and the `recenter` algorithm. A correctness bug in any of
those areas would either produce silent data corruption (wrong prices, phantom liquidity)
or a hard panic (out-of-bounds index). Both outcomes are unacceptable in a trading system.

---

## 2. Running the Tests

```bash
# From the project root
cd orderbook

# Run all tests (debug profile, no features needed)
cargo test

# Run with output captured (useful for CI)
cargo test -- --test-output immediate
```

---

## 3. Test Execution Output

```
running 14 tests
test optimized::book::tests::test_band_clearing_after_recenter ... ok
test optimized::book::tests::test_l1_optimized_basic ... ok
test optimized::book::tests::test_eps_threshold ... ok
test optimized::book::tests::test_no_infinite_recursion ... ok
test optimized::book::tests::test_large_price_jump_reseed ... ok
test optimized::book::tests::test_nan_inf_sanitization ... ok
test optimized::book::tests::test_negative_shift_recenter_no_ghost_liquidity ... ok
test optimized::book::tests::test_depth_collection_exact ... ok
test optimized::book::tests::test_no_ghost_liquidity_after_soft_recenter ... ok
test optimized::book::tests::test_recenter_threshold ... ok
test optimized::book::tests::test_small_shift_recenter_no_ghost_liquidity ... ok
test optimized::book::tests::test_massive_wraparound ... ok
test optimized::book::tests::test_wraparound_shift_no_corruption ... ok
test optimized::book::tests::test_negative_shift_recenter ... ok

test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

---

## 4. Test Categories

Tests are grouped here by the concern they address. The grouping is conceptual — all 14
tests live in the same `mod tests` block inside `book.rs`.

---

### 4.1 Foundational Correctness

#### `test_l1_optimized_basic`

Exercises the core update-and-read cycle: insert several bid and ask levels, then verify
that `best_bid()`, `best_ask()`, `mid_price()`, and `spread_ticks()` all return the
expected values. This is the sanity check that runs first; if it fails, every other test
is suspect.

---

### 4.2 Data Robustness — Input Sanitisation

The book receives data from an external feed. Market data feeds occasionally emit
malformed or degenerate values. These two tests verify that bad input is neutralised
before it can corrupt internal state.

#### `test_nan_inf_sanitization`

Sends `L2Diff` entries carrying `f32::NAN`, `f32::INFINITY`, `f32::NEG_INFINITY`, and
negative quantities. After each malformed update, the test confirms:

- The quantity stored for that price level is 0.0 (sanitised to zero).
- The corresponding bit in the `occupied` bitset is cleared.
- `best_bid()` and `best_ask()` are unaffected and still return valid levels from the
  surrounding context.

A `NaN` quantity that reaches the bitset comparison (`qty > EPS`) produces undefined
behaviour in IEEE 754 (the comparison evaluates false), which would mark the level as
unoccupied. The sanitisation step makes this explicit and deterministic before the value
ever touches the bitset.

#### `test_eps_threshold`

Sends quantities below the epsilon floor (`EPS = 1e-9`). Verifies that such levels are
treated as zero — the bitset bit is cleared and `best_rel` is not updated to point at them.
This prevents denormalised floating-point values from polluting the `occupied` bitset with
phantom levels that `find_first_from_head` would otherwise trip over.

---

### 4.3 Ghost Liquidity and Data Integrity

"Ghost liquidity" is the class of bug where a quantity survives a ring-buffer shift and
is re-interpreted as belonging to a different price after the anchor moves. This is the
most dangerous correctness failure in a ring-buffer book design, and it is invisible to
any test that only checks BBO correctness without also inspecting individual price levels
after recentering.

These four tests approach the problem from different angles — positive shift, negative
shift, wraparound, and soft-recenter conditions — to build confidence that no code path
leaves stale data in the buffer.

#### `test_no_ghost_liquidity_after_soft_recenter`

Places a known quantity at a specific price, triggers a soft recenter (anchor moves by
less than CAP), then verifies that the quantity is either present at its original price
or absent entirely. It must never appear at a different price. This is the primary
correctness invariant of `clear_band_phys`.

#### `test_small_shift_recenter_no_ghost_liquidity`

Performs a sequence of small upward anchor shifts and verifies after each step that
the price-to-quantity mapping is consistent. A level inserted before a shift must not
be visible at a shifted price after the shift completes.

#### `test_negative_shift_recenter_no_ghost_liquidity`

Same intent as the previous test, but with the anchor moving downward. Negative shifts
follow a different code path (the band being cleared is on the high-index side of the
buffer rather than the low-index side), so this test is necessary to exercise that branch
independently.

#### `test_wraparound_shift_no_corruption`

Forces a shift that causes `(head + shift) % CAP` to wrap around the physical end of the
array (i.e., head starts near `CAP` and the new head wraps to near 0). Verifies that
all levels, including those that cross the wrap boundary, are consistent: occupied slots
report the correct quantity, and slots that left the window report zero. Removes levels
one by one after the shift and confirms each removal leaves `best_bid()` consistent.

---

### 4.4 Recenter Logic

These tests cover the mechanical correctness of the sliding-window algorithm across all
the triggering conditions and edge cases.

#### `test_recenter_threshold`

Verifies the hysteresis margins that control when `recenter()` fires:

- An update with `rel` inside `[RECENTER_LOW_MARGIN, RECENTER_HIGH_MARGIN)` must NOT
  trigger a recenter.
- An update with `rel` at or beyond `RECENTER_HIGH_MARGIN` MUST trigger a recenter.
- After recentering, the BBO is unchanged and all previously inserted levels remain
  accessible.

This test protects against off-by-one errors in the margin comparisons in
`needs_recenter_soft`.

#### `test_large_price_jump_reseed`

Sends an update for a price that is more than `CAP` ticks away from the current anchor.
Verifies that `recenter()` detects this as a catastrophic jump and performs a full reseed:
the entire `qty` array and `occupied` bitset are zeroed, the anchor is moved to the new
price, and `best_rel` resets to `usize::MAX`. The book should then correctly accept the
incoming level as the first entry after the reseed.

This scenario models a flash crash, a large gap-open, or the very first update after a
cold start on a distant price.

#### `test_negative_shift_recenter`

Focuses on the `head` pointer arithmetic for a downward anchor shift. The physical index
of `rel=0` moves forward in the array when the anchor moves up, and backward when the
anchor moves down. The backward (negative) case uses `wrapping_sub`, which silently
overflows if the shift exceeds `head`. This test catches that class of arithmetic error
by inserting levels, triggering a downward shift, and verifying that `best_bid()` resolves
to the correct absolute price tick.

#### `test_band_clearing_after_recenter`

After a recenter that shifts the window upward by S ticks, the S physical slots that
previously corresponded to the lowest S prices are now outside the window. If the book
had live levels in those slots before the shift, those slots must be zeroed — both in
`qty` and in `occupied`. This test explicitly inserts quantities at prices near the low
edge of the window, triggers a shift, and then verifies that those slots read as 0.0 and
their bitset bits are clear. Any stale data left here would re-appear as ghost levels the
next time the window slides back in that direction.

#### `test_no_infinite_recursion`

Guards against a recursive call cycle: `set_bid_level()` may call `recenter()` if the
incoming price is near an edge; `recenter()` calls `find_first_from_head()` to recompute
`best_rel`; `find_first_from_head()` must not call `set_bid_level()` or `recenter()`
again. This test constructs the exact state that would trigger the cycle (a price right
at the recenter threshold) and verifies that the call returns normally rather than
overflowing the stack. A missing guard condition here would produce an undetectable stack
overflow in release builds.

---

### 4.5 Stress Tests and Read Validation

#### `test_massive_wraparound`

Runs a high-volume burst of updates at prices that steadily march across the tick space,
triggering many recenter operations in sequence. After the burst, verifies that:

- `best_bid()` and `best_ask()` return values consistent with the last applied update.
- The `occupied` bitset has no bits set for levels that were cleared during the recenters.
- The total `bid_depth()` and `ask_depth()` match the number of live levels actually
  inserted after the last reseed.

This test is the closest thing to a soak test that unit tests can provide — it checks
long-term consistency under repeated window-sliding.

#### `test_depth_collection_exact`

Inserts exactly N non-contiguous bid levels (with deliberate gaps between them) and calls
`top_bids(N)`. Verifies that exactly N levels are returned, that they are in descending
price order, and that no empty slots between them are included in the result. Then calls
`orderbook_imbalance_depth(N)` and verifies the returned ratio against a hand-computed
expected value.

This test prevents off-by-one errors in the `top_bids` scan loop — specifically, the
condition that terminates the scan when the requested count is reached vs. when the window
is exhausted.

---

## 5. Coverage Summary

| Category                       | Tests                                                                 | What could go wrong without them                     |
|--------------------------------|-----------------------------------------------------------------------|------------------------------------------------------|
| Foundational correctness       | `test_l1_optimized_basic`                                             | Basic update/read cycle silently broken              |
| Input sanitisation             | `test_nan_inf_sanitization`, `test_eps_threshold`                     | NaN/Inf propagates into bitset; denormals cause phantom levels |
| Ghost liquidity (positive)     | `test_no_ghost_liquidity_after_soft_recenter`, `test_small_shift_recenter_no_ghost_liquidity` | Stale qty re-labelled to wrong price after upward shift |
| Ghost liquidity (negative)     | `test_negative_shift_recenter_no_ghost_liquidity`                     | Same bug, different code path (downward shift)       |
| Ghost liquidity (wraparound)   | `test_wraparound_shift_no_corruption`                                 | Head wrap corrupts data at buffer boundary           |
| Band clearing                  | `test_band_clearing_after_recenter`                                   | Slots exiting the window not zeroed; reappear later  |
| Recenter trigger conditions    | `test_recenter_threshold`                                             | Hysteresis margins off by one; recenter too early or too late |
| Hard reseed                    | `test_large_price_jump_reseed`                                        | Catastrophic jump not handled; book in invalid state |
| Negative shift arithmetic      | `test_negative_shift_recenter`                                        | `wrapping_sub` overflow on downward anchor movement  |
| Infinite recursion guard       | `test_no_infinite_recursion`                                          | Stack overflow on recenter-triggering price          |
| Long-run stability             | `test_massive_wraparound`                                             | Accumulated corruption after many recenters          |
| Depth scan correctness         | `test_depth_collection_exact`                                         | `top_bids` skips gaps incorrectly; imbalance wrong   |

---

## 6. Test Environment

| Component | Details |
|-----------|---------|
| CPU       | AMD Ryzen 5 8600G (Zen 4) @ 5.05 GHz |
| RAM       | ~15 GiB (system) |
| GPU       | AMD Radeon RX 9060 XT (unused by tests) |
| OS        | Windows 11 Pro (64-bit) |
| Compiler  | Rust 1.70+ (stable) |