//! The queue entry of an ordered edit: the command and the lines it names, held as ids so they
//! can be found again after other editors' edits, and the hub wrapper that tells a rejoin.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use craft_collab::{ClientMsg, Hub, ServerMsg};
use gridcraft_core::RangeRef;
use gridcraft_engine::Session;
use gridcraft_formula::ids::Lookup;
use serde_json::{Value as Json, json};

use crate::axis::AxisKind;
use crate::ids::Id;
use crate::layout::Layout;
use crate::ops::{LINES_MAX, TOO_MANY_TEXT};

/// The relay scope every ordered edit takes.
pub const SCOPE: &str = "grid/struct";

/// Told when an ordered edit is asked for while another one of this window is still going.
pub const BUSY_TEXT: &str = "Another change to rows or columns is still going. Try again in a moment.";
/// Told when every line a delete named was deleted by someone else first.
pub const GONE_TEXT: &str = "Those rows or columns were already deleted.";
/// Told when the lock was lost while an ordered edit that had run was being shared. The edit is
/// kept and sent again when the link is back, but another editor may have changed the structure
/// at the same time, so the result may differ from what was meant.
pub const LOCK_LOST_TEXT: &str = "The connection dropped while your change to rows or columns was being shared. It is kept and sent again, but another editor may have changed the sheet at the same time. Check the sheet before you repeat it.";
/// Told when the lock was lost before an ordered edit that waited for an open undo group ran.
pub const LOCK_LOST_UNRUN_TEXT: &str = "The connection dropped before your change to rows or columns was made. Nothing was changed. Try again.";

/// A line on one sheet's axis, by id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Slot {
    pub sheet: Id,
    pub axis: AxisKind,
    pub line: Id,
}

/// An ordered edit waiting for the lock.
#[derive(Clone, Debug)]
pub(crate) struct Pending {
    /// The command id (`home.insertRows` and its kin); empty for an undelete.
    pub command: String,
    /// The lines the command names: for an insert the line the new ones go before, for a delete
    /// every line to delete.
    pub anchors: Vec<Slot>,
    /// How many lines an insert adds.
    pub count: u32,
    /// An undelete: the sheet, the axis and the ids to bring back.
    pub undelete: Option<(Id, AxisKind, Vec<Id>)>,
    /// The undo or redo this edit runs, if it is one.
    pub undo: Option<crate::undo::Replay>,
}

fn axis_of(command: &str) -> Option<(AxisKind, bool)> {
    match command {
        "home.insertRows" => Some((AxisKind::Rows, true)),
        "home.insertColumns" => Some((AxisKind::Cols, true)),
        "home.deleteRows" => Some((AxisKind::Rows, false)),
        "home.deleteColumns" => Some((AxisKind::Cols, false)),
        _ => None,
    }
}

fn parse_span(text: &str) -> Option<RangeRef> {
    let t = text.trim();
    RangeRef::parse(t).or_else(|| if t.contains(':') { None } else { RangeRef::parse(&format!("{t}:{t}")) })
}

fn span_of(r: RangeRef, axis: AxisKind) -> (u32, u32) {
    let (a, b) = if axis == AxisKind::Rows { (r.start.row, r.end.row) } else { (r.start.col, r.end.col) };
    (a.min(b), a.max(b))
}

/// Reads what the command names (its parameters, else the selection) into a [`Pending`].
pub(crate) fn request(session: &Session, layout: &Layout, command: &str, params: &Json) -> Result<Pending, String> {
    let (axis, insert) = axis_of(command).ok_or_else(|| "not an ordered command".to_owned())?;
    let doc = session.active().ok_or_else(|| "no workbook".to_owned())?;
    let sheet = match params.get("sheet") {
        Some(v) if v.is_u64() => v.as_u64().and_then(|i| usize::try_from(i).ok()).filter(|i| *i < doc.wb.sheets.len()),
        Some(v) => v.as_str().and_then(|n| doc.wb.sheet_index(n)),
        None => Some(doc.wb.active_sheet()),
    }
    .ok_or_else(|| "no such sheet".to_owned())?;
    let sl = layout.sheets.get(sheet).ok_or_else(|| "no such sheet".to_owned())?;
    let key = if axis == AxisKind::Rows { "rows" } else { "cols" };
    let text = params.get(key).or_else(|| params.get("range")).and_then(Json::as_str);
    let count_param = params.get("count").and_then(Json::as_u64).and_then(|c| u32::try_from(c).ok());
    let spans: Vec<(u32, u32)> = match text {
        Some(t) if !insert => {
            t.split(',').map(|p| parse_span(p).map(|r| span_of(r, axis)).ok_or_else(|| format!("not a range: {p}"))).collect::<Result<_, _>>()?
        }
        Some(t) => vec![parse_span(t).map(|r| span_of(r, axis)).ok_or_else(|| format!("not a range: {t}"))?],
        None if insert => vec![span_of(doc.selection.current(), axis)],
        None => doc.selection.ranges.iter().map(|r| span_of(*r, axis)).collect(),
    };
    let make = |line: Id| Slot { sheet: sl.id, axis, line };
    if insert {
        let (a, b) = spans.first().copied().ok_or_else(|| "no rows or columns named".to_owned())?;
        let count = count_param.unwrap_or(b - a + 1).max(1);
        if count as usize > LINES_MAX {
            return Err(TOO_MANY_TEXT.to_owned());
        }
        let line = sl.axis(axis).id_at(a).ok_or_else(|| "the change lies outside the worksheet".to_owned())?;
        return Ok(Pending { command: command.to_owned(), anchors: vec![make(line)], count, undelete: None, undo: None });
    }
    let spans: Vec<(u32, u32)> = match (count_param, spans.first()) {
        (Some(c), Some((a, _))) => vec![(*a, a.saturating_add(c.max(1) - 1))],
        _ => spans,
    };
    let mut anchors = Vec::new();
    for (a, b) in spans {
        if anchors.len().saturating_add((b - a) as usize) >= LINES_MAX {
            return Err(TOO_MANY_TEXT.to_owned());
        }
        anchors.extend((a..=b).filter_map(|p| sl.axis(axis).id_at(p)).map(make));
    }
    Ok(Pending { command: command.to_owned(), anchors, count: 0, undelete: None, undo: None })
}

