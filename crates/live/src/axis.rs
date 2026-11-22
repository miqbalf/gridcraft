//! One sheet axis (rows or columns) as ids.
//!
//! The axis is an explicit prefix of ids followed by the implicit continuation of base ids: past
//! the prefix, position by position, come the base ids after the largest base id the prefix
//! holds. Deleting an id adds it to the deleted set; nothing is ever taken out of the prefix, so
//! undoing a delete is taking the id out of that set again. Only positions below `limit` (the
//! sheet's row or column count) are shown; lines pushed past it by an insert are not.
//!
//! The axis is written only under the structure lock, so every peer applies the same edits in
//! the same order and holds the same axis.

use std::collections::{BTreeSet, HashMap};

use crate::ids::Id;
use gridcraft_formula::ids::Lookup;

/// Rows or columns.
pub use gridcraft_formula::adjust::Axis as AxisKind;

/// The first id of space 1: every base id is below it.
const SPACE_ONE: u64 = 1 << 32;
/// The furthest into the implicit tail an op's `prev` may reach (more than the sheet has lines):
/// a junk id past it is treated as unknown instead of materialising billions of ids.
const TAIL_REACH: u64 = 2_100_000;

/// One axis as ids. See the [module documentation](self).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Axis {
    /// The explicit prefix, each id once, in order.
    explicit: Vec<Id>,
    deleted: BTreeSet<Id>,
    limit: u32,
    /// Derived from the three above.
    /// The explicit ids that are shown, in order.
    shown: Vec<Id>,
    /// For each explicit id: its index in `explicit`, and how many shown ids come before it.
    index: HashMap<Id, (usize, u64)>,
    /// The base id the implicit tail starts with.
    next_base: u64,
}

/// Where an insert lands: the array index the new ids go to, the base ids to append to the array
/// first (they were positions in the implicit tail), and the array element the new ids follow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Insertion {
    pub index: u32,
    pub materialise: Vec<Id>,
    pub prev: Option<Id>,
}

impl Axis {
    /// The base file's axis: every position holds its base id.
    #[must_use]
    pub fn base(limit: u32) -> Axis {
        Axis::from_parts(Vec::new(), BTreeSet::new(), limit)
    }

    /// An axis from its stored parts. An id that appears twice in `explicit` counts at its first
    /// place only, as every replica reading the same array does.
    #[must_use]
    pub fn from_parts(explicit: Vec<Id>, deleted: BTreeSet<Id>, limit: u32) -> Axis {
        let mut seen = BTreeSet::new();
        let explicit: Vec<Id> = explicit.into_iter().filter(|id| seen.insert(*id)).collect();
        let mut axis = Axis { explicit, deleted, limit, shown: Vec::new(), index: HashMap::new(), next_base: 1 };
        axis.derive();
        axis
    }

    /// The explicit prefix, in order.
    #[must_use]
    pub fn explicit(&self) -> &[Id] {
        &self.explicit
    }

    /// Every deleted id.
    #[must_use]
    pub fn deleted(&self) -> &BTreeSet<Id> {
        &self.deleted
    }

    /// How many positions are shown.
    #[must_use]
    pub fn limit(&self) -> u32 {
        self.limit
    }

    fn derive(&mut self) {
        self.shown.clear();
        self.index.clear();
        let mut next_base = 1;
        for (i, id) in self.explicit.iter().enumerate() {
            self.index.insert(*id, (i, self.shown.len() as u64));
            if !self.deleted.contains(id) {
                self.shown.push(*id);
            }
            if id.space() == 0 && id.0 >= next_base {
                next_base = id.0 + 1;
            }
        }
        self.next_base = next_base;
    }

    /// The explicit lines that are shown, in order: positions `0..len`.
    #[must_use]
    pub fn shown(&self) -> &[Id] {
        &self.shown
    }

    /// Whether this is the base file's own axis: nothing explicit, nothing deleted.
    #[must_use]
    pub fn is_base(&self) -> bool {
        self.explicit.is_empty() && self.deleted.is_empty()
    }

    /// The id shown at `pos`, if `pos` is on the sheet.
    #[must_use]
    pub fn id_at(&self, pos: u32) -> Option<Id> {
        if pos >= self.limit {
            return None;
        }
        let pos = usize::try_from(pos).ok()?;
        match self.shown.get(pos) {
            Some(id) => Some(*id),
            None => self.tail_id(u64::try_from(pos - self.shown.len()).ok()?),
        }
    }

    /// The base id shown `offset` positions into the implicit tail.
    fn tail_id(&self, offset: u64) -> Option<Id> {
        let mut candidate = self.next_base.checked_add(offset)?;
        for d in self.deleted.range(Id(self.next_base)..Id(SPACE_ONE)) {
            if d.0 <= candidate {
                candidate += 1;
            } else {
                break;
            }
        }
        (candidate < SPACE_ONE).then_some(Id(candidate))
    }

    /// How many lines are shown before `id`, and whether `id` is on this axis at all.
    fn shown_before(&self, id: Id) -> Option<u64> {
        if let Some((_, before)) = self.index.get(&id) {
            return Some(*before);
        }
        if id.space() != 0 || id.0 < self.next_base {
            return None;
        }
        let gone = self.deleted.range(Id(self.next_base)..id).count() as u64;
        Some(self.shown.len() as u64 + (id.0 - self.next_base) - gone)
    }

