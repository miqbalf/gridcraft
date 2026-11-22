//! The id form of a formula: references name rows, columns and sheets by stable ids instead of
//! positions, so a formula shared between peers keeps pointing at the same lines while rows and
//! columns are inserted and deleted around it.
//!
//! [`print_ids`] turns an A1 expression into id-form text, and [`parse_ids`] turns that text back
//! into an expression at the positions the layout has today. Ids are lowercase base 36. A cell is
//! `@{r:5,c:1}`; a `$` flag follows the id it anchors (`@{s:2,r:3,c:2,$r,$c}`, `s` is the sheet,
//! written only when the reference names one); whole rows and columns are `@{r:5}:@{r:7}` and
//! `@{c:1}:@{c:3}`; a range is `X:Y`, and a trim reference joins its ends with `.:`, `:.` or `.:.`.
//! Everything else is A1 text: names, structured references, `INDIRECT` text, 3-D spans and
//! external references. A reference the resolver cannot give ids for also stays A1 text, which
//! is read at today's positions.
//!
//! The crate stays at layer 1: the layout comes in through [`Resolver`], which this crate does
//! not implement.

use gridcraft_core::{CellError, MAX_COLS, MAX_ROWS};

use crate::adjust::Axis;
use crate::ast::{Anchor, Expr, RefKind, Reference, SheetSel, TrimMode, UnOp};
use crate::parser::{MAX_FORMULA_LEN, parse_unlimited};
use crate::printer::print;

/// What the id form needs to know about the current layout. No live dependency: the crate stays L1.
///
/// This module always passes `Some(sheet)` to [`Resolver::line_id`] and [`Resolver::line_pos`]
/// (the host sheet's id for an unqualified reference), so an implementation never has to guess
/// which sheet the formula lives on.
pub trait Resolver {
    fn sheet_id(&self, name: &str) -> Option<u64>;
    fn sheet_name(&self, id: u64) -> Option<String>;
    /// `sheet` None = the formula's own sheet.
    fn line_id(&self, sheet: Option<u64>, axis: Axis, pos: u32) -> Option<u64>;
    fn line_pos(&self, sheet: Option<u64>, axis: Axis, id: u64) -> Lookup;
}

/// Where a line is now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The line is at this position.
    At(u32),
    /// The line was deleted. `before` and `after` are the positions of the nearest surviving
    /// lines on each side.
    Deleted { before: Option<u32>, after: Option<u32> },
    /// The resolver has never heard of the id. It reads like a deleted line with no neighbours.
    Unknown,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    /// The id-form text is malformed; the byte offset is in the text given.
    #[error("id form syntax error at byte {0}")]
    Syntax(usize),
    /// A sheet name in the formula has no id.
    #[error("unknown sheet {0}")]
    UnknownSheet(String),
}

/// Joins the two ends of a range token; a trim reference swaps it for its own symbol.
const RANGE_JOIN: &str = "}:@{";

fn b36(n: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let mut digits = Vec::new();
    let mut n = n;
    while n > 0 {
        let d = (n % 36) as u8;
        digits.push(if d < 10 { b'0' + d } else { b'a' + d - 10 });
        n /= 36;
    }
    digits.reverse();
    String::from_utf8_lossy(&digits).into_owned()
}

// ------------------------------------------------------------------------------------ printing

/// One line coordinate of a reference end: position and `$` flag.
type Coord = Option<(u32, bool)>;

fn end_token(r: &impl Resolver, sheet: Option<u64>, lines: u64, row: Coord, col: Coord) -> Option<String> {
    let mut out = String::from("@{");
    if let Some(s) = sheet {
        out.push_str("s:");
        out.push_str(&b36(s));
        out.push(',');
    }
    let mut flags = String::new();
    for (axis, tag, coord) in [(Axis::Rows, 'r', row), (Axis::Cols, 'c', col)] {
        let Some((pos, abs)) = coord else { continue };
        let id = r.line_id(Some(lines), axis, pos)?;
        out.push(tag);
        out.push(':');
        out.push_str(&b36(id));
        out.push(',');
        if abs {
            flags.push('$');
            flags.push(tag);
            flags.push(',');
        }
    }
    out.push_str(&flags);
    out.pop(); // the trailing comma
    out.push('}');
    Some(out)
}

