use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use orderbook::engine::{FeedConfig, MarketFeedSimulator};
use orderbook::optimized::book::L2Book;

/// Benchmark the hot-path update performance of the optimized L2Book.
///
/// Tests single-update, batch-10, and batch-100 throughput to characterise
/// latency distribution across different workload sizes. Messages are
/// pre-generated to isolate book update cost from simulator overhead.
fn bench_book_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("book_update");

    let tick_size = 0.1;
    let lot_size = 0.001;

    // Seed the book with a bootstrap snapshot.
    let mut sim = MarketFeedSimulator::new();
    let boot = sim.bootstrap_update();
    let mut book = L2Book::new(tick_size, lot_size);
    book.update(&boot, "SIM");

    // Pre-generate 100 incremental updates to avoid measuring simulator cost.
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

    // Batch 10: captures short-burst latency over a small run of updates.
    group.bench_function("batch_10_updates", |b| {
        b.iter(|| {
            let mut book_clone = book.clone();
            for i in 0..10 {
                let msg = &messages[i % messages.len()];
                black_box(book_clone.update(black_box(msg), black_box("SIM")));
            }
        });
    });

    // Batch 100: amortises per-iteration overhead and measures sustained throughput.
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

/// Benchmark update latency across a range of synthetic book depths.
///
/// Uses `FeedConfig` to control the number of price levels per side.
/// Increasing depth raises the number of diffs per update message, exercising
/// the book's insertion and deletion paths more aggressively.
fn bench_book_update_by_depth(c: &mut Criterion) {
    let mut group = c.benchmark_group("book_update_by_depth");

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

        // Pre-generate 50 updates at this depth configuration.
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

/// Benchmark all read-path operations on a fully initialised L2Book.
///
/// Covers every public analytic that sits on the signal-generation critical
/// path: BBO, mid-price, full and depth-bounded imbalance, and top-N level
/// extraction for both sides.
fn bench_book_read_ops(c: &mut Criterion) {
    let mut group = c.benchmark_group("book_read_ops");

    let tick_size = 0.1;
    let lot_size = 0.001;

    let mut sim = MarketFeedSimulator::new();
    let boot = sim.bootstrap_update();
    let mut book = L2Book::new(tick_size, lot_size);
    book.update(&boot, "SIM");

    // Warm the book with a few extra ticks to ensure non-trivial state.
    for _ in 0..10 {
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

    group.bench_function("orderbook_imbalance", |b| {
        b.iter(|| black_box(book.orderbook_imbalance()));
    });

    group.bench_function("orderbook_imbalance_depth_5", |b| {
        b.iter(|| black_box(book.orderbook_imbalance_depth(5)));
    });

    group.bench_function("orderbook_imbalance_depth_10", |b| {
        b.iter(|| black_box(book.orderbook_imbalance_depth(10)));
    });

    group.bench_function("top_bids_10", |b| {
        b.iter(|| black_box(book.top_bids(10)));
    });

    group.bench_function("top_asks_10", |b| {
        b.iter(|| black_box(book.top_asks(10)));
    });

    group.finish();
}

/// Benchmark `MarketFeedSimulator` message-production throughput.
///
/// Measures the cost of generating a single incremental update and a full
/// bootstrap snapshot independently, making it possible to profile the feed
/// pipeline without involving the book consumer at all.
fn bench_simulator_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("simulator_throughput");

    // next_update: the steady-state per-tick cost.
    group.bench_function("next_update", |b| {
        let mut sim = MarketFeedSimulator::new();
        let boot = sim.bootstrap_update();
        let tick_size = 0.1;
        let lot_size = 0.001;
        let mut book = L2Book::new(tick_size, lot_size);
        book.update(&boot, "SIM");

        b.iter(|| {
            black_box(sim.next_update());
        });
    });

    // bootstrap_update: the one-shot full-snapshot cost.
    group.bench_function("bootstrap_update", |b| {
        b.iter(|| {
            let mut sim = MarketFeedSimulator::new();
            black_box(sim.bootstrap_update());
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_book_update,
    bench_book_update_by_depth,
    bench_book_read_ops,
    bench_simulator_throughput
);
criterion_main!(benches);
