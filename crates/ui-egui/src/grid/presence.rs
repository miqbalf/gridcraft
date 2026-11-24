//! Other editors' selections on the sheet, while editing live: for each peer a 2 px outline of its
//! active cell in its colour (thicker while its cell editor is open), its ranges tinted at about
//! 15 %, and a name tag at the top right of the active cell while it moves, for three seconds
//! after, and on hover. A peer on another sheet shows as dots on that sheet's tab
//! ([`paint_tab_dots`]). Each drawn selection is an accessibility node, "Bob's selection, B3:D9 on
//! Sheet1".
//!
//! The colour is the badge's (`craft_ui::presence::colours_for` over the room), so a badge and
//! the outline of the same person always agree.

use egui::{Color32, Painter, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use gridcraft_engine::core::{CellRef, RangeRef};
use gridcraft_engine::model::Sheet;

use super::Geo;
use crate::SheetApp;
use crate::theme;

/// Outline width of a peer's active cell, and while its editor is open.
const OUTLINE: f32 = 2.0;
const OUTLINE_EDITING: f32 = 3.5;
/// Opacity of a range tint.
const TINT: f32 = 0.15;
const DOT: f32 = 3.5;

/// The range a peer has selected, as text: its first range, else its active cell.
#[must_use]
pub fn selection_text(active: CellRef, ranges: &[RangeRef]) -> String {
    ranges.first().map_or_else(|| active.a1(), RangeRef::a1)
}

/// Draws every peer whose selection is on sheet `si`.
pub(crate) fn paint(app: &SheetApp, ui: &egui::Ui, p: &Painter, geo: &Geo, sh: &Sheet, si: usize) {
    let Some(live) = app.live() else { return };
    let lang = app.ui.language;
    let now = ui.input(|i| i.time);
    let hover = ui.input(|i| i.pointer.hover_pos());
    let clip = p.clip_rect();
    for (peer, colour) in live.peers().iter().zip(live.colours()) {
        let Some(pos) = peer.caret.as_ref().filter(|c| c.sheet == si) else { continue };
        for range in &pos.ranges {
            let rect = geo.range_rect(sh, *range);
            for pane in geo.range_panes(*range) {
                p.with_clip_rect(pane.intersect(clip)).rect_filled(rect, 0.0, colour.gamma_multiply(TINT));
            }
        }
        if geo.hidden_under_frozen(sh, pos.active) {
            continue;
        }
        let cell = geo.cell_rect(sh, pos.active);
        let width = if pos.editing { OUTLINE_EDITING } else { OUTLINE };
        p.rect_stroke(cell, 0.0, Stroke::new(width, colour), StrokeKind::Inside);
        let over = hover.is_some_and(|h| geo.mirror_rect(cell).contains(h));
        if over || live.tag_recent(&peer.id, now) {
            paint_tag(p, cell, &peer.display, colour, clip);
        }
        // The selection as a named node for assistive technology.
        let name = lang.tr_fill(
            "{name}'s selection, {range} on {sheet}",
            &[("name", &peer.display), ("range", &selection_text(pos.active, &pos.ranges)), ("sheet", &sh.name)],
        );
        let id = ui.scope_id().with("gridcraft.live.selection").with(peer.id.as_str());
        let response = ui.interact(geo.mirror_rect(cell), id, Sense::hover());
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, &name));
    }
}

/// The name tag: the peer's name on its colour at the top right of its active cell (below the
/// cell when the row above is out of view).
fn paint_tag(p: &Painter, cell: Rect, name: &str, colour: Color32, clip: Rect) {
    let font = theme::ui_font(11.0);
    let ink = if luma(colour) > 150.0 { Color32::BLACK } else { Color32::WHITE };
    let galley = p.layout_no_wrap(name.to_owned(), font, ink);
    let size = galley.size() + vec2(8.0, 3.0);
    let above = Rect::from_min_size(pos2(cell.right() - size.x, cell.top() - size.y), size);
    let rect = if clip.contains_rect(above) { above } else { Rect::from_min_size(pos2(cell.right() - size.x, cell.bottom()), size) };
    p.rect_filled(rect, 2.0, colour);
    p.galley(rect.min + vec2(4.0, 1.5), galley, ink);
}

fn luma(c: Color32) -> f32 {
    0.299 * f32::from(c.r()) + 0.587 * f32::from(c.g()) + 0.114 * f32::from(c.b())
}

/// Dots at the top right of a sheet tab, one per peer working on that sheet.
pub(crate) fn paint_tab_dots(p: &Painter, tab: Rect, colours: &[Color32]) {
    for (n, colour) in colours.iter().enumerate() {
        let at = pos2(tab.right() - 8.0 - n as f32 * (DOT * 2.0 + 2.0), tab.top() + 7.0);
        p.circle_filled(at, DOT, *colour);
    }
}