fn ref_token(rf: &Reference, host: u64, r: &impl Resolver) -> Result<Option<String>, IdError> {
    let (named, lines) = match &rf.sheet {
        SheetSel::Current => (None, host),
        SheetSel::Named(n) if n.contains('[') => return Ok(None),
        SheetSel::Named(n) => {
            let id = r.sheet_id(n).ok_or_else(|| IdError::UnknownSheet(n.clone()))?;
            (Some(id), id)
        }
        SheetSel::Span(..) => return Ok(None),
    };
    let row = |v: u32, abs: bool| Some((v, abs));
    let token = match &rf.kind {
        RefKind::Cell(a) => end_token(r, named, lines, row(a.row, a.row_abs), row(a.col, a.col_abs)),
        RefKind::Range(a, b) => end_token(r, named, lines, row(a.row, a.row_abs), row(a.col, a.col_abs)).and_then(|x| {
            let y = end_token(r, None, lines, row(b.row, b.row_abs), row(b.col, b.col_abs))?;
            Some(format!("{x}:{y}"))
        }),
        RefKind::Rows(r0, a0, r1, a1) => end_token(r, named, lines, row(*r0, *a0), None).and_then(|x| {
            let y = end_token(r, None, lines, row(*r1, *a1), None)?;
            Some(format!("{x}:{y}"))
        }),
        RefKind::Cols(c0, a0, c1, a1) => end_token(r, named, lines, None, row(*c0, *a0)).and_then(|x| {
            let y = end_token(r, None, lines, None, row(*c1, *a1))?;
            Some(format!("{x}:{y}"))
        }),
    };
    Ok(token)
}

/// A1 expression → id-form text: `@{r:5,c:1}`, `@{s:2,r:3,c:2,$r,$c}`, whole rows
/// `@{r:5}:@{r:7}`, whole columns `@{c:1}:@{c:3}` (always a pair, as in A1), ranges `X:Y`. Ids
/// are base 36. A range whose ends differ in shape or sheet (`A1:S2!B2`, `A1:B:B`) is two
/// tokens joined by the range operator. Names, structured references,
/// INDIRECT text, 3D spans and external references stay A1 text.
pub fn print_ids(e: &Expr, host_sheet: u64, r: &impl Resolver) -> Result<String, IdError> {
    let mut failure = None;
    let mapped = e.clone().map(&mut |x| match x {
        Expr::Ref(rf) => match ref_token(&rf, host_sheet, r) {
            Ok(Some(token)) => Expr::Name(token),
            Ok(None) => Expr::Ref(rf),
            Err(err) => {
                failure.get_or_insert(err);
                Expr::Ref(rf)
            }
        },
        Expr::Unary(UnOp::Trim(mode), inner) => match *inner {
            Expr::Name(token) if token.contains(RANGE_JOIN) => {
                let joined = format!("}}{}@{{", mode.symbol());
                Expr::Name(token.replacen(RANGE_JOIN, &joined, 1))
            }
            other => Expr::Unary(UnOp::Trim(mode), Box::new(other)),
        },
        other => other,
    });
    match failure {
        Some(err) => Err(err),
        None => Ok(print(&mapped)),
    }
}

// ------------------------------------------------------------------------------------ scanning

/// One end of a token: a cell (`row` and `col`), a row or a column.
struct End {
    sheet: Option<u64>,
    row: Option<(u64, bool)>,
    col: Option<(u64, bool)>,
}

/// A whole `@{...}` token with its optional second end.
struct Token {
    first: End,
    second: Option<End>,
    trim: Option<TrimMode>,
}

/// Whether an id token starts at `i`: `@{` and a field tag, which no array constant has.
fn token_starts(b: &[u8], i: usize) -> bool {
    b.get(i..i + 2) == Some(b"@{") && matches!(b.get(i + 2), Some(b's' | b'r' | b'c')) && b.get(i + 3) == Some(&b':')
}

fn read_id(b: &[u8], i: &mut usize) -> Result<u64, usize> {
    let start = *i;
    while b.get(*i).is_some_and(|c| c.is_ascii_digit() || c.is_ascii_lowercase()) {
        *i += 1;
    }
    let digits = b.get(start..*i).and_then(|d| std::str::from_utf8(d).ok()).ok_or(start)?;
    if digits.is_empty() {
        return Err(start);
    }
    u64::from_str_radix(digits, 36).map_err(|_| start)
}