fn name_of(axis: AxisKind, a: u32, b: u32) -> String {
    if axis == AxisKind::Rows { format!("{}:{}", a + 1, b + 1) } else { RangeRef::cols(a, b).a1() }
}

/// The command's parameters for the lines where its anchors are now. `Err` is the text for the
/// user (nothing is left to do).
pub(crate) fn resolve(p: &Pending, layout: &Layout) -> Result<Json, String> {
    let first = p.anchors.first().ok_or_else(|| GONE_TEXT.to_owned())?;
    let i = layout.sheet_index(first.sheet).ok_or_else(|| GONE_TEXT.to_owned())?;
    let sl = layout.sheets.get(i).ok_or_else(|| GONE_TEXT.to_owned())?;
    let axis = first.axis;
    let key = if axis == AxisKind::Rows { "rows" } else { "cols" };
    if p.command.ends_with("insertRows") || p.command.ends_with("insertColumns") {
        let at = match sl.axis(axis).pos_of(first.line) {
            Lookup::At(x) => x,
            Lookup::Deleted { after: Some(x), .. } => x,
            Lookup::Deleted { after: None, before: Some(x) } => x + 1,
            _ => return Err(GONE_TEXT.to_owned()),
        };
        let last = at.saturating_add(p.count.max(1) - 1);
        return Ok(json!({ key: name_of(axis, at, last), "sheet": i }));
    }
    let mut positions: Vec<u32> = p
        .anchors
        .iter()
        .filter_map(|s| match sl.axis(axis).pos_of(s.line) {
            Lookup::At(x) => Some(x),
            _ => None,
        })
        .collect();
    positions.sort_unstable();
    positions.dedup();
    if positions.is_empty() {
        return Err(GONE_TEXT.to_owned());
    }
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for x in positions {
        match runs.last_mut() {
            Some(r) if r.1 + 1 == x => r.1 = x,
            _ => runs.push((x, x)),
        }
    }
    let text = runs.iter().map(|(a, b)| name_of(axis, *a, *b)).collect::<Vec<_>>().join(",");
    Ok(json!({ key: text, "sheet": i }))
}

/// A hub that counts the relay's join answers, so a rejoin (which releases this peer's locks)
/// is told. Everything else passes through.
pub struct Tracked<H> {
    inner: H,
    joins: Arc<AtomicU64>,
}

impl<H> Tracked<H> {
    pub(crate) fn new(inner: H) -> (Self, Arc<AtomicU64>) {
        let joins = Arc::new(AtomicU64::new(0));
        (Tracked { inner, joins: joins.clone() }, joins)
    }

    /// The host's hub.
    pub fn inner(&self) -> &H {
        &self.inner
    }

    /// The host's hub, to steer its transport.
    pub fn inner_mut(&mut self) -> &mut H {
        &mut self.inner
    }
}

impl<H: Hub> Hub for Tracked<H> {
    fn send(&mut self, msg: ClientMsg) -> craft_collab::Result<()> {
        self.inner.send(msg)
    }

    fn poll(&mut self) -> Vec<ServerMsg> {
        let msgs = self.inner.poll();
        let n = msgs.iter().filter(|m| matches!(m, ServerMsg::Joined { .. })).count();
        if n > 0 {
            self.joins.fetch_add(n as u64, Ordering::Relaxed);
        }
        msgs
    }

    fn connected(&self) -> bool {
        self.inner.connected()
    }

    fn set_token(&mut self, token: String) {
        self.inner.set_token(token);
    }
}

/// Sends what the lock's queue asks for, best effort: a failed send is the same as a lost
/// message, which the queue's wait and the relay's lease already cover. The one place the
/// lock's messages leave this crate (the fencing token of a later relay joins here).
pub(crate) fn send_actions<H: Hub>(core: &mut craft_live::LiveEditing<Tracked<H>, crate::kind::Grid>, actions: &[craft_live::ordered::Action]) {
    use craft_live::ordered::Action;
    for a in actions {
        let sent = match a {
            Action::SendLock => core.lock(SCOPE),
            Action::SendUnlock => core.unlock(SCOPE),
        };
        if let Err(e) = sent {
            log::debug!("gridcraft-live: the structure lock message was not sent: {e}");
        }
    }
}
