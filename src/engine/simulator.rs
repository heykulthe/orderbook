//! Synthetic L2 market-feed simulator.
//!
//! Produces a realistic stream of [`L2UpdateMsg`] incremental diffs by driving
//! the mid-price via discrete Geometric Brownian Motion, sampling stochastic
//! spreads, and distributing resting quantities log-normally with depth decay.
//!
//! ## Architecture
//!
//! Two parallel state representations are maintained:
//!
//! * **`lob`** — the production-grade ring-buffer [`L2Book`] from the optimized
//!   module. Used exclusively for O(1) best-bid / best-ask reads when sealing
//!   each outgoing message's Adler-32 checksum.
//!
//! * **`cur_bids` / `cur_asks`** — plain [`HashMap`] mirrors of the current
//!   visible book. These exist purely so `make_and_apply_diffs` can snapshot
//!   the previous state, compute the diff set, and apply mutations — all without
//!   touching the ring-buffer internals or requiring public fields on `lob`.
//!
//! The two representations are always kept in sync: every mutation applied to
//! the shadow maps is also forwarded to `lob` via [`L2Book::update`].

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use adler::Adler32;
use rand_distr::{Distribution, LogNormal, Normal};

use crate::common::messages::{L2Diff, L2UpdateMsg, MsgType};
use crate::common::types::{Price, Qty, Side};
use crate::optimized::book::L2Book;

// ---------------------------------------------------------------------------
// FeedConfig
// ---------------------------------------------------------------------------

/// Runtime parameters for the market-feed simulator.
///
/// All fields are `pub` so callers can construct a custom config directly
/// without needing a builder API.
pub struct FeedConfig {
    /// Instrument identifier forwarded verbatim into every outgoing message.
    pub symbol: String,

    /// Minimum price increment.
    /// All prices are integer multiples of this value (i.e. stored as ticks).
    pub tick_size: f64,

    /// Minimum order quantity.
    /// Sampled sizes are floored to this precision.
    pub lot_size: f64,

    /// Number of visible price levels per side.
    pub depth: usize,

    /// Wall-clock cadence between successive updates, in milliseconds.
    pub dt_ms: u64,

    /// Annualised daily price volatility expressed as a fraction.
    /// For example `0.60` means 60 % per day.
    pub sigma_daily: f64,
}

impl Default for FeedConfig {
    fn default() -> Self {
        Self {
            symbol: "BTC-USDT".to_string(),
            tick_size: 0.1,
            lot_size: 0.001,
            depth: 20,
            dt_ms: 100, // 10 Hz feed cadence
            sigma_daily: 0.60,
        }
    }
}

// ---------------------------------------------------------------------------
// MarketFeedSimulator
// ---------------------------------------------------------------------------

/// Synthetic L2 orderbook market-feed simulator.
///
/// Call [`MarketFeedSimulator::bootstrap_update`] once to obtain an initial
/// full-book snapshot, then call [`MarketFeedSimulator::next_update`] in a
/// loop to obtain incremental diff messages at the configured cadence.
pub struct MarketFeedSimulator {
    /// Feed configuration: symbol, tick/lot sizes, depth, cadence, volatility.
    params: FeedConfig,

    /// High-performance ring-buffer orderbook.
    /// Updated on every tick so that `best_bid()` / `best_ask()` are O(1)
    /// for checksum computation.
    lob: L2Book,

    /// Shadow bid-side state — a price-tick → quantity map maintained in
    /// lockstep with `lob`. Used solely by `make_and_apply_diffs` to snapshot
    /// the previous book and compute the diff vector.
    cur_bids: HashMap<Price, Qty>,

    /// Shadow ask-side state — symmetric counterpart to `cur_bids`.
    cur_asks: HashMap<Price, Qty>,

    /// Current mid-price expressed in price ticks (floating-point so that
    /// the Brownian increment accumulates without rounding each step).
    mid_px_ticks: f64,

    /// Stochastic spread sampler.
    /// Draws from Normal(μ = 2.0, σ = 0.8); samples are rounded and
    /// clamped to [1, 5] ticks before use.
    half_spread_dist: Normal<f64>,

