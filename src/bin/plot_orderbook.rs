use orderbook::engine::{FeedConfig, MarketFeedSimulator};
use orderbook::optimized::book::L2Book;
use plotters::prelude::*;
use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};

/// Lightweight snapshot structure for plotting (avoids cloning the entire L2Book).
struct PlotSnapshot {
    top_bids: Vec<(i64, f64)>, // (price_tick, qty)
    top_asks: Vec<(i64, f64)>,
    mid_price_dollars: Option<f64>,
}

/// Captured BBO information written to the CSV log for later validation.
struct BboRecord {
    best_bid_tick: Option<i64>,
    best_bid_qty: Option<f64>,
    best_bid_dollars: Option<f64>,
    best_ask_tick: Option<i64>,
    best_ask_qty: Option<f64>,
    best_ask_dollars: Option<f64>,
    mid_price_dollars: Option<f64>,
}

/// Linearly interpolates between two RGB colours by `ratio` ∈ [0, 1].
fn interpolate_color(ratio: f64, start: RGBColor, end: RGBColor) -> RGBColor {
    let r = (start.0 as f64 + (end.0 as f64 - start.0 as f64) * ratio) as u8;
    let g = (start.1 as f64 + (end.1 as f64 - start.1 as f64) * ratio) as u8;
    let b = (start.2 as f64 + (end.2 as f64 - start.2 as f64) * ratio) as u8;
    RGBColor(r, g, b)
}

/// Returns a bid colour shaded from dark blue (best price) to light blue (furthest level).
///
/// `depth_ratio`: 0.0 = best bid (dark blue), 1.0 = furthest bid (light blue).
fn bid_color(depth_ratio: f64) -> RGBColor {
    let light_blue = RGBColor(173, 216, 230);
    let dark_blue = RGBColor(0, 0, 139);
    interpolate_color(depth_ratio, dark_blue, light_blue)
}

/// Returns an ask colour shaded from dark red (best price) to light red (furthest level).
///
/// `depth_ratio`: 0.0 = best ask (dark red), 1.0 = furthest ask (light red).
fn ask_color(depth_ratio: f64) -> RGBColor {
    let light_red = RGBColor(255, 182, 193);
    let dark_red = RGBColor(139, 0, 0);
    interpolate_color(depth_ratio, dark_red, light_red)
}

/// Extracts a consistent snapshot from the book.
///
/// The mid-price is derived from the captured BBO so that it is guaranteed to
/// be consistent with the returned top-of-book levels rather than a separate
/// call that could race with an intervening update.
fn capture_snapshot(
    book: &L2Book,
    depth_levels: usize,
    tick_size: f64,
) -> (PlotSnapshot, BboRecord) {
    let top_bids = book.top_bids(depth_levels);
    let top_asks = book.top_asks(depth_levels);

    let best_bid = top_bids.first().copied();
    let best_ask = top_asks.first().copied();

    let best_bid_tick = best_bid.map(|(p, _)| p);
    let best_ask_tick = best_ask.map(|(p, _)| p);
    let best_bid_qty = best_bid.map(|(_, q)| q);
    let best_ask_qty = best_ask.map(|(_, q)| q);
    let best_bid_dollars = best_bid_tick.map(|tick| tick as f64 * tick_size);
    let best_ask_dollars = best_ask_tick.map(|tick| tick as f64 * tick_size);

    let mid_price_dollars = match (best_bid_tick, best_ask_tick) {
        (Some(bid), Some(ask)) if bid < ask => Some(((bid as f64 + ask as f64) * 0.5) * tick_size),
        _ => None,
    };

    let snapshot = PlotSnapshot {
        top_bids,
        top_asks,
        mid_price_dollars,
    };

    let bbo = BboRecord {
        best_bid_tick,
        best_bid_qty,
        best_bid_dollars,
        best_ask_tick,
        best_ask_qty,
        best_ask_dollars,
        mid_price_dollars,
    };

    (snapshot, bbo)
}

