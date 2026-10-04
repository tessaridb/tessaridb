/// How many cells a spatial index writes per record's geometry.
///
/// Unit: cells.
///
/// A record's covering is one entry per cell, so this is directly the index's
/// write amplification for a geometry: a point produces one entry whatever the
/// budget, and a country produces up to this many. It bounds the *store* rather
/// than the query, and the two want opposite things — more cells approximate the
/// shape more tightly and cost more to write, so the trade is per workload and
/// this is the workload-free starting point.
///
/// Sixteen because the covering halves its error roughly per level and stops as
/// soon as the next subdivision would exceed the budget, so a budget of sixteen
/// buys two full levels of refinement past the first cell that meets the box.
/// Eight was the alternative and refines one level less, which for an elongated
/// shape — a river, a road, a coastline, the shapes a bounding box already
/// serves worst — leaves the covering close to the box it started from.
///
/// It is a bound and not a target. A geometry needing fewer cells writes fewer,
/// and the covering keeps a **coarser** cell rather than dropping a finer one
/// when the budget runs out, so exceeding it costs candidates to refine and
/// never rows.
pub const SPATIAL_INDEX_CELLS_PER_RECORD: usize = 16;

/// How many cells a **query** box is covered by.
///
/// Unit: cells.
///
/// The other side of the same trade, and it is not the same number for the same
/// reasons. A record's covering is paid once per record at write time and
/// forever after in space; a query's is paid once per read and in nothing else,
/// so a query can afford to be finer. What it cannot afford is unboundedly
/// finer: each cell of a query covering costs one range scan **plus one lookup
/// per level above it**, so the read's fixed cost is linear in this number while
/// the candidates it saves are not.
///
/// Sixteen, measured (G058 C1, `benchmarks/2026-10-04-macos-aarch64-covering-budget.md`):
/// over a skewed disk corpus and street, district and country boxes, budgets 4
/// to 256 return the same records, and sixteen has the lowest street-level p99
/// (0.59 ms; 2.9 ms at 8, 1.05 ms at 32) while staying within 3 % of the best
/// p50 for the larger boxes, whose cost is the candidates they refine. Coarser
/// reads many more entries per street query; finer pays a scan and a lookup per
/// level for every extra cell.
///
/// It is a bound and not a target, with the same guarantee: the covering keeps a
/// coarser cell rather than dropping a finer one, so exhausting the budget costs
/// candidates to refine and never rows.
pub const SPATIAL_QUERY_CELLS: usize = 16;

/// How many entries a nearest-first walk will read from a cell's whole subtree
/// before it descends into that subtree instead.
///
/// Unit: index entries.
///
/// The walk over cells is best-first, and the tree it walks is **implicit**:
/// every cell exists at every level whether or not anything was ever written
/// there. Without a cut-off, reaching one record a kilometre away in an empty
/// region means opening a cell at each of the thirty-two levels on the way down,
/// and each of those is a seek that finds one entry or none.
///
/// So a cell is first read as a whole subtree, with a limit one above this
/// number. A short answer means the scan was not truncated — every entry under
/// that cell is in hand, the walk ranks them all and never descends. Only a
/// subtree that fills the limit is worth splitting into four.
///
/// Sixty-four, because four levels of descent cost four seeks and four scans to
/// find what one scan of sixty-four entries returns outright, and a region
/// holding fewer than this many records is not a region a walk needs to be
/// clever about. Larger wastes reads inside a dense cell that pruning would have
/// skipped; smaller reinstates the deep chain this exists to cut.
pub const SPATIAL_WALK_SUBTREE_ENTRIES: usize = 64;

/// The most records a nearest-first walk of a spatial index measures before it
/// gives the read to the scan (G058 C1).
///
/// Unit: records taken from the walk, each fetched, tested against the whole
/// condition and measured exactly. Without a condition a walk takes about as
/// many as it answers with; under a condition that the near records rarely meet
/// it could take the whole table one seek at a time, which costs more than the
/// scan it replaces. Past this many it declines, and the read says so.
pub const SPATIAL_NEAREST_EXAMINATION_CAP: usize = 4_096;