    /// Brownian price-increment sampler.
    /// Draws from Normal(0, σ_dt) where σ_dt is derived from `sigma_daily`
    /// and the configured update cadence.
    price_drift_dist: Normal<f64>,

    /// Log-normal resting-quantity sampler.
    /// Draws from LogNormal(μ = -1.2, σ = 0.6); the sample is the *base*
    /// size before depth-decay attenuation is applied.
    qty_dist: LogNormal<f64>,

    /// Per-level exponential size-decay factor.
    /// The target quantity at depth k is `base_qty * depth_decay^k`,
    /// floored at `params.lot_size`.
    depth_decay: f64,

    /// Monotonically-increasing sequence counter stamped on every outgoing
    /// [`L2UpdateMsg`] and embedded in the Adler-32 checksum payload.
    update_seq: u64,
}

// ---------------------------------------------------------------------------
// Core implementation
// ---------------------------------------------------------------------------

impl MarketFeedSimulator {
    // ------------------------------------------------------------------
    // Constructors
    // ------------------------------------------------------------------

    /// Creates a simulator with the default [`FeedConfig`].
    pub fn new() -> Self {
        Self::with_config(FeedConfig::default())
    }

    /// Creates a simulator from a custom [`FeedConfig`].
    ///
    /// # Panics
    ///
    /// Panics if `tick_size <= 0`, `lot_size <= 0`, or `dt_ms == 0`.
    pub fn with_config(cfg: FeedConfig) -> Self {
        let mut cfg = cfg;

        assert!(cfg.tick_size > 0.0, "tick_size must be strictly positive");
        assert!(cfg.lot_size > 0.0, "lot_size must be strictly positive");
        assert!(cfg.dt_ms > 0, "dt_ms must be strictly positive");

        // Depth of at least 1 level makes the book meaningful.
        cfg.depth = cfg.depth.max(1);

        // Rescale daily volatility to per-step volatility via sqrt(dt):
        //
        //   steps_per_day = (1_000 ms / dt_ms) * 60 * 60 * 24
        //   sigma_dt      = sigma_daily / sqrt(steps_per_day)
        //
        // We work directly in ticks so the Brownian increment drives
        // `mid_px_ticks` without any price-unit conversion.
        let steps_per_day = (1_000.0 / cfg.dt_ms as f64) * 60.0 * 60.0 * 24.0;
        let sigma_per_step = cfg.sigma_daily / steps_per_day.sqrt();

        let mut feed = MarketFeedSimulator {
            lob: L2Book::new(cfg.tick_size, cfg.lot_size),
            cur_bids: HashMap::new(),
            cur_asks: HashMap::new(),
            // Start mid at 65,000 USD expressed in 0.1-tick units → 650,000 ticks
            mid_px_ticks: 650_000.0,
            half_spread_dist: Normal::new(2.0, 0.8).unwrap(),
            price_drift_dist: Normal::new(0.0, sigma_per_step).unwrap(),
            qty_dist: LogNormal::new(-1.2, 0.6).unwrap(),
            depth_decay: 0.92,
            update_seq: 0,
            params: cfg,
        };

        // Seed the ring-buffer and shadow maps with an initial full snapshot.
        feed.rebuild_full_book_from_state();
        feed
    }

    // ------------------------------------------------------------------
    // Public API
    // ------------------------------------------------------------------

    /// Returns a full-book snapshot as a bootstrap [`L2UpdateMsg`].
    ///
    /// Wipes all state and rebuilds the book around the current mid-price.
    /// Every bid and ask level is included as an upsert diff so that a
    /// downstream consumer can initialise a clean local copy from this
    /// single message.
    pub fn bootstrap_update(&mut self) -> L2UpdateMsg {
        // Clear and rebuild — populates cur_bids/cur_asks and resets lob.
        self.rebuild_full_book_from_state();

        // Snapshot the full book into a diff vector for the client.
        let mut snapshot_diffs: Vec<L2Diff> =
            Vec::with_capacity(self.cur_bids.len() + self.cur_asks.len());

        for (&px, &sz) in &self.cur_bids {
            snapshot_diffs.push(L2Diff {
                side: Side::Bid,
                price_tick: px,
                size: sz,
            });
        }
        for (&px, &sz) in &self.cur_asks {
            snapshot_diffs.push(L2Diff {
                side: Side::Ask,
                price_tick: px,
                size: sz,
            });
        }

        // Second sequence increment — matches the original simulator's dual-step
        // behaviour: one increment inside rebuild, one more here so the client
        // bootstrap message has a strictly higher seq than the internal seed.
        self.update_seq = self.update_seq.wrapping_add(1);

        L2UpdateMsg {
            msg_type: MsgType::L2Update,
            symbol: self.params.symbol.clone(),
            ts: Self::now_ns(),
            seq: self.update_seq,
            diffs: snapshot_diffs,
            checksum: self.checksum(),
        }
    }

