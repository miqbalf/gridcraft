//! The one place a session's starting state is made from the base file: both [`GridLive`]
//! (the editor) and [`GridWorkerLive`](crate::GridWorkerLive) (the document service) start here,
//! so the two cannot disagree about the seed, the layout or the random source.

use craft_collab::Replica;
use gridcraft_engine::{DocState, Session, Trust};
use gridcraft_model::Workbook;
use sha2::{Digest, Sha256};

use crate::app::Mapping;
use crate::doc::{GridDoc, GridSeed};
use crate::layout::Layout;
use crate::session::{BaseFile, LiveFail};
use crate::styles::StyleHash;
use crate::volatile::LiveVolatile;

/// Checks the base's hash when one is given, opens it, and says what the room is seeded with.
pub(crate) fn open_base(base: &BaseFile) -> Result<(Workbook, GridSeed), LiveFail> {
    let sha: [u8; 32] = Sha256::digest(base.bytes).into();
    if base.expected_sha256.is_some_and(|want| want != sha) {
        return Err(LiveFail::BaseMismatch);
    }
    let (wb, warnings) = gridcraft_engine::io::open_bytes(base.name, base.bytes).map_err(|e| LiveFail::Open(e.to_string()))?;
    if !warnings.is_empty() {
        log::info!("gridcraft-live: opening the base gave {} warnings", warnings.len());
    }
    let seed = GridSeed { base_sha256: sha, sheets: u32::try_from(wb.sheets.len()).unwrap_or(u32::MAX) };
    Ok((wb, seed))
}

/// Adds `wb` to `session` as a document and builds the mapping over a replica seeded with
/// `seed`: the random source and layout, in step.
pub(crate) fn seat(session: &mut Session, wb: Workbook, seed: &GridSeed, title: String, uid: &str, hash: StyleHash) -> Result<Mapping, LiveFail> {
    session.add_document(DocState::new(wb, None, title, Trust::FromFile));
    let (doc, wb) = session.active().map(|d| (d.uid, d.wb.clone())).ok_or(LiveFail::Open("no workbook".into()))?;
    let replica = GridDoc::seeded(seed).map_err(LiveFail::Live)?;
    let vol = LiveVolatile::new(&replica, &wb);
    let layout = Layout::from_doc(&replica, &wb);
    let layout_rev = vol.update_layout(layout.clone());
    Ok(Mapping::new(doc, wb, layout, layout_rev, uid, hash, vol))
}
