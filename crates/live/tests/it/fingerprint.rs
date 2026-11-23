//! A workbook as text, for comparing peers: shared by the in-memory and the relay tests.

use gridcraft_core::CellRef;
use gridcraft_model::{Cell, Workbook};

/// Values, formulas as text, style content and line info of every sheet.
pub fn finger(wb: &Workbook) -> String {
    let mut out = String::new();
    for (i, s) in wb.sheets.iter().enumerate() {
        out.push_str(&format!("sheet {i} w={} h={}\n", s.default_col_width, s.default_row_height));
        let mut cells: Vec<(CellRef, &Cell)> = s.cells.iter().collect();
        cells.sort_by_key(|(c, _)| *c);
        for (c, cell) in cells {
            let f = cell.formula.as_ref().map(|f| f.text.clone());
            let style = serde_json::to_string(wb.styles.get(cell.style)).unwrap();
            out.push_str(&format!("{} {:?} {:?} {}\n", c.a1(), cell.value, f, style));
        }
        // A line with nothing set and no entry are the same line.
        let set = |m: &std::collections::BTreeMap<u32, gridcraft_model::LineInfo>| {
            // Style ids differ between peers (interning order): compare the style itself.
            m.iter()
                .filter(|(_, l)| **l != Default::default())
                .map(|(k, l)| {
                    let st = l.style.map(|id| serde_json::to_string(wb.styles.get(id)).unwrap());
                    (*k, gridcraft_model::LineInfo { style: None, ..*l }, st)
                })
                .collect::<Vec<_>>()
        };
        out.push_str(&format!("rows {:?}\ncols {:?}\n", set(&s.rows), set(&s.cols)));
    }
    out
}

/// The first lines where two fingerprints differ, for a failure message.
pub fn diff(a: &str, b: &str) -> String {
    let (la, lb): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    let n = la.len().max(lb.len());
    let mut out = Vec::new();
    for i in 0..n {
        let (x, y) = (la.get(i).copied().unwrap_or("<none>"), lb.get(i).copied().unwrap_or("<none>"));
        if x != y {
            // Long lines (a sheet's rows) differ deep inside: show them from a little before the
            // first character that differs.
            let at = x.chars().zip(y.chars()).take_while(|(p, q)| p == q).count().saturating_sub(120);
            let cut = |s: &str| s.chars().skip(at).take(700).collect::<String>();
            out.push(format!("line {i} (from char {at}): {} | {}", cut(x), cut(y)));
            if out.len() >= 4 {
                break;
            }
        }
    }
    out.join("\n")
}
