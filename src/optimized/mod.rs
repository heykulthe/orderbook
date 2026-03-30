/// High-performance L2 order book backed by a fixed-size ring buffer and a
/// bitset presence index.
///
/// Price levels are mapped to contiguous array slots via modular arithmetic,
/// giving O(1) insert, update, and removal without any heap allocation per
/// operation. The bitset tracks which slots are occupied so that best-bid /
/// best-ask scans touch only cache-warm words rather than iterating the full
/// price range.
pub mod book;

pub use book::L2Book;