    fn on_sheet(&self, pos: u64) -> Option<u32> {
        u32::try_from(pos).ok().filter(|p| *p < self.limit)
    }

    /// Where `id` is now.
    #[must_use]
    pub fn pos_of(&self, id: Id) -> Lookup {
        let Some(q) = self.shown_before(id) else {
            return Lookup::Unknown;
        };
        if self.deleted.contains(&id) {
            return Lookup::Deleted { before: q.checked_sub(1).and_then(|p| self.on_sheet(p)), after: self.on_sheet(q) };
        }
        self.on_sheet(q).map_or(Lookup::Unknown, Lookup::At)
    }

    /// Insert `ids` so the first lands at `pos` (clamped to the sheet), and say where in the
    /// array that is. Ids the axis already holds are left out; the doc's write does the same.
    pub fn insert(&mut self, pos: u32, ids: &[Id]) -> Insertion {
        let pos = pos.min(self.limit);
        let at = usize::try_from(pos).unwrap_or(usize::MAX);
        let (index, materialise) = match self.shown.get(at) {
            Some(target) => (self.index.get(target).map_or(self.explicit.len(), |(i, _)| *i), Vec::new()),
            None => {
                let offset = u64::try_from(at.saturating_sub(self.shown.len())).unwrap_or(u64::MAX);
                let upto = self.tail_id(offset).map_or(SPACE_ONE, |id| id.0);
                let materialise: Vec<Id> = (self.next_base..upto).map(Id).collect();
                (self.explicit.len() + materialise.len(), materialise)
            }
        };
        self.explicit.extend(materialise.iter().copied());
        let prev = index.checked_sub(1).and_then(|i| self.explicit.get(i).copied());
        let mut fresh = BTreeSet::new();
        let new: Vec<Id> = ids.iter().copied().filter(|id| !self.index.contains_key(id) && !materialise.contains(id) && fresh.insert(*id)).collect();
        let index = index.min(self.explicit.len());
        self.explicit.splice(index..index, new);
        self.derive();
        Insertion { index: u32::try_from(index).unwrap_or(u32::MAX), materialise, prev }
    }

    /// Insert `ids` right after the array element `prev` (at the front when `None`), as an
    /// op-log entry says. A `prev` in the implicit tail materialises the base ids up to it; one
    /// the axis does not know appends. Returns where the first new line is shown now (clamped to
    /// the sheet) and how many were new.
    pub fn insert_after(&mut self, prev: Option<Id>, ids: &[Id]) -> (u32, usize) {
        let index = match prev {
            None => 0,
            Some(p) => match self.index.get(&p) {
                Some((i, _)) => i + 1,
                None if p.space() == 0 && p.0 >= self.next_base && p.0 - self.next_base <= TAIL_REACH => {
                    let upto = p.0;
                    let from = self.next_base;
                    self.explicit.extend((from..=upto).map(Id));
                    self.derive();
                    self.explicit.len()
                }
                None => self.explicit.len(),
            },
        };
        let mut fresh = BTreeSet::new();
        let new: Vec<Id> = ids.iter().copied().filter(|id| !self.index.contains_key(id) && fresh.insert(*id)).collect();
        let count = new.len();
        let index = index.min(self.explicit.len());
        self.explicit.splice(index..index, new);
        self.derive();
        let first = ids.iter().find_map(|id| match self.pos_of(*id) {
            Lookup::At(p) => Some(p),
            _ => None,
        });
        // Not shown (pushed past the sheet, or deleted): the number of shown lines before the gap.
        let at = first.unwrap_or_else(|| {
            let before = self.explicit.get(..index).map_or(0, |e| e.iter().filter(|i| !self.deleted.contains(*i)).count());
            u32::try_from(before).unwrap_or(u32::MAX).min(self.limit)
        });
        (at, count)
    }

    /// Delete `ids`. Returns the positions they were shown at, before the delete, as ascending
    /// `(first, count)` areas. Ids not shown are marked deleted too but give no position.
    pub fn delete(&mut self, ids: &[Id]) -> Vec<(u32, u32)> {
        let positions: BTreeSet<u32> = ids
            .iter()
            .filter_map(|id| match self.pos_of(*id) {
                Lookup::At(p) => Some(p),
                _ => None,
            })
            .collect();
        self.deleted.extend(ids.iter().copied());
        self.derive();
        areas(&positions)
    }

    /// Bring `ids` back. Returns the positions they are shown at after it, as ascending
    /// `(first, count)` areas.
    pub fn undelete(&mut self, ids: &[Id]) -> Vec<(u32, u32)> {
        for id in ids {
            self.deleted.remove(id);
        }
        self.derive();
        let positions: BTreeSet<u32> = ids
            .iter()
            .filter_map(|id| match self.pos_of(*id) {
                Lookup::At(p) => Some(p),
                _ => None,
            })
            .collect();
        areas(&positions)
    }
}

/// Ascending positions as `(first, count)` runs.
fn areas(positions: &BTreeSet<u32>) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for p in positions {
        match out.last_mut() {
            Some((first, count)) if first.checked_add(*count) == Some(*p) => *count += 1,
            _ => out.push((*p, 1)),
        }
    }
    out
}
