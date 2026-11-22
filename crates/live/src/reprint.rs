//! Formulas after an ordered edit: which formula cells name a line, and the text a formula has
//! once a delete has shrunk the ends of its ranges.
//!
//! Shared formulas are id form (`@{r:5,c:1}`). An id-form formula never changes when lines are
//! inserted or deleted, because it prints through the current axis. Two things still need the
//! text: the receivers re-print the formulas that name an op's ids from the id-form source (the
//! state wins over the engine's `adjust`), and the author of a delete rewrites a range end that
//! pointed at a deleted line so it names the nearest surviving line (so a later insert at the
//! range's new top moves the range, as in Excel).

use std::collections::{BTreeSet, HashMap};

use crate::axis::{Axis, AxisKind};
use crate::ids::Id;
use gridcraft_formula::ids::Lookup;

/// One field of an id token: its value and where its digits are in the text.
type Field = Option<(u64, (usize, usize))>;

#[derive(Clone, Copy, Debug, Default)]
struct End {
    sheet: Field,
    row: Field,
    col: Field,
}

#[derive(Debug)]
struct Tok {
    first: End,
    second: Option<End>,
}

fn token_starts(b: &[u8], i: usize) -> bool {
    b.get(i..i + 2) == Some(b"@{") && matches!(b.get(i + 2), Some(b's' | b'r' | b'c')) && b.get(i + 3) == Some(&b':')
}

fn read_id(b: &[u8], i: &mut usize) -> Option<(u64, (usize, usize))> {
    let start = *i;
    while b.get(*i).is_some_and(|c| c.is_ascii_digit() || c.is_ascii_lowercase()) {
        *i += 1;
    }
    let digits = std::str::from_utf8(b.get(start..*i)?).ok()?;
    Some((u64::from_str_radix(digits, 36).ok()?, (start, *i)))
}

fn read_end(b: &[u8], i: &mut usize) -> Option<End> {
    *i += 2;
    let mut end = End::default();
    loop {
        let tag = (b.get(*i).copied(), b.get(*i + 1).copied());
        *i += 2;
        match tag {
            (Some(b's'), Some(b':')) => end.sheet = Some(read_id(b, i)?),
            (Some(b'r'), Some(b':')) => end.row = Some(read_id(b, i)?),
            (Some(b'c'), Some(b':')) => end.col = Some(read_id(b, i)?),
            (Some(b'$'), Some(b'r' | b'c')) => {}
            _ => return None,
        }
        match b.get(*i) {
            Some(b',') => *i += 1,
            Some(b'}') => {
                *i += 1;
                return Some(end);
            }
            _ => return None,
        }
    }
}

const LINKS: [&str; 4] = [".:.", ".:", ":.", ":"];