    /// Advances the simulation by one tick and returns an incremental
    /// [`L2UpdateMsg`].
    ///
    /// Steps the Brownian price process, computes the diff between the new
    /// target book and the current shadow state, applies the diffs to both
    /// the shadow maps and the optimised ring-buffer book, then seals and
    /// returns the message.
    pub fn next_update(&mut self) -> L2UpdateMsg {
        // 1. Advance the mid-price.
        self.step_brownian();

        // 2. Compute and commit the incremental diff (shadow maps updated here).
        let diffs = self.make_and_apply_diffs();

        // 3. Seal the sequence number.
        self.update_seq = self.update_seq.wrapping_add(1);
        let ts = Self::now_ns();

        // 4. Forward the diffs into the ring-buffer book so that
        //    lob.best_bid() / lob.best_ask() reflect the post-tick state
        //    before checksum() reads them.  The placeholder checksum (0) will
        //    cause the internal verify step to return false, but diffs are
        //    always applied by update() regardless of that result — so the
        //    ring buffer remains consistent.
        let prelim = L2UpdateMsg {
            msg_type: MsgType::L2Update,
            symbol: self.params.symbol.clone(),
            ts,
            seq: self.update_seq,
            diffs,
            checksum: 0, // filled in below after lob is updated
        };
        self.lob.update(&prelim, &prelim.symbol);

        // 5. Compute the real checksum from the freshly-updated BBO and
        //    replace the placeholder, then return.
        let real_checksum = self.checksum();
        L2UpdateMsg {
            checksum: real_checksum,
            ..prelim
        }
    }