/// Reads `@{...}` at `*i`.
fn read_end(b: &[u8], i: &mut usize) -> Result<End, usize> {
    let open = *i;
    *i += 2;
    let (mut sheet, mut row, mut col) = (None, None, None);
    let (mut row_abs, mut col_abs) = (false, false);
    loop {
        let at = *i;
        let tag = (b.get(*i).copied(), b.get(*i + 1).copied());
        *i += 2;
        match tag {
            (Some(b's'), Some(b':')) if sheet.is_none() => sheet = Some(read_id(b, i)?),
            (Some(b'r'), Some(b':')) if row.is_none() => row = Some(read_id(b, i)?),
            (Some(b'c'), Some(b':')) if col.is_none() => col = Some(read_id(b, i)?),
            (Some(b'$'), Some(b'r')) if !row_abs => row_abs = true,
            (Some(b'$'), Some(b'c')) if !col_abs => col_abs = true,
            _ => return Err(at),
        }
        match b.get(*i) {
            Some(b',') => *i += 1,
            Some(b'}') => {
                *i += 1;
                break;
            }
            _ => return Err(*i),
        }
    }
    if (row.is_none() && col.is_none()) || (row_abs && row.is_none()) || (col_abs && col.is_none()) {
        return Err(open);
    }
    Ok(End { sheet, row: row.map(|id| (id, row_abs)), col: col.map(|id| (id, col_abs)) })
}

/// The link between the two ends of a range token, and the trim it stands for.
const LINKS: [(&str, Option<TrimMode>); 4] =
    [(".:.", Some(TrimMode::All)), (".:", Some(TrimMode::Leading)), (":.", Some(TrimMode::Trailing)), (":", None)];

fn read_token(b: &[u8], i: &mut usize) -> Result<Token, usize> {
    let start = *i;
    let first = read_end(b, i)?;
    for (link, trim) in LINKS {
        let after = *i + link.len();
        if b.get(*i..after) == Some(link.as_bytes()) && token_starts(b, after) {
            let mut j = after;
            let second = read_end(b, &mut j)?;
            let same_shape = first.row.is_some() == second.row.is_some() && first.col.is_some() == second.col.is_some();
            // Ends that differ in sheet or shape are two references joined by the range
            // operator, as the parser keeps `A1:S2!B2` and `A1:B:B`: the token ends here.
            if second.sheet.is_none() && same_shape {
                *i = j;
                return Ok(Token { first, second: Some(second), trim });
            }
            break;
        }
    }
    // A row or column on its own is not a reference: whole lines always come as `X:Y`.
    if first.row.is_none() || first.col.is_none() {
        return Err(start);
    }
    Ok(Token { first, second: None, trim: None })
}

/// Index just past the string, quoted sheet name or bracketed part that starts at `i`.
fn skip_quoted(b: &[u8], i: usize) -> usize {
    let Some(&quote) = b.get(i) else { return i };
    let mut j = i + 1;
    while let Some(&c) = b.get(j) {
        j += 1;
        if c == quote {
            if b.get(j) == Some(&quote) {
                j += 1;
            } else {
                break;
            }
        }
    }
    j
}

fn skip_brackets(b: &[u8], i: usize) -> usize {
    let (mut j, mut depth) = (i + 1, 1usize);
    while depth > 0 {
        match b.get(j) {
            None => break,
            Some(b'\'') => j += 1,
            Some(b'[') => depth += 1,
            Some(b']') => depth -= 1,
            Some(_) => {}
        }
        j += 1;
    }
    j
}

/// Where a placeholder stands in the masked text, for mapping parse errors back.
struct Mask {
    text: String,
    /// `(masked start, masked length, original start, original length)`.
    spans: Vec<(usize, usize, usize, usize)>,
    tokens: Vec<Token>,
    prefix: String,
}

impl Mask {
    fn original_pos(&self, pos: usize) -> usize {
        let mut delta = 0i64;
        for &(ms, ml, os, ol) in &self.spans {
            if pos < ms {
                break;
            }
            if pos < ms + ml {
                return os;
            }
            delta += ol as i64 - ml as i64;
        }
        (pos as i64 + delta).max(0) as usize
    }
}

/// The text a token is masked as: a reference on a sheet no workbook has, so every operator
/// that applies to references (`#`, trim, ranges) parses around it.
fn placeholder(prefix: &str, index: usize, token: &Token) -> String {
    // The same `$` marks as the id form's, so the masked text lexes as the A1 text would.
    let end = |e: &End, row: &str, col: &str| {
        let d = |abs: Option<(u64, bool)>| if abs.is_some_and(|(_, a)| a) { "$" } else { "" };
        match (e.row, e.col) {
            (Some(_), Some(_)) => format!("{}{col}{}{row}", d(e.col), d(e.row)),
            (Some(_), None) => format!("{}{row}", d(e.row)),
            _ => format!("{}{col}", d(e.col)),
        }
    };
    let first = end(&token.first, "1", "A");
    let tail = match &token.second {
        Some(second) => format!("{}{}", token.trim.map_or(":", TrimMode::symbol), end(second, "2", "B")),
        None => String::new(),
    };
    format!("'{prefix}{index}_'!{first}{tail}")
}

