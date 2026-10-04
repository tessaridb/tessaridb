//! Why a shape is refused at the door, and where its defect is.

use crate::grid::OffGrid;
use tessari_types::Position;

/// Why the store would not hold a shape.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Refused {
    /// A coordinate could not be put on the grid at all.
    #[error(transparent)]
    OffGrid(#[from] OffGrid),
    /// The snapped shape breaks a rule the store holds all its geometry to.
    #[error("{defect}, at {at}")]
    Malformed {
        /// What was wrong.
        defect: Defect,
        /// Where in the shape.
        at: Site,
    },
}

impl Refused {
    pub(super) fn malformed(defect: Defect, at: Site) -> Self {
        Self::Malformed { defect, at }
    }

    /// Say that this happened one level further in than it currently reads.
    ///
    /// Only the error path pays for the location, which is why the site is built
    /// outward from the defect rather than threaded down through every call.
    pub(super) fn under(self, step: Step) -> Self {
        match self {
            Self::Malformed { defect, mut at } => {
                at.0.insert(0, step);
                Self::Malformed { defect, at }
            }
            off_grid @ Self::OffGrid(_) => off_grid,
        }
    }
}

/// What was wrong with a snapped shape.
///
/// The list is closed. Every coordinate a variant carries is a **snapped**
/// coordinate — the store's version of the position, not the caller's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Defect {
    /// A path with fewer than two positions goes nowhere.
    #[error("a line needs two positions to be a path, and this one has {had}")]
    LineTooShort {
        /// How many it had.
        had: usize,
    },
    /// A ring needs three corners and the repeat that closes it.
    #[error("a ring needs four positions to bound an area, and this one has {had}")]
    RingTooShort {
        /// How many it had.
        had: usize,
    },
    /// The first and last positions of a ring differ.
    #[error("a ring must end where it began")]
    RingNotClosed,
    /// Two positions in a row are the same grid point.
    ///
    /// Very often this is snapping showing its work: the two were distinct as
    /// written and are one position at the store's resolution.
    #[error(
        "two positions in a row are the same grid point, longitude {} latitude {}",
        position.longitude,
        position.latitude
    )]
    RepeatedPosition {
        /// The grid point they both became.
        position: Position,
    },
    /// An edge whose direction round the world its own coordinates do not state.
    ///
    /// Two positions more than half the world apart in longitude can be joined
    /// two ways, and the wrapped one is the shorter. Coordinates are read as
    /// written, so the store would keep the longer path — which for a shape
    /// meant to cross the antimeridian is the complement of what was asked for,
    /// with nothing to report it. Neither reading is guessed.
    #[error(
        "an edge from longitude {} latitude {} to longitude {} latitude {} spans more than half the world, so which way round it goes is not stated: to cross the antimeridian, split the shape in two at ±180; to mean the long way round, put a position between the two ends",
        from.longitude,
        from.latitude,
        to.longitude,
        to.latitude
    )]
    EdgeSpansHalfTheWorld {
        /// Where the edge starts.
        from: Position,
        /// Where it ends.
        to: Position,
    },
    /// A ring that closes but encloses nothing.
    #[error("the ring encloses no area")]
    RingHasNoArea,
    /// Two edges of one ring meet where they should not.
    ///
    /// A crossing and a touch are both this: a ring that meets itself anywhere
    /// other than at a shared corner has an inside the store cannot name.
    #[error(
        "the ring meets itself: an edge from longitude {} latitude {} reaches an edge from longitude {} latitude {}",
        from.longitude,
        from.latitude,
        meets.longitude,
        meets.latitude
    )]
    RingSelfIntersects {
        /// Where one of the two edges starts.
        from: Position,
        /// Where the other starts.
        meets: Position,
    },
    /// A hole reaches outside the shell it is cut from.
    #[error(
        "a hole reaches outside its shell, at longitude {} latitude {}",
        position.longitude,
        position.latitude
    )]
    HoleOutsideShell {
        /// A position on the part that is outside.
        position: Position,
    },
    /// Two holes of one polygon share area.
    #[error(
        "two holes of one polygon overlap, at longitude {} latitude {}",
        position.longitude,
        position.latitude
    )]
    HolesOverlap {
        /// A position in the shared part.
        position: Position,
    },
    /// Two members of a multi-polygon share area, or share a stretch of edge.
    ///
    /// Named by position in the collection rather than by coordinate: what is
    /// wrong is the pair, and the place where they meet is a rational point
    /// rather than a grid one, so a coordinate here would be a rounded
    /// approximation of the complaint.
    #[error("member {earlier} and member {later} of a multi-polygon share area")]
    MembersOverlap {
        /// The earlier of the two, counting from zero.
        earlier: usize,
        /// The later of the two.
        later: usize,
    },
}

/// Where in a shape a defect is.
///
/// Outermost step first, so it reads the way a person would point: the second
/// member, its first hole, its fifth position.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Site(Vec<Step>);

impl Site {
    /// A defect that belongs to the shape as a whole.
    #[must_use]
    pub const fn whole() -> Self {
        Self(Vec::new())
    }

    /// A defect one step in.
    #[must_use]
    pub fn at(step: Step) -> Self {
        Self(vec![step])
    }

    /// The steps, outermost first.
    #[must_use]
    pub fn steps(&self) -> &[Step] {
        &self.0
    }
}

impl core::fmt::Display for Site {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0.is_empty() {
            return f.write_str("the shape itself");
        }
        for (index, step) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(" → ")?;
            }
            write!(f, "{step}")?;
        }
        Ok(())
    }
}

/// One step into a shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Step {
    /// The nth shape of a multi-shape or a collection, counting from zero.
    Member(usize),
    /// A polygon's outer ring.
    Shell,
    /// The nth ring cut out of a polygon, counting from zero.
    Hole(usize),
    /// The nth position, counting from zero.
    Position(usize),
}

impl core::fmt::Display for Step {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Member(index) => write!(f, "member {index}"),
            Self::Shell => f.write_str("the shell"),
            Self::Hole(index) => write!(f, "hole {index}"),
            Self::Position(index) => write!(f, "position {index}"),
        }
    }
}

// ------------------------------------------------------------ the shapes
