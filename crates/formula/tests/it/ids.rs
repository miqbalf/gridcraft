//! The id form against the engine's own `adjust`: a formula printed as ids before a structural
//! edit and read back after it must be the formula `adjust` gives. The model resolver below is
//! a set of axes with ids, deletions and inserts, as a live layout has them. The cases are
//! seeded (the seed is in every failure message), in the style of the engine's command fuzz.

use std::collections::BTreeMap;

use gridcraft_core::{MAX_COLS, MAX_ROWS, col_to_letters, quote_sheet};
use gridcraft_formula::adjust::{Axis, Edit, adjust};
use gridcraft_formula::ids::{IdError, Lookup, Resolver, parse_ids, print_ids};
use gridcraft_formula::{Expr, SheetSel, parse, print};

// ---------------------------------------------------------------- deterministic randomness

/// SplitMix64, as in the engine's command-sequence fuzz.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
    fn int(&mut self, lo: u32, hi: u32) -> u32 {
        lo + (self.next() % u64::from((hi - lo + 1).max(1))) as u32
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

// ---------------------------------------------------------------- the model layout

/// One axis of one sheet: the ids in order with tombstones for deleted ones, then an endless
/// tail of untouched base ids (id = position + 1 at the start, as space 0 has them).
#[derive(Clone)]
struct Line {
    order: Vec<(u64, bool)>,
    tail: u64,
}

impl Line {
    fn new() -> Self {
        Line { order: Vec::new(), tail: 1 }
    }
    fn alive(&self) -> usize {
        self.order.iter().filter(|(_, alive)| *alive).count()
    }
    fn extend_to(&mut self, n: usize) {
        while self.alive() < n {
            self.order.push((self.tail, true));
            self.tail += 1;
        }
    }
    /// Index in `order` of the live line at `pos`, or the end when `pos` is the first tail line.
    fn index_of(&self, pos: usize) -> usize {
        let mut seen = 0;
        for (i, (_, alive)) in self.order.iter().enumerate() {
            if *alive {
                if seen == pos {
                    return i;
                }
                seen += 1;
            }
        }
        self.order.len()
    }
    fn id_at(&self, pos: u32) -> u64 {
        let alive = self.alive();
        let pos = pos as usize;
        if pos < alive { self.order.iter().filter(|(_, a)| *a).nth(pos).map_or(0, |(id, _)| *id) } else { self.tail + (pos - alive) as u64 }
    }
    fn lookup(&self, id: u64) -> Lookup {
        let alive = self.alive();
        if id >= self.tail && id < FRESH {
            return u32::try_from(alive as u64 + (id - self.tail)).map_or(Lookup::Unknown, Lookup::At);
        }
        let mut before = 0u32;
        for (i, alive) in &self.order {
            if *i == id {
                return if *alive { Lookup::At(before) } else { Lookup::Deleted { before: before.checked_sub(1), after: Some(before) } };
            }
            before += u32::from(*alive);
        }
        Lookup::Unknown
    }
    fn insert(&mut self, at: u32, count: u32, fresh: &mut u64) {
        self.extend_to(at as usize);
        let index = self.index_of(at as usize);
        for _ in 0..count {
            self.order.insert(index, (*fresh, true));
            *fresh += 1;
        }
    }
    fn delete(&mut self, at: u32, count: u32) -> Vec<u64> {
        self.extend_to((at + count) as usize);
        let mut gone = Vec::new();
        for _ in 0..count {
            let index = self.index_of(at as usize);
            if let Some(slot) = self.order.get_mut(index) {
                slot.1 = false;
                gone.push(slot.0);
            }
        }
        gone
    }
    fn restore(&mut self, ids: &[u64]) {
        for slot in &mut self.order {
            if ids.contains(&slot.0) {
                slot.1 = true;
            }
        }
    }
}

struct Model {
    sheets: Vec<(u64, String)>,
    lines: BTreeMap<(u64, bool), Line>,
    fresh: u64,
}

/// Ids the model hands to inserted lines start here, far above the base ids.
const FRESH: u64 = 1 << 40;
const HOST: u64 = 7;
const SHEETS: [(u64, &str); 3] = [(HOST, "S1"), (36, "S2"), (1300, "Q 3")];

impl Model {
    fn new() -> Self {
        let sheets = SHEETS.iter().map(|(id, n)| (*id, n.to_string())).collect();
        let lines = SHEETS.iter().flat_map(|(id, _)| [((*id, false), Line::new()), ((*id, true), Line::new())]).collect();
        Model { sheets, lines, fresh: FRESH }
    }
    fn line_mut(&mut self, sheet: u64, axis: Axis) -> Option<&mut Line> {
        self.lines.get_mut(&(sheet, axis == Axis::Cols))
    }
    fn apply(&mut self, sheet: u64, edit: &Edit) {
        let mut fresh = self.fresh;
        match edit {
            Edit::Insert { axis, at, count } => {
                if let Some(l) = self.line_mut(sheet, *axis) {
                    l.insert(*at, *count, &mut fresh);
                }
            }
            Edit::Delete { axis, at, count } => {
                if let Some(l) = self.line_mut(sheet, *axis) {
                    l.delete(*at, *count);
                }
            }
            _ => {}
        }
        self.fresh = fresh;
    }
    /// A few random edits first, so ids are not just position + 1.
    fn with_history(rng: &mut Rng) -> Self {
        let mut m = Model::new();
        for _ in 0..rng.below(6) {
            let (sheet, _) = SHEETS[rng.below(SHEETS.len())];
            let edit = random_edit(rng, 12);
            m.apply(sheet, &edit);
        }
        m
    }
}

impl Resolver for Model {
    fn sheet_id(&self, name: &str) -> Option<u64> {
        self.sheets.iter().find(|(_, n)| n == name).map(|(id, _)| *id)
    }
    fn sheet_name(&self, id: u64) -> Option<String> {
        self.sheets.iter().find(|(i, _)| *i == id).map(|(_, n)| n.clone())
    }
    fn line_id(&self, sheet: Option<u64>, axis: Axis, pos: u32) -> Option<u64> {
        self.lines.get(&(sheet?, axis == Axis::Cols)).map(|l| l.id_at(pos))
    }
    fn line_pos(&self, sheet: Option<u64>, axis: Axis, id: u64) -> Lookup {
        sheet.and_then(|s| self.lines.get(&(s, axis == Axis::Cols))).map_or(Lookup::Unknown, |l| l.lookup(id))
    }
}

fn random_edit(rng: &mut Rng, span: u32) -> Edit {
    let axis = if rng.chance(50) { Axis::Rows } else { Axis::Cols };
    let (at, count) = (rng.int(0, span + 2), rng.int(1, 4));
    if rng.chance(50) { Edit::Insert { axis, at, count } } else { Edit::Delete { axis, at, count } }
}

// ---------------------------------------------------------------- random formulas

fn pos(rng: &mut Rng, span: u32, max: u32) -> u32 {
    if rng.chance(6) { max - 1 } else { rng.int(0, span) }
}

fn anchor(rng: &mut Rng, row: u32, col: u32) -> String {
    let (ca, ra) = (if rng.chance(30) { "$" } else { "" }, if rng.chance(30) { "$" } else { "" });
    format!("{ca}{}{ra}{}", col_to_letters(col), row + 1)
}

/// Two positions, in order when `ordered` (`adjust` puts a span in order on the edited axis).
fn pair(rng: &mut Rng, span: u32, max: u32, ordered: bool) -> (u32, u32) {
    let (a, b) = (pos(rng, span, max), pos(rng, span, max));
    if ordered && a > b { (b, a) } else { (a, b) }
}

fn reference(rng: &mut Rng, span: u32, ordered: bool) -> String {
    let sheet = if rng.chance(40) { format!("{}!", quote_sheet(SHEETS[rng.below(SHEETS.len())].1)) } else { String::new() };
    let flag = |rng: &mut Rng| if rng.chance(30) { "$" } else { "" };
    let body = match rng.below(4) {
        0 => {
            let (r, c) = (pos(rng, span, MAX_ROWS), pos(rng, span, MAX_COLS));
            anchor(rng, r, c)
        }
        1 => {
            let ((r0, r1), (c0, c1)) = (pair(rng, span, MAX_ROWS, ordered), pair(rng, span, MAX_COLS, ordered));
            format!("{}:{}", anchor(rng, r0, c0), anchor(rng, r1, c1))
        }
        2 => {
            let (a, b) = pair(rng, span, MAX_ROWS, ordered);
            format!("{}{}:{}{}", flag(rng), a + 1, flag(rng), b + 1)
        }
        _ => {
            let (a, b) = pair(rng, span, MAX_COLS, ordered);
            format!("{}{}:{}{}", flag(rng), col_to_letters(a), flag(rng), col_to_letters(b))
        }
    };
    format!("{sheet}{body}")
}

fn formula(rng: &mut Rng, depth: u32, span: u32, ordered: bool) -> String {
    let sub = |rng: &mut Rng| formula(rng, depth + 1, span, ordered);
    match rng.below(if depth >= 2 { 3 } else { 12 }) {
        0..=2 => reference(rng, span, ordered),
        3 => format!("SUM({},{})", sub(rng), sub(rng)),
        4 => format!("{}+{}*2", sub(rng), sub(rng)),
        5 => format!("IF({}>1,\"text @{{r:1,c:1}} and A1\",{})", sub(rng), sub(rng)),
        6 => format!("Total+{}", sub(rng)),
        7 => format!("({})", sub(rng)),
        8 => format!("-{}%", reference(rng, span, ordered)),
        9 => format!("@{}", reference(rng, span, ordered)),
        10 => {
            let (r, c) = (rng.int(0, span), rng.int(0, span));
            format!("{}#", anchor(rng, r, c))
        }
        _ => {
            let mode = [".:", ":.", ".:."][rng.below(3)];
            let ((r0, r1), (c0, c1)) = (pair(rng, span, MAX_ROWS, true), pair(rng, span, MAX_COLS, true));
            format!("SUM({}{mode}{})", anchor(rng, r0, c0), anchor(rng, r1, c1))
        }
    }
}

fn parsed(text: &str, seed: u64) -> Expr {
    parse(text).unwrap_or_else(|e| panic!("seed {seed}: generated `{text}` does not parse: {e:?}"))
}

fn converts(e: &Expr) -> bool {
    e.references().iter().any(|r| !matches!(r.sheet, SheetSel::Span(..)))
}

// ---------------------------------------------------------------- tests

#[test]
fn a1_to_ids_and_back_is_identity() {
    for seed in 0..500 {
        let mut rng = Rng(seed);
        let model = Model::with_history(&mut rng);
        let text = formula(&mut rng, 0, 40, false);
        let e = parsed(&text, seed);
        let ids = print_ids(&e, HOST, &model).unwrap_or_else(|err| panic!("seed {seed}: `{text}`: {err:?}"));
        if converts(&e) {
            assert_ne!(ids, print(&e), "seed {seed}: `{text}` printed no ids");
        }
        let back = parse_ids(&ids, HOST, &model).unwrap_or_else(|err| panic!("seed {seed}: `{ids}`: {err:?}"));
        assert_eq!(back, e, "seed {seed}: `{text}` through `{ids}` came back as `{}`", print(&back));
    }
}

/// Prints `text` as ids, applies `edit` to the layout and to `adjust`, and compares them.
fn edit_matches_adjust(seed: u64, make: impl Fn(&mut Rng) -> Edit) {
    for seed in seed..seed + 800 {
        let mut rng = Rng(seed);
        let mut model = Model::with_history(&mut rng);
        let text = formula(&mut rng, 0, 14, true);
        let e = parsed(&text, seed);
        let ids = print_ids(&e, HOST, &model).unwrap_or_else(|err| panic!("seed {seed}: `{text}`: {err:?}"));
        let edit = make(&mut rng);
        let (target, name) = SHEETS[rng.below(SHEETS.len())];
        model.apply(target, &edit);
        let expected = adjust(e, "S1", name, &edit);
        let got = parse_ids(&ids, HOST, &model).unwrap_or_else(|err| panic!("seed {seed}: `{ids}`: {err:?}"));
        assert_eq!(
            got,
            expected,
            "seed {seed}: `{text}` (`{ids}`) after {edit:?} on {name}: ids give `{}`, adjust gives `{}`",
            print(&got),
            print(&expected)
        );
    }
}

#[test]
fn printed_after_insert_equals_adjust() {
    edit_matches_adjust(1_000, |rng| {
        let axis = if rng.chance(50) { Axis::Rows } else { Axis::Cols };
        Edit::Insert { axis, at: rng.int(0, 16), count: rng.int(1, 4) }
    });
}

#[test]
fn printed_after_delete_equals_adjust() {
    edit_matches_adjust(5_000, |rng| {
        let axis = if rng.chance(50) { Axis::Rows } else { Axis::Cols };
        Edit::Delete { axis, at: rng.int(0, 16), count: rng.int(1, 6) }
    });
}

#[test]
fn restored_ids_bring_the_reference_back() {
    for seed in 0..200 {
        let mut rng = Rng(seed);
        let mut model = Model::with_history(&mut rng);
        let (row, col) = (rng.int(0, 12), rng.int(0, 12));
        let (r1, c1) = (row + rng.int(0, 3), col + rng.int(0, 3));
        let text = format!("SUM({}, {}:{}, {}:{})", anchor(&mut rng, row, col), anchor(&mut rng, row, col), anchor(&mut rng, r1, c1), r1 + 1, r1 + 1);
        let e = parsed(&text, seed);
        let ids = print_ids(&e, HOST, &model).unwrap_or_else(|err| panic!("seed {seed}: {err:?}"));
        // Delete the rows holding the whole formula: nothing it names survives.
        let (first, last) = (row.min(r1), row.max(r1));
        let rows = model.line_mut(HOST, Axis::Rows).map(|l| l.delete(first, last - first + 1)).unwrap_or_default();
        let gone = parse_ids(&ids, HOST, &model).unwrap_or_else(|err| panic!("seed {seed}: {err:?}"));
        assert_ne!(gone, e, "seed {seed}: `{text}` survived its rows being deleted");
        assert!(print(&gone).contains("#REF!"), "seed {seed}: `{}`", print(&gone));
        if let Some(l) = model.line_mut(HOST, Axis::Rows) {
            l.restore(&rows);
        }
        let back = parse_ids(&ids, HOST, &model).unwrap_or_else(|err| panic!("seed {seed}: {err:?}"));
        assert_eq!(back, e, "seed {seed}: `{text}` did not come back: `{}`", print(&back));
        assert_eq!(print(&back), print(&e));
    }
}

#[test]
fn names_structured_refs_and_3d_stay_text() {
    let keep = ["Total", "Table1[[#Headers],[Col1]:[Col3]]", "Table1[@Col]", "'S1:S3'!A1", "INDIRECT(\"B2\")", "\"@{r:1,c:1}\"", "{1,2;3,4}"];
    let text = keep.join("+").replace("'S1:S3'!A1", "S1:S3!A1");
    let e = parsed(&text, 0);
    for seed in 0..100 {
        let mut rng = Rng(seed);
        let mut model = Model::with_history(&mut rng);
        let ids = print_ids(&e, HOST, &model).unwrap_or_else(|err| panic!("{err:?}"));
        for token in keep {
            assert!(ids.contains(token), "`{token}` changed in `{ids}`");
        }
        let edit = random_edit(&mut rng, 4);
        let (target, name) = SHEETS[rng.below(SHEETS.len())];
        model.apply(target, &edit);
        let by_ids = parse_ids(&ids, HOST, &model).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(by_ids, e, "ids path changed the formula after {edit:?}");
        assert_eq!(adjust(e.clone(), "S1", name, &edit), e, "adjust path changed the formula after {edit:?}");
    }
    // An array constant after `@` is not an id token.
    let at_array = parse_ids("@{1,2}", HOST, &Model::new()).unwrap_or_else(|err| panic!("{err:?}"));
    assert_eq!(print(&at_array), "@{1,2}");
}

#[test]
fn sheet_ids_resolve_and_a_gone_sheet_is_ref() {
    let model = Model::new();
    let e = parsed("S2!B3+'Q 3'!$C$4+A1", 0);
    let ids = print_ids(&e, HOST, &model).unwrap_or_else(|err| panic!("{err:?}"));
    assert!(ids.contains("s:10"), "S2 has id 36 = `10` in base 36: {ids}");
    // A rename keeps the id: the same text reads back under the new name.
    let mut renamed = Model::new();
    renamed.sheets = vec![(HOST, "S1".into()), (36, "Costs".into()), (1300, "Q 3".into())];
    let back = parse_ids(&ids, HOST, &renamed).unwrap_or_else(|err| panic!("{err:?}"));
    assert_eq!(print(&back), "Costs!B3+'Q 3'!$C$4+A1");
    // A sheet without an id is an error on the way out; a sheet that is gone is `#REF!` on the way in.
    assert!(matches!(print_ids(&parsed("Nowhere!A1", 0), HOST, &model), Err(IdError::UnknownSheet(n)) if n == "Nowhere"));
    let mut gone = Model::new();
    gone.sheets.retain(|(id, _)| *id != 36);
    let back = parse_ids(&ids, HOST, &gone).unwrap_or_else(|err| panic!("{err:?}"));
    assert_eq!(print(&back), "#REF!+'Q 3'!$C$4+A1");
}

#[test]
fn hostile_id_text_is_an_error_not_a_panic() {
    let model = Model::new();
    // Truncated inside a token: always an error.
    for seed in 0..300 {
        let mut rng = Rng(seed);
        let text = formula(&mut rng, 0, 40, false);
        let ids = print_ids(&parsed(&text, seed), HOST, &model).unwrap_or_else(|err| panic!("seed {seed}: {err:?}"));
        let Some(start) = ids.find("@{") else { continue };
        let end = start + ids[start..].find('}').unwrap_or(0);
        for cut in start + 1..=end {
            let truncated = &ids[..cut];
            assert!(parse_ids(truncated, HOST, &model).is_err(), "seed {seed}: `{truncated}` parsed");
        }
    }
    // Random bytes, and random mixes of the id form's own characters: no panic, offsets inside the text.
    let alphabet = b"@{}:,$rcs0123456789azZ\".[]'#!+()-_ ";
    for seed in 0..3_000 {
        let mut rng = Rng(seed);
        let len = rng.below(48);
        let bytes: Vec<u8> = (0..len).map(|_| if rng.chance(15) { rng.next() as u8 } else { alphabet[rng.below(alphabet.len())] }).collect();
        let text = String::from_utf8_lossy(&bytes);
        if let Err(IdError::Syntax(at)) = parse_ids(&text, HOST, &model) {
            assert!(at <= text.len(), "seed {seed}: offset {at} outside `{text}`");
        }
    }
    for bad in [
        "@{r:zzzzzzzzzzzzzzzzzz,c:1}",
        "@{r:1,c:1",
        "@{r:1,r:2}",
        "@{$r}",
        "@{r:1,c:1}:@{r:2}",
        "@{r:1}",
        "@{r:1,c:1}:@{s:2,r:1,c:1}",
        "A@{r:1,c:1}",
        "@{r:1,c:1}(2)",
        "@{r:1,c:1}5",
    ] {
        assert!(parse_ids(bad, HOST, &model).is_err(), "`{bad}` parsed");
    }
}