/// Longest id-form text read. A formula of [`MAX_FORMULA_LEN`] characters has at most about 4,100
/// references, each at most about 70 bytes of id form, so this leaves room to spare.
const MAX_ID_TEXT: usize = 512 * 1024;

/// `_gcid` followed by more `x`s than any run after `_gcid` in `text` (compared without case),
/// so the prefix occurs nowhere in the text. One pass.
fn free_prefix(text: &str) -> String {
    const BASE: &[u8] = b"_gcid";
    let b = text.as_bytes();
    let mut longest = 0usize;
    let mut i = 0usize;
    while i + BASE.len() <= b.len() {
        if b.get(i..i + BASE.len()).is_some_and(|w| w.eq_ignore_ascii_case(BASE)) {
            let mut j = i + BASE.len();
            while b.get(j).is_some_and(|c| c.eq_ignore_ascii_case(&b'x')) {
                j += 1;
            }
            longest = longest.max(j - i - BASE.len());
            i = j;
        } else {
            i += 1;
        }
    }
    format!("_gcid{}", "x".repeat(longest + 1))
}

/// Replaces every id token by a name that cannot occur in the text, leaving strings, quoted
/// sheet names and structured references alone.
fn mask(text: &str) -> Result<Mask, IdError> {
    if text.len() > MAX_ID_TEXT {
        return Err(IdError::Syntax(MAX_ID_TEXT));
    }
    let prefix = free_prefix(text);
    let b = text.as_bytes();
    let mut m = Mask { text: String::with_capacity(text.len()), spans: Vec::new(), tokens: Vec::new(), prefix };
    let (mut i, mut copied) = (0usize, 0usize);
    while i < b.len() {
        match b.get(i) {
            Some(b'"' | b'\'') => i = skip_quoted(b, i),
            Some(b'[') => i = skip_brackets(b, i),
            Some(b'@') if token_starts(b, i) => {
                let start = i;
                let token = read_token(b, &mut i).map_err(IdError::Syntax)?;
                let glued = |c: Option<&u8>, extra: &[u8]| {
                    c.is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'!' | b'$' | b'\'') || extra.contains(c))
                };
                if glued(b.get(i), b"(\"") || glued(start.checked_sub(1).and_then(|p| b.get(p)), b"") {
                    return Err(IdError::Syntax(start));
                }
                m.text.push_str(text.get(copied..start).ok_or(IdError::Syntax(start))?);
                let name = placeholder(&m.prefix, m.tokens.len(), &token);
                m.spans.push((m.text.len(), name.len(), start, i - start));
                m.text.push_str(&name);
                m.tokens.push(token);
                copied = i;
            }
            _ => i += 1,
        }
    }
    m.text.push_str(text.get(copied..).ok_or(IdError::Syntax(copied))?);
    Ok(m)
}

// ------------------------------------------------------------------------------------ parsing

fn rep(l: Lookup) -> Option<i64> {
    match l {
        Lookup::At(p) => Some(2 * p as i64),
        Lookup::Deleted { before: Some(b), .. } => Some(2 * b as i64 + 1),
        Lookup::Deleted { before: None, after: Some(a) } => Some(2 * a as i64 - 1),
        _ => None,
    }
}

/// Both ends of a span on one axis, now. A deleted end moves inward to the nearest surviving
/// line; `None` when nothing of the span survives. Both ends in place stay as they are (a span
/// pushed past the last line is clamped to it, as a whole-sheet span stays whole).
fn span(a: Lookup, b: Lookup, max: u32) -> Option<(u32, u32)> {
    let last = max.checked_sub(1)?;
    if let (Lookup::At(x), Lookup::At(y)) = (a, b) {
        return if x >= max && y >= max { None } else { Some((x.min(last), y.min(last))) };
    }
    let a_is_low = rep(a)? <= rep(b)?;
    let (low, high) = if a_is_low { (a, b) } else { (b, a) };
    let low = match low {
        Lookup::At(p) => p,
        Lookup::Deleted { after, .. } => after?,
        Lookup::Unknown => return None,
    };
    let high = match high {
        Lookup::At(p) => p,
        Lookup::Deleted { before, .. } => before?,
        Lookup::Unknown => return None,
    };
    if low > high || low >= max {
        return None;
    }
    let high = high.min(last);
    Some(if a_is_low { (low, high) } else { (high, low) })
}

