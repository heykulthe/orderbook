use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use orderbook::engine::{FeedConfig, MarketFeedSimulator};
use orderbook::optimized::book::L2Book;

/// Benchmark the update hot-path of the optimized L2Book.
///
/// Measures single-update and batch-100-update latency to characterise
/// the amortised cost of one incremental diff application through the
/// ring-buffer write path.
fn bench_book_update_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("book_update_latency");

    let tick_size = 0.1;
    let lot_size = 0.001;

    // Bootstrap a seeded book and pre-generate 100 incremental messages so that
    // simulator overhead is entirely excluded from the measured iterations.
    let mut sim = MarketFeedSimulator::new();
    let boot = sim.bootstrap_update();
    let mut book = L2Book::new(tick_size, lot_size);
    book.update(&boot, "SIM");

    let num_messages = 100;
    let mut messages = Vec::with_capacity(num_messages);
    for _ in 0..num_messages {
        messages.push(sim.next_update());
    }

    // Single update: isolates the cost of one incremental diff application.
    group.bench_function("single_update", |b| {
        let mut book_clone = book.clone();
        let mut msg_idx = 0;

        b.iter(|| {
            let msg = &messages[msg_idx % messages.len()];
            black_box(book_clone.update(black_box(msg), black_box("SIM")));
            msg_idx += 1;
        });
    });

    // Batch 100: amortises per-iteration setup and measures sustained throughput.
    group.bench_function("batch_100_updates", |b| {
        b.iter(|| {
            let mut book_clone = book.clone();
            for msg in &messages {
                black_box(book_clone.update(black_box(msg), black_box("SIM")));
            }
        });
    });

    group.finish();
}

/// Benchmark the read-path analytics of the optimized L2Book.
///
/// All four operations measured here sit on the signal-generation critical path
/// in a real trading system: best-bid/ask are required for every order decision,
/// mid-price is used for fair-value calculations, and imbalance drives short-term
/// alpha signals.  Each is measured in isolation against a warm, fully populated
/// book to give a clean per-operation latency figure.
fn bench_book_read_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("book_read_latency");

    let tick_size = 0.1;
    let lot_size = 0.001;

    let mut sim = MarketFeedSimulator::new();
    let boot = sim.bootstrap_update();
    let mut book = L2Book::new(tick_size, lot_size);
    book.update(&boot, "SIM");

    // Warm the book with a handful of ticks to ensure a non-trivial internal
    // state (both sides populated, ring buffer initialised past first recenter).
    for _ in 0..20 {
        let upd = sim.next_update();
        book.update(&upd, "SIM");
    }

    group.bench_function("best_bid", |b| {
        b.iter(|| black_box(book.best_bid()));
    });

    group.bench_function("best_ask", |b| {
        b.iter(|| black_box(book.best_ask()));
    });

    group.bench_function("mid_price", |b| {
        b.iter(|| black_box(book.mid_price()));
    });

    group.bench_function("imbalance", |b| {
        b.iter(|| black_box(book.orderbook_imbalance()));
    });

    group.finish();
}

/// Benchmark update latency as a function of synthetic book depth.
///
/// Uses `FeedConfig::depth` to control the number of active price levels per
/// side, which directly determines the number of diffs produced per update
/// message.  Sweeping across depths [5, 10, 20, 50] reveals how the book's
/// insertion, deletion, and recenter logic scales with the size of each diff
/// payload  a key consideration for instruments with wide, liquid books.
fn bench_book_depth_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("book_depth_scaling");

    let tick_size = 0.1;
    let lot_size = 0.001;

    for &depth in &[5_usize, 10, 20, 50] {
        let sim_config = FeedConfig {
            symbol: "BTC-USDT".to_string(),
            tick_size,
            lot_size,
            depth,
            dt_ms: 100,
            sigma_daily: 0.60,
        };

        let mut sim = MarketFeedSimulator::with_config(sim_config);
        let boot = sim.bootstrap_update();
        let mut book = L2Book::new(tick_size, lot_size);
        book.update(&boot, "SIM");

        // Pre-generate 50 updates at this depth to exclude simulator cost.
        let mut messages = Vec::with_capacity(50);
        for _ in 0..50 {
            messages.push(sim.next_update());
        }

        group.bench_with_input(BenchmarkId::new("depth", depth), &depth, |b, _| {
            let mut book_clone = book.clone();
            let mut msg_idx = 0;

            b.iter(|| {
                let msg = &messages[msg_idx % messages.len()];
                black_box(book_clone.update(black_box(msg), black_box("SIM")));
                msg_idx += 1;
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_book_update_latency,
    bench_book_read_latency,
    bench_book_depth_scaling
);
criterion_main!(benches);