fn tokens(text: &str) -> Vec<Tok> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b.get(i) {
            Some(b'"') => {
                i += 1;
                while let Some(&c) = b.get(i) {
                    i += 1;
                    if c == b'"' {
                        if b.get(i) == Some(&b'"') {
                            i += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
            _ if token_starts(b, i) => {
                let mut j = i;
                let Some(first) = read_end(b, &mut j) else {
                    i += 2;
                    continue;
                };
                let mut second = None;
                for link in LINKS {
                    let after = j + link.len();
                    if b.get(j..after) == Some(link.as_bytes()) && token_starts(b, after) {
                        let mut k = after;
                        if let Some(e) = read_end(b, &mut k) {
                            let same = first.row.is_some() == e.row.is_some() && first.col.is_some() == e.col.is_some();
                            if e.sheet.is_none() && same {
                                second = Some(e);
                                j = k;
                            }
                        }
                        break;
                    }
                }
                out.push(Tok { first, second });
                i = j;
            }
            _ => i += 1,
        }
    }
    out
}

/// What a formula's text names: `(sheet id or the host's, axis, line id)`.
#[must_use]
pub fn named_lines(text: &str, host: u64) -> Vec<(u64, AxisKind, u64)> {
    let mut out = Vec::new();
    for t in tokens(text) {
        let ends = std::iter::once(&t.first).chain(t.second.iter());
        let sheet = t.first.sheet.map_or(host, |(s, _)| s);
        for e in ends {
            if let Some((r, _)) = e.row {
                out.push((sheet, AxisKind::Rows, r));
            }
            if let Some((c, _)) = e.col {
                out.push((sheet, AxisKind::Cols, c));
            }
        }
    }
    out
}

/// `text` with every range end on `axis` of sheet `sheet` that names a deleted line moved to the
/// nearest surviving line inside the range, using `after` (the axis once the delete is done). A
/// range with no line left is left alone (it prints `#REF!`). `None` when nothing changes.
#[must_use]
pub fn shrink_ends(text: &str, host: u64, sheet: Id, axis: AxisKind, after: &Axis) -> Option<String> {
    let mut edits: Vec<((usize, usize), String)> = Vec::new();
    for t in tokens(text) {
        let Some(second) = t.second else { continue };
        if t.first.sheet.map_or(host, |(s, _)| s) != sheet.0 {
            continue;
        }
        let pick = |e: &End| if axis == AxisKind::Rows { e.row } else { e.col };
        let (Some((a, a_span)), Some((z, z_span))) = (pick(&t.first), pick(&second)) else { continue };
        let (la, lz) = (after.pos_of(Id(a)), after.pos_of(Id(z)));
        let start = match la {
            Lookup::At(p) => Some(p),
            Lookup::Deleted { after, .. } => after,
            Lookup::Unknown => None,
        };
        let end = match lz {
            Lookup::At(p) => Some(p),
            Lookup::Deleted { before, .. } => before,
            Lookup::Unknown => None,
        };
        let (Some(s), Some(e)) = (start, end) else { continue };
        if s > e {
            continue;
        }
        if matches!(la, Lookup::Deleted { .. })
            && let Some(id) = after.id_at(s)
        {
            edits.push((a_span, id.text()));
        }
        if matches!(lz, Lookup::Deleted { .. })
            && let Some(id) = after.id_at(e)
        {
            edits.push((z_span, id.text()));
        }
    }
    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|(span, _)| std::cmp::Reverse(span.0));
    let mut out = text.to_owned();
    for ((from, to), new) in edits {
        out.replace_range(from..to, &new);
    }
    Some(out)
}

/// Which formula cells name each line: `(sheet id, axis, line id)` to `(host sheet id, cell key)`.
#[derive(Debug, Default)]
pub struct RefIndex {
    by_line: HashMap<(u64, bool, u64), BTreeSet<(u64, String)>>,
    by_cell: HashMap<(u64, String), Vec<(u64, bool, u64)>>,
    /// Built from the base and the written formulas.
    pub built: bool,
}

impl RefIndex {
    /// Forgets everything; the next use builds it again.
    pub fn clear(&mut self) {
        self.by_line.clear();
        self.by_cell.clear();
        self.built = false;
    }

    /// Records (or with `None` forgets) the formula at `(host, key)`.
    pub fn set(&mut self, host: u64, key: &str, text: Option<&str>) {
        if let Some(old) = self.by_cell.remove(&(host, key.to_owned())) {
            for l in old {
                if let Some(set) = self.by_line.get_mut(&l) {
                    set.remove(&(host, key.to_owned()));
                }
            }
        }
        let Some(text) = text else { return };
        let lines: Vec<(u64, bool, u64)> = named_lines(text, host).into_iter().map(|(s, a, id)| (s, a == AxisKind::Cols, id)).collect();
        if lines.is_empty() {
            return;
        }
        for l in &lines {
            self.by_line.entry(*l).or_default().insert((host, key.to_owned()));
        }
        self.by_cell.insert((host, key.to_owned()), lines);
    }

    /// The formula cells that name any of `ids` on `axis` of `sheet`.
    #[must_use]
    pub fn naming(&self, sheet: Id, axis: AxisKind, ids: &[Id]) -> BTreeSet<(u64, String)> {
        let cols = axis == AxisKind::Cols;
        ids.iter().filter_map(|id| self.by_line.get(&(sheet.0, cols, id.0))).flatten().cloned().collect()
    }
}