fn point(l: Lookup, max: u32) -> Option<u32> {
    match l {
        Lookup::At(p) if p < max => Some(p),
        _ => None,
    }
}

fn resolve_kind(t: &Token, sheet: u64, r: &impl Resolver) -> Option<RefKind> {
    let at = |axis: Axis, id: u64| r.line_pos(Some(sheet), axis, id);
    let (a, b) = (&t.first, t.second.as_ref());
    match (a.row, a.col, b) {
        (Some((ri, ra)), Some((ci, ca)), None) => {
            let anchor = Anchor { row: point(at(Axis::Rows, ri), MAX_ROWS)?, col: point(at(Axis::Cols, ci), MAX_COLS)?, row_abs: ra, col_abs: ca };
            Some(RefKind::Cell(anchor))
        }
        (Some((r0, a0)), Some((c0, ca0)), Some(End { row: Some((r1, a1)), col: Some((c1, ca1)), .. })) => {
            let (row0, row1) = span(at(Axis::Rows, r0), at(Axis::Rows, *r1), MAX_ROWS)?;
            let (col0, col1) = span(at(Axis::Cols, c0), at(Axis::Cols, *c1), MAX_COLS)?;
            Some(RefKind::Range(
                Anchor { row: row0, col: col0, row_abs: a0, col_abs: ca0 },
                Anchor { row: row1, col: col1, row_abs: *a1, col_abs: *ca1 },
            ))
        }
        (Some((r0, a0)), None, Some(End { row: Some((r1, a1)), col: None, .. })) => {
            let (lo, hi) = span(at(Axis::Rows, r0), at(Axis::Rows, *r1), MAX_ROWS)?;
            Some(RefKind::Rows(lo, a0, hi, *a1))
        }
        (None, Some((c0, a0)), Some(End { row: None, col: Some((c1, a1)), .. })) => {
            let (lo, hi) = span(at(Axis::Cols, c0), at(Axis::Cols, *c1), MAX_COLS)?;
            Some(RefKind::Cols(lo, a0, hi, *a1))
        }
        _ => None,
    }
}

/// The expression for one token at today's positions: `#REF!` when a deleted line, sheet or
/// unknown id leaves nothing to point at.
fn resolve(t: &Token, host: u64, r: &impl Resolver) -> Expr {
    let named = t.first.sheet.map(|id| (id, r.sheet_name(id)));
    let found = match named {
        None => Some((SheetSel::Current, host)),
        Some((id, Some(name))) => Some((SheetSel::Named(name), id)),
        Some((_, None)) => None,
    };
    let reference = found.and_then(|(sheet, lines)| Some(Expr::Ref(Reference { sheet, kind: resolve_kind(t, lines, r)? })));
    reference.unwrap_or(Expr::Error(CellError::Ref))
}

/// Id-form text → expression at today's positions. A deleted single cell is `#REF!`; a
/// range shrinks inward to the nearest surviving ids, and is `#REF!` when none survive.
pub fn parse_ids(text: &str, host_sheet: u64, r: &impl Resolver) -> Result<Expr, IdError> {
    let m = mask(text)?;
    // The limit is the formula's, counted on the A1 text it stands for: each token costs what
    // the shortest A1 form of it would (`A1`, `A1:B2`), not the placeholder's length.
    let a1_len = text.chars().count() - m.spans.iter().map(|s| s.3).sum::<usize>()
        + m.tokens.iter().map(|t| if t.second.is_some() { 3 } else { 2 }).sum::<usize>();
    if a1_len > MAX_FORMULA_LEN {
        return Err(IdError::Syntax(0));
    }
    let parsed = parse_unlimited(&m.text).map_err(|e| IdError::Syntax(m.original_pos(e.pos)))?;
    Ok(parsed.map(&mut |x| match x {
        Expr::Ref(Reference { sheet: SheetSel::Named(n), kind }) => {
            let index = n.strip_prefix(&m.prefix).and_then(|rest| rest.strip_suffix('_')).and_then(|digits| digits.parse::<usize>().ok());
            match index.and_then(|i| m.tokens.get(i)) {
                Some(token) => resolve(token, host_sheet, r),
                None => Expr::Ref(Reference { sheet: SheetSel::Named(n), kind }),
            }
        }
        other => other,
    }))
}
