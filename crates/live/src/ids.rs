//! Stable row, column and sheet ids.
//!
//! An id is a `u64` `(space << 32) | n`. Space 0 belongs to the base file: the line at base
//! index `i` is `n = i + 1`, so `0` is never a line. Every other space is granted to one peer by
//! the relay at the join, and that peer mints `n` in it. In text (yrs keys, the formula id form)
//! an id is lowercase base 36, and a cell key is `"<row id>.<col id>"`.

/// A row, column or sheet id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(pub u64);

const RADIX: u64 = 36;
const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

impl Id {
    /// The base file's line (or sheet) at 0-based `index`: `index + 1` in space 0.
    #[must_use]
    pub fn base(index: u32) -> Id {
        Id(u64::from(index) + 1)
    }

    /// The `n`th id minted in `space`.
    #[must_use]
    pub fn minted(space: u32, n: u32) -> Id {
        Id((u64::from(space) << 32) | u64::from(n))
    }

    /// The space this id was minted in (0 for the base file).
    #[must_use]
    pub fn space(self) -> u32 {
        // The shift leaves at most 32 bits, so this never saturates.
        u32::try_from(self.0 >> 32).unwrap_or(u32::MAX)
    }

    /// The base index this id stands for, when it is a base id.
    #[must_use]
    pub fn base_index(self) -> Option<u32> {
        if self.space() != 0 || self.0 == 0 {
            return None;
        }
        u32::try_from(self.0 - 1).ok()
    }

    /// Lowercase base 36, with no leading zeros.
    #[must_use]
    pub fn text(self) -> String {
        let mut n = self.0;
        let mut out = Vec::new();
        loop {
            // `n % 36` is below 36, so the index is always in the table.
            let d = usize::try_from(n % RADIX).unwrap_or(0);
            out.push(DIGITS.get(d).copied().unwrap_or(b'0'));
            n /= RADIX;
            if n == 0 {
                break;
            }
        }
        out.reverse();
        String::from_utf8(out).unwrap_or_default()
    }

    /// The id [`text`](Self::text) printed: lowercase base 36 with no leading zeros, and no
    /// larger than `u64`. Anything else is `None`, so `parse(s)` and `text` are inverse.
    #[must_use]
    pub fn parse(s: &str) -> Option<Id> {
        let bytes = s.as_bytes();
        if bytes.is_empty() || (bytes.len() > 1 && bytes.first() == Some(&b'0')) {
            return None;
        }
        let mut n: u64 = 0;
        for b in bytes {
            let d = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'z' => b - b'a' + 10,
                _ => return None,
            };
            n = n.checked_mul(RADIX)?.checked_add(u64::from(d))?;
        }
        Some(Id(n))
    }
}

/// The key of the cell at slot `(row, col)`: `"<row id>.<col id>"`.
#[must_use]
pub fn cell_key(row: Id, col: Id) -> String {
    format!("{}.{}", row.text(), col.text())
}

/// The slot a cell key names, or `None` when it is not one [`cell_key`] prints.
#[must_use]
pub fn parse_cell_key(k: &str) -> Option<(Id, Id)> {
    let (r, c) = k.split_once('.')?;
    Some((Id::parse(r)?, Id::parse(c)?))
}