fn main() -> Result<(), Box<dyn Error>> {
    // ─── Simulation parameters ────────────────────────────────────────────────
    let num_snapshots = 200; // total number of chart frames
    let sample_every = 100; // take one snapshot every N feed updates
    let depth_levels = 10; // visible price levels per side
    let output_file = "orderbook_timeseries.png";
    let bbo_log_file = "orderbook_bbo_log.csv";

    println!("=== Orderbook Simulation ===");
    println!(
        "Generating {} orderbook snapshots (1 every {} updates)...",
        num_snapshots, sample_every
    );

    // ─── Feed configuration ───────────────────────────────────────────────────
    // High sigma_daily produces a highly visible Brownian motion path on the chart.
    let tick_size = 0.1;
    let lot_size = 0.001;
    let sim_config = FeedConfig {
        symbol: "BTC-USDT".to_string(),
        tick_size,
        lot_size,
        depth: 20,
        dt_ms: 100,
        sigma_daily: 10.0, // 1000% annualised — exaggerated for visual clarity
    };

    // ─── Initialise simulator and book ───────────────────────────────────────
    let mut sim = MarketFeedSimulator::with_config(sim_config);
    let mut book = L2Book::new(tick_size, lot_size);

    // Apply the full-book bootstrap snapshot to seed the book.
    let boot = sim.bootstrap_update();
    book.update(&boot, "SIM");

    // ─── Snapshot collection loop ─────────────────────────────────────────────
    let mut snapshots: Vec<PlotSnapshot> = Vec::with_capacity(num_snapshots);
    let mut bbo_log: Vec<BboRecord> = Vec::with_capacity(num_snapshots);

    // Capture the initial state (after bootstrap, before any incremental updates).
    let (snapshot, bbo_record) = capture_snapshot(&book, depth_levels, tick_size);
    snapshots.push(snapshot);
    bbo_log.push(bbo_record);

    for i in 1..num_snapshots {
        // Advance the feed by `sample_every` ticks between each captured frame.
        for _ in 0..sample_every {
            let upd = sim.next_update();
            book.update(&upd, "SIM");
        }

        let (snapshot, bbo_record) = capture_snapshot(&book, depth_levels, tick_size);
        snapshots.push(snapshot);
        bbo_log.push(bbo_record);

        if i % 50 == 0 {
            println!("  Progress: {}/{} snapshots", i, num_snapshots);
        }
    }

    println!(
        "Snapshots collected. Writing BBO log to {}...",
        bbo_log_file
    );
    write_bbo_log(bbo_log_file, &bbo_log)?;
    println!("BBO log saved. Generating chart...");

    // ─── Determine chart axis ranges ─────────────────────────────────────────
    let mut min_price_dollars = f64::MAX;
    let mut max_price_dollars = f64::MIN;
    let mut max_qty = 0.0_f64;

    for snapshot in &snapshots {
        for (price_tick, qty) in snapshot.top_bids.iter().chain(snapshot.top_asks.iter()) {
            let price_dollars = *price_tick as f64 * tick_size;
            min_price_dollars = min_price_dollars.min(price_dollars);
            max_price_dollars = max_price_dollars.max(price_dollars);
            max_qty = max_qty.max(*qty);
        }
    }

    if min_price_dollars == f64::MAX || max_price_dollars == f64::MIN {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "No orders found in snapshots — cannot determine price range.",
        )));
    }

    // Add a margin so the outermost levels are not clipped by the axis.
    let price_range = max_price_dollars - min_price_dollars;
    min_price_dollars -= price_range / 10.0;
    max_price_dollars += price_range / 10.0;

    // ─── Build the chart ──────────────────────────────────────────────────────
    let root = BitMapBackend::new(output_file, (1600, 900)).into_drawing_area();
    root.fill(&WHITE)?;

    let mut chart = ChartBuilder::on(&root)
        .caption("Orderbook Time Series", ("sans-serif", 40))
        .margin(10)
        .x_label_area_size(40)
        .y_label_area_size(80)
        .build_cartesian_2d(
            0_f64..(num_snapshots as f64),
            min_price_dollars..max_price_dollars,
        )?;

    chart
        .configure_mesh()
        .x_desc("Snapshot (Time)")
        .y_desc("Price (USD)")
        .draw()?;

    // ─── Draw price-level rectangles ──────────────────────────────────────────
    for (t, snapshot) in snapshots.iter().enumerate() {
        // Bids: best price at index 0 (dark blue) → furthest at index N (light blue).
        for (i, (price_tick, qty)) in snapshot.top_bids.iter().enumerate() {
            let depth_ratio = i as f64 / depth_levels.max(1) as f64;
            let color = bid_color(depth_ratio);
            let price_dollars = *price_tick as f64 * tick_size;

            let width = if max_qty > 0.0 {
                (qty / max_qty) * 0.8
            } else {
                0.0
            };

            let x_start = t as f64 + 0.5 - width / 2.0;
            let x_end = t as f64 + 0.5 + width / 2.0;
            let rect_height = 0.5 * tick_size; // adjacent levels touch

            chart.draw_series(std::iter::once(Rectangle::new(
                [
                    (x_start, price_dollars - rect_height),
                    (x_end, price_dollars + rect_height),
                ],
                color.filled(),
            )))?;
        }

        // Asks: best price at index 0 (dark red) → furthest at index N (light red).
        for (i, (price_tick, qty)) in snapshot.top_asks.iter().enumerate() {
            let depth_ratio = i as f64 / depth_levels.max(1) as f64;
            let color = ask_color(depth_ratio);
            let price_dollars = *price_tick as f64 * tick_size;

            let width = if max_qty > 0.0 {
                (qty / max_qty) * 0.8
            } else {
                0.0
            };

            let x_start = t as f64 + 0.5 - width / 2.0;
            let x_end = t as f64 + 0.5 + width / 2.0;
            let rect_height = 0.5 * tick_size;

            chart.draw_series(std::iter::once(Rectangle::new(
                [
                    (x_start, price_dollars - rect_height),
                    (x_end, price_dollars + rect_height),
                ],
                color.filled(),
            )))?;
        }
    }

    // ─── Draw mid-price line ──────────────────────────────────────────────────
    let mid_prices: Vec<(f64, f64)> = snapshots
        .iter()
        .enumerate()
        .filter_map(|(t, snapshot)| {
            // +0.5 centres the line within each time bucket.
            snapshot.mid_price_dollars.map(|mid| (t as f64 + 0.5, mid))
        })
        .collect();

    chart
        .draw_series(LineSeries::new(mid_prices, &BLACK))?
        .label("Mid Price")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], &BLACK));

    chart
        .configure_series_labels()
        .background_style(&WHITE.mix(0.8))
        .border_style(&BLACK)
        .draw()?;

    root.present()?;

    // ─── Summary ──────────────────────────────────────────────────────────────
    println!("Chart saved to: {}", output_file);
    println!("BBO log saved to: {}", bbo_log_file);
    println!("Number of snapshots: {}", num_snapshots);
    println!(
        "Min price: ${:.2}, Max price: ${:.2}",
        min_price_dollars, max_price_dollars
    );
    println!("Max quantity: {:.4}", max_qty);

    Ok(())
}

