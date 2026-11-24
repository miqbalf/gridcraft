//! An engine report into writes: what a local command changed, as the overlay's keys.
//!
//! Only what differs between the report's `before` and `after` is written, part by part (a cell's
//! content, its format, a row's height, its hidden flag), so a peer's concurrent change to a
//! different part of the same cell or line is kept. A part that ends up equal to the base file's
//! removes its key; a part cleared against a base that holds something writes `null`.

use std::collections::{BTreeSet, HashMap};

use gridcraft_core::CellRef;
use gridcraft_engine::EditReport;
use gridcraft_model::{StyleId, Workbook};
use serde_json::Value as Json;

use crate::axis::AxisKind;
use crate::cells::{Home, is_random, same_content, to_content};
use crate::doc::{Entry, GridDoc};
use crate::gate::NOT_YET_TEXT;
use crate::ids::{Id, cell_key};
use crate::layout::Layout;
use crate::remote::{base_cell, base_line, filter_hidden, line_at, line_key, size_part};
use crate::split::{CellWrite, Plan, Put};
use crate::styles::{self, StyleHash};
use craft_live::LocalError;

/// What the mapping needs to plan one batch.
pub(crate) struct Planner<'a> {
    pub base: &'a Workbook,
    pub layout: &'a Layout,
    pub doc: &'a GridDoc,
    pub uid: &'a str,
    pub space: u32,
    pub hash: StyleHash,
    /// Canonical style JSON to the key it is written under.
    pub keys: &'a mut HashMap<String, String>,
    /// How many keys this peer has minted (collisions).
    pub minted: &'a mut u32,
    /// Output: a hidden flag differs from what the room will show; rebuild the engine.
    pub resync: bool,
    /// Output: a written formula calls a random function.
    pub random: bool,
    /// The report's `before` with its structural edits replayed (set for an ordered edit).
    pub before: Option<&'a Workbook>,
    /// The lines the edit inserted (sheet index, axis, position): blank in `before`, so what
    /// they took from the line before them is written.
    pub inserted: &'a [(usize, AxisKind, u32)],
}

fn refused<T>() -> Result<T, LocalError> {
    Err(LocalError::Refused(NOT_YET_TEXT.to_owned()))
}

fn number(v: f64) -> Json {
    serde_json::Number::from_f64(v).map_or(Json::Null, Json::Number)
}