    /// Returns the configured update cadence in milliseconds.
    #[inline]
    pub fn dt_ms(&self) -> u64 {
        self.params.dt_ms
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Returns the current wall-clock time as nanoseconds since the Unix epoch.
    #[inline]
    fn now_ns() -> i64 {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        (t.as_secs() as i64) * 1_000_000_000_i64 + (t.subsec_nanos() as i64)
    }

    /// Computes an Adler-32 checksum over the string
    /// `"symbol|seq|best_bid_tick|best_ask_tick"`.
    ///
    /// Reads BBO from the optimised ring-buffer book in O(1).
    fn checksum(&self) -> u32 {
        let (bb, _) = self.lob.best_bid().unwrap_or((0, 0.0));
        let (ba, _) = self.lob.best_ask().unwrap_or((0, 0.0));
        let payload = format!("{}|{}|{}|{}", self.params.symbol, self.update_seq, bb, ba);
        let mut h = Adler32::new();
        h.write_slice(payload.as_bytes());
        h.checksum()
    }

    /// Advances the mid-price by one discrete Brownian step.
    ///
    /// The raw Normal draw is scaled by 10 to keep the simulated book visibly
    /// active at a 10 Hz feed rate.
    fn step_brownian(&mut self) {
        let increment = self.price_drift_dist.sample(&mut rand::thread_rng());
        self.mid_px_ticks += increment * 10.0;
    }

    /// Samples the bid-ask spread in ticks from a clamped Normal distribution.
    ///
    /// Draws from Normal(μ = 2.0, σ = 0.8), rounds to the nearest integer,
    /// and clamps to [1, 5] ticks so the spread is always at least one tick
    /// wide.
    fn sample_spread_ticks(&mut self) -> i64 {
        let raw = self
            .half_spread_dist
            .sample(&mut rand::thread_rng())
            .round();
        raw.clamp(1.0, 5.0) as i64
    }

    /// Returns the target resting quantity for price level `k` (0-indexed from
    /// the best price).
    ///
    /// A log-normal base quantity is attenuated by `depth_decay^k` so that
    /// liquidity thins out naturally away from the mid-price. The result is
    /// floored at `params.lot_size`.
    fn target_level_size(&mut self, level: usize) -> f64 {
        let base = self.qty_dist.sample(&mut rand::thread_rng());
        let attenuation = self.depth_decay.powi(level as i32);
        (base * attenuation).max(self.params.lot_size)
    }

    /// Computes the best-bid and best-ask tick anchors from a mid-price tick
    /// and a sampled spread.
    ///
    /// The spread is split as symmetrically as possible across both sides.
    /// The ask is always strictly greater than the bid.
    #[inline]
    fn level_seeds(mid_tick_i: i64, spread: i64) -> (i64, i64) {
        let enforced_spread = spread.max(1);
        let half_down = enforced_spread / 2;
        let mut half_up = enforced_spread - half_down;

        // Ensure the ask side always gets at least one tick of gap.
        if half_up == 0 {
            half_up = 1;
        }

        let best_bid_tick = mid_tick_i - half_down;
        let mut best_ask_tick = mid_tick_i + half_up;

        // Hard safety: ask must be strictly above bid regardless of rounding.
        if best_ask_tick <= best_bid_tick {
            best_ask_tick = best_bid_tick + 1;
        }

        (best_bid_tick, best_ask_tick)
    }

    /// Clears both the shadow maps and the ring-buffer book, then populates a
    /// fresh full-depth snapshot centred on the current mid-price.
    ///
    /// Increments `update_seq` by one, resets `lob` to a clean default so its
    /// anchors are re-initialised by the subsequent `update()` call, and applies
    /// the full snapshot via a preliminary [`L2UpdateMsg`] (checksum = 0
    /// placeholder — the return value of `update()` is intentionally ignored).
    fn rebuild_full_book_from_state(&mut self) {
        let spread = self.sample_spread_ticks();
        let mid_tick_i = self.mid_px_ticks.round() as i64;
        let (bid_anchor, ask_anchor) = Self::level_seeds(mid_tick_i, spread);
        let depth = self.params.depth;

        // Wipe shadow state before rebuilding.
        self.cur_bids.clear();
        self.cur_asks.clear();

        let mut diffs: Vec<L2Diff> = Vec::with_capacity(depth * 2);

        // Bids descend from the best-bid anchor.
        for k in 0..depth {
            let px = bid_anchor - k as i64;
            let sz = self.target_level_size(k);
            self.cur_bids.insert(px, sz);
            diffs.push(L2Diff {
                side: Side::Bid,
                price_tick: px,
                size: sz,
            });
        }

        // Asks ascend from the best-ask anchor.
        for k in 0..depth {
            let px = ask_anchor + k as i64;
            let sz = self.target_level_size(k);
            self.cur_asks.insert(px, sz);
            diffs.push(L2Diff {
                side: Side::Ask,
                price_tick: px,
                size: sz,
            });
        }

        self.update_seq = self.update_seq.wrapping_add(1);

        // Reset the ring-buffer book so that `initialize_anchors` fires again
        // on the upcoming update call, re-deriving anchor positions from the
        // new price level cluster.
        let tick_size = self.params.tick_size;
        let lot_size = self.params.lot_size;
        self.lob = L2Book::new(tick_size, lot_size);

        // Apply the full snapshot.  The placeholder checksum of 0 causes
        // lob.update() to return false in benchmark mode, but diffs are always
        // committed to the ring buffer before that check, so the book state is
        // always correct regardless of the return value.
        let seed_msg = L2UpdateMsg {
            msg_type: MsgType::L2Update,
            symbol: self.params.symbol.clone(),
            ts: Self::now_ns(),
            seq: self.update_seq,
            diffs,
            checksum: 0,
        };
        self.lob.update(&seed_msg, &seed_msg.symbol);
    }

    /// Builds the next target book, computes the minimal diff set against the
    /// current shadow state, and applies those diffs to both the shadow maps
    /// and the ring-buffer book.
    ///
    /// Returns the diff vector so the caller can embed it in an outgoing
    /// [`L2UpdateMsg`].
    fn make_and_apply_diffs(&mut self) -> Vec<L2Diff> {
        // Snapshot the current shadow state before computing the new target.
        // These clones are O(depth) and live only for the duration of this call.
        let prev_bids = self.cur_bids.clone();
        let prev_asks = self.cur_asks.clone();

        // -------------------------------------------------------------------
        // Build the next target book around the updated mid-price.
        // -------------------------------------------------------------------
        let spread = self.sample_spread_ticks();
        let mid_tick_i = self.mid_px_ticks.round() as i64;
        let (bid_anchor, ask_anchor) = Self::level_seeds(mid_tick_i, spread);
        let depth = self.params.depth;

        let mut next_bids: HashMap<Price, Qty> = HashMap::with_capacity(depth);
        let mut next_asks: HashMap<Price, Qty> = HashMap::with_capacity(depth);

        for k in 0..depth {
            next_bids.insert(bid_anchor - k as i64, self.target_level_size(k));
            next_asks.insert(ask_anchor + k as i64, self.target_level_size(k));
        }

        // -------------------------------------------------------------------
        // Compute the minimal diff: deletions, quantity changes, and new levels.
        // -------------------------------------------------------------------
        let eps = 1e-9_f64;
        let mut diffs: Vec<L2Diff> = Vec::new();

        // ---- Bid side: removals and quantity updates ----
        for (&px, &qty) in &prev_bids {
            match next_bids.get(&px) {
                Some(&new_qty) if (new_qty - qty).abs() > eps => {
                    // Quantity changed — emit an upsert.
                    diffs.push(L2Diff {
                        side: Side::Bid,
                        price_tick: px,
                        size: new_qty,
                    });
                }
                None => {
                    // Level fell outside the visible window — remove it (size = 0).
                    diffs.push(L2Diff {
                        side: Side::Bid,
                        price_tick: px,
                        size: 0.0,
                    });
                }
                _ => {} // Unchanged — no diff needed.
            }
        }

        // ---- Ask side: removals and quantity updates ----
        for (&px, &qty) in &prev_asks {
            match next_asks.get(&px) {
                Some(&new_qty) if (new_qty - qty).abs() > eps => {
                    diffs.push(L2Diff {
                        side: Side::Ask,
                        price_tick: px,
                        size: new_qty,
                    });
                }
                None => {
                    diffs.push(L2Diff {
                        side: Side::Ask,
                        price_tick: px,
                        size: 0.0,
                    });
                }
                _ => {}
            }
        }

        // ---- Bid side: brand-new levels ----
        for (&px, &new_qty) in &next_bids {
            if !prev_bids.contains_key(&px) {
                diffs.push(L2Diff {
                    side: Side::Bid,
                    price_tick: px,
                    size: new_qty,
                });
            }
        }

        // ---- Ask side: brand-new levels ----
        for (&px, &new_qty) in &next_asks {
            if !prev_asks.contains_key(&px) {
                diffs.push(L2Diff {
                    side: Side::Ask,
                    price_tick: px,
                    size: new_qty,
                });
            }
        }

        // -------------------------------------------------------------------
        // Commit the diffs to the shadow HashMaps (maintains feed consistency).
        // The ring-buffer book is updated by the caller (next_update) once the
        // seq number and timestamp are finalised.
        // -------------------------------------------------------------------
        for d in &diffs {
            match d.side {
                Side::Bid => {
                    if d.size == 0.0 {
                        self.cur_bids.remove(&d.price_tick);
                    } else {
                        self.cur_bids.insert(d.price_tick, d.size);
                    }
                }
                Side::Ask => {
                    if d.size == 0.0 {
                        self.cur_asks.remove(&d.price_tick);
                    } else {
                        self.cur_asks.insert(d.price_tick, d.size);
                    }
                }
            }
        }

        diffs
    }
}

impl Default for MarketFeedSimulator {
    fn default() -> Self {
        Self::new()
    }
}
