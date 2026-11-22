//! [`GridWorkerLive`]: the document service's side of a room. It makes no edits of its own, so
//! it keeps the room's workbook up to date from the other editors and needs neither the
//! structure lock nor the join counter of [`GridLive`](crate::GridLive); its link is the plain
//! `LiveEditing<H, Grid>`, which is the type `craft-live-worker` wants.

use std::sync::Arc;

use craft_collab::{Hub, Replica};
use craft_live::{Access, LiveEditing};
use gridcraft_engine::{DocState, Session, Trust};
use gridcraft_model::{Sheet, Workbook};
use sha2::{Digest, Sha256};

use crate::app::{GridApp, Mapping};
use crate::doc::GridSeed;
use crate::kind::Grid;
use crate::layout::Layout;
use crate::session::{BaseFile, LiveFail, Peer};
use crate::styles::fnv1a64;
use crate::volatile::LiveVolatile;

/// The base workbook with the room's changes applied as they arrive, over hub `H` itself.
pub struct GridWorkerLive<H: Hub> {
    core: LiveEditing<H, Grid>,
    map: Mapping,
    session: Session,
}

impl<H: Hub> GridWorkerLive<H> {
    /// Opens `base`, checks its hash when one is given, and joins `room` as `uid`.
    ///
    /// # Errors
    /// [`LiveFail`].
    pub fn start(hub: H, room: &str, base: &BaseFile, access: Access, uid: &str) -> Result<Self, LiveFail> {
        let sha: [u8; 32] = Sha256::digest(base.bytes).into();
        if base.expected_sha256.is_some_and(|want| want != sha) {
            return Err(LiveFail::BaseMismatch);
        }
        let (wb, _warnings) = gridcraft_engine::io::open_bytes(base.name, base.bytes).map_err(|e| LiveFail::Open(e.to_string()))?;
        let seed = GridSeed { base_sha256: sha, sheets: u32::try_from(wb.sheets.len()).unwrap_or(u32::MAX) };
        let core = LiveEditing::<H, Grid>::start(hub, room, &seed, access).map_err(LiveFail::Live)?;
        let mut session = Session::new();
        session.add_document(DocState::new(wb, None, "live".to_owned(), Trust::FromFile));
        let (doc, wb) = session.active().map(|d| (d.uid, d.wb.clone())).ok_or_else(|| LiveFail::Open("no workbook".into()))?;
        let replica = crate::doc::GridDoc::seeded(&seed).map_err(LiveFail::Live)?;
        let vol = LiveVolatile::new(&replica, &wb);
        let layout = Layout::from_doc(&replica, &wb);
        let layout_rev = vol.update_layout(layout.clone());
        let source: Arc<dyn gridcraft_calc::VolatileSource> = vol.clone();
        session.set_volatile_source(doc, Some(source)).map_err(|e| LiveFail::Open(e.to_string()))?;
        let map = Mapping::new(doc, wb, layout, layout_rev, uid, fnv1a64, vol);
        Ok(GridWorkerLive { core, map, session })
    }

    /// One turn of the link: takes in the other editors' changes. The editors in the room.
    pub fn pump(&mut self) -> Vec<Peer> {
        let mut app = GridApp { session: &mut self.session, map: &mut self.map };
        self.core.pump(&mut app)
    }

    #[must_use]
    pub fn core(&self) -> &LiveEditing<H, Grid> {
        &self.core
    }

    pub fn core_mut(&mut self) -> &mut LiveEditing<H, Grid> {
        &mut self.core
    }

    /// The workbook as opened, which a save lays the room's state over.
    #[must_use]
    pub fn base(&self) -> &Arc<Workbook> {
        &self.map.base
    }

    /// The sheets as the room has them now (copy-on-write: a sheet no edit touched keeps its
    /// pointer).
    #[must_use]
    pub fn sheets(&self) -> Vec<Arc<Sheet>> {
        self.session.documents().iter().find(|d| d.uid == self.map.doc).map(|d| d.wb.sheets.clone()).unwrap_or_default()
    }
}