impl Planner<'_> {
    /// Adds one report's writes to `plan`. Anything the report holds that is not shared (and
    /// every structural edit) refuses the whole batch.
    pub(crate) fn report(&mut self, r: &EditReport, plan: &mut Plan) -> Result<(), LocalError> {
        if !r.unshared.is_empty() {
            return refused();
        }
        for (sheet, at) in &r.cells {
            self.cell(r, *sheet, *at, plan)?;
        }
        let mut lines: BTreeSet<(usize, u8, u32)> = r.lines.iter().map(|(s, a, i)| (*s, u8::from(*a == AxisKind::Cols), *i)).collect();
        // Rows the engine fitted to their content by itself are not reported as edits, but peers
        // do not fit rows, so the fitted height travels.
        lines.extend(r.cells.iter().map(|(s, c)| (*s, 0, c.row)));
        // A row can be fitted again by a command that wrote it without changing a cell (a fill
        // of what was there): its height changed all the same, and travels too.
        lines.extend(r.fitted.iter().map(|(s, row)| (*s, 0, *row)));
        lines.extend(self.inserted.iter().map(|(s, a, p)| (*s, u8::from(*a == AxisKind::Cols), *p)));
        for (sheet, cols, idx) in lines {
            let axis = if cols == 1 { AxisKind::Cols } else { AxisKind::Rows };
            self.line(r, sheet, axis, idx, plan)?;
        }
        for i in &r.sheet_props {
            self.props(r, *i, plan)?;
        }
        Ok(())
    }

    fn before_wb<'r>(&self, r: &'r EditReport) -> &'r Workbook
    where
        Self: 'r,
    {
        self.before.unwrap_or(&*r.before)
    }

    fn cell(&mut self, r: &EditReport, i: usize, at: CellRef, plan: &mut Plan) -> Result<(), LocalError> {
        let (Some((row, col)), Some(sheet)) = (self.layout.slot_at(i, at), self.layout.sheets.get(i)) else { return refused() };
        let (Some(before), Some(after)) = (self.before_wb(r).sheet(i), r.after.sheet(i)) else { return refused() };
        let (cb, ca) = (before.cell(at), after.cell(at));
        let base = base_cell(self.base, i, row, col);
        let same_place = row.base_index() == Some(at.row) && col.base_index() == Some(at.col);
        let content = if same_content(cb, ca) {
            Put::Keep
        } else {
            match ca.filter(|c| c.has_content()) {
                None if base.is_none_or(|b| !b.has_content()) => Put::Reset,
                None => Put::Clear,
                Some(c) if same_place && same_content(base, Some(c)) => Put::Reset,
                Some(c) => {
                    let home = Home { layout: self.layout, sheet: sheet.id.0 };
                    let mut content = to_content(c, &home, self.uid);
                    if let Some(x) = content.as_mut() {
                        x.p = Some(match self.doc.cell(sheet.id, &cell_key(row, col)) {
                            Entry::Set(seen) => seen.fingerprint(),
                            _ => "0".to_owned(),
                        });
                    }
                    if c.formula.as_ref().is_some_and(|f| is_random(&f.text)) {
                        self.random = true;
                    }
                    content.map_or(Put::Clear, Put::Set)
                }
            }
        };
        let (mut fmt, mut style) = (Put::Keep, None);
        let json = |wb: &Workbook, id: StyleId| styles::canonical(&styles::to_json(wb.styles.get(id)));
        let (sb, sa) = (json(self.before_wb(r), cb.map_or(StyleId::DEFAULT, |c| c.style)), json(&r.after, ca.map_or(StyleId::DEFAULT, |c| c.style)));
        if sb != sa {
            let dflt = json(&r.after, StyleId::DEFAULT);
            let base_style = base.map_or_else(|| dflt.clone(), |b| json(self.base, b.style));
            if sa == base_style {
                fmt = Put::Reset;
            } else if sa == dflt {
                fmt = Put::Clear;
            } else {
                let value = styles::to_json(r.after.styles.get(ca.map_or(StyleId::DEFAULT, |c| c.style)));
                let key = self.style_key(&sa);
                fmt = Put::Set(key.clone());
                style = Some((key, value));
            }
        }
        plan.cell(CellWrite { sheet: sheet.id, key: cell_key(row, col), content, fmt, style });
        Ok(())
    }

    /// The key a style is written under: its hash, or a minted key when another style holds it.
    fn style_key(&mut self, canonical: &str) -> String {
        if let Some(k) = self.keys.get(canonical) {
            return k.clone();
        }
        let hashed = styles::key_of(canonical, self.hash);
        let taken = self.doc.style(&hashed).is_some_and(|v| styles::canonical(&v) != canonical);
        let key = if taken {
            *self.minted += 1;
            styles::minted_key(Id::minted(self.space, *self.minted))
        } else {
            hashed
        };
        self.keys.insert(canonical.to_owned(), key.clone());
        key
    }

    fn line(&mut self, r: &EditReport, i: usize, axis: AxisKind, idx: u32, plan: &mut Plan) -> Result<(), LocalError> {
        let (Some(layout), Some(before), Some(after)) = (self.layout.sheets.get(i), self.before_wb(r).sheet(i), r.after.sheet(i)) else {
            return refused();
        };
        let Some(id) = layout.axis(axis).id_at(idx) else { return refused() };
        let (b, a) = (line_at(before, axis, idx), line_at(after, axis, idx));
        if b == a {
            return Ok(());
        }
        // Outline levels and collapsed groups are ordered edits, not shared in M1.
        if (b.outline, b.collapsed) != (a.outline, a.collapsed) {
            return refused();
        }
        let base = base_line(self.base, i, axis, id);
        let sheet = layout.id;
        if b.style != a.style {
            self.line_style(r, a.style, base.style, (sheet, axis, id), plan);
        }
        if b.size != a.size {
            let put = match a.size {
                s if s == base.size => Put::Reset,
                None => Put::Clear,
                Some(v) => Put::Set(number(f64::from(v))),
            };
            plan.line(sheet, axis, line_key(id, size_part(axis)), put);
        }
        if axis == AxisKind::Rows && b.custom != a.custom {
            let put = if a.custom == base.custom { Put::Reset } else { Put::Set(Json::Bool(a.custom)) };
            plan.line(sheet, axis, line_key(id, "hc"), put);
        }
        if b.hidden != a.hidden {
            let fh = filter_hidden(self.base, i, axis, id);
            let manual_base = base.hidden && !fh;
            let put = if a.hidden == manual_base { Put::Reset } else { Put::Set(Json::Bool(a.hidden)) };
            plan.line(sheet, axis, line_key(id, "hide"), put);
            // The room shows `manual or filter-hidden`: a filter-hidden row stays hidden.
            if fh && !a.hidden {
                self.resync = true;
            }
        }
        Ok(())
    }

    /// A whole-line format: written as `<id>/st`, the key of a style in the `styles` map, like a
    /// cell's format.
    fn line_style(&mut self, r: &EditReport, after: Option<StyleId>, base: Option<StyleId>, at: (Id, AxisKind, Id), plan: &mut Plan) {
        let (sheet, axis, id) = at;
        let json = |wb: &Workbook, s: Option<StyleId>| styles::canonical(&styles::to_json(wb.styles.get(s.unwrap_or(StyleId::DEFAULT))));
        let sa = json(&r.after, after);
        let key = line_key(id, "st");
        // A line with a style that looks like the default is still not a line with no style: a
        // row's style wins over a column's. So the document keeps which of the two it is.
        let put = if after.is_some() == base.is_some() && sa == json(self.base, base) {
            Put::Reset
        } else if after.is_none() {
            Put::Clear
        } else {
            let k = self.style_key(&sa);
            plan.style(k.clone(), styles::to_json(r.after.styles.get(after.unwrap_or(StyleId::DEFAULT))));
            Put::Set(Json::String(k))
        };
        plan.line(sheet, axis, key, put);
    }

    fn props(&mut self, r: &EditReport, i: usize, plan: &mut Plan) -> Result<(), LocalError> {
        let (Some(layout), Some(before), Some(after)) = (self.layout.sheets.get(i), self.before_wb(r).sheet(i), r.after.sheet(i)) else {
            return refused();
        };
        if before.default_col_width != after.default_col_width {
            plan.prop(layout.id, "defw", number(f64::from(after.default_col_width)));
        }
        if before.default_row_height != after.default_row_height {
            plan.prop(layout.id, "defh", number(f64::from(after.default_row_height)));
        }
        Ok(())
    }
}

/// Whether the workbook holds a formula that calls a random function.
pub(crate) fn has_random(wb: &Workbook) -> bool {
    wb.sheets.iter().any(|s| s.cells.iter().any(|(_, c)| c.formula.as_ref().is_some_and(|f| is_random(&f.text))))
}