/// Writes the captured BBO records to a CSV file for offline validation.
///
/// Columns: `snapshot_index`, `best_bid_tick`, `best_bid_qty`, `best_bid_usd`,
///          `best_ask_tick`, `best_ask_qty`, `best_ask_usd`, `mid_usd`.
fn write_bbo_log(path: &str, entries: &[BboRecord]) -> Result<(), Box<dyn Error>> {
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);

    writeln!(
        writer,
        "snapshot_index,best_bid_tick,best_bid_qty,best_bid_usd,best_ask_tick,best_ask_qty,best_ask_usd,mid_usd"
    )?;

    for (idx, entry) in entries.iter().enumerate() {
        let bid_tick = entry
            .best_bid_tick
            .map(|v| v.to_string())
            .unwrap_or_default();
        let bid_qty = entry
            .best_bid_qty
            .map(|v| format!("{:.8}", v))
            .unwrap_or_default();
        let bid_usd = entry
            .best_bid_dollars
            .map(|v| format!("{:.8}", v))
            .unwrap_or_default();

        let ask_tick = entry
            .best_ask_tick
            .map(|v| v.to_string())
            .unwrap_or_default();
        let ask_qty = entry
            .best_ask_qty
            .map(|v| format!("{:.8}", v))
            .unwrap_or_default();
        let ask_usd = entry
            .best_ask_dollars
            .map(|v| format!("{:.8}", v))
            .unwrap_or_default();

        let mid_usd = entry
            .mid_price_dollars
            .map(|v| format!("{:.8}", v))
            .unwrap_or_default();

        writeln!(
            writer,
            "{idx},{},{},{},{},{},{},{}",
            bid_tick, bid_qty, bid_usd, ask_tick, ask_qty, ask_usd, mid_usd
        )?;
    }

    Ok(())
}
