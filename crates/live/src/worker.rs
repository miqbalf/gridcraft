//! [`GridWorkerLive`]: the document service's side of a room. It makes no edits of its own, so
//! it keeps the room's workbook up to date from the other editors and needs neither the
//! structure lock nor the join counter of [`GridLive`](crate::GridLive); its link is the plain
//! `LiveEditing<H, Grid>`, which is the type `craft-live-worker` wants.

use std::sync::Arc;

use craft_collab::Hub;
use craft_live::{Access, LiveEditing, LiveStatus};
use gridcraft_engine::Session;
use gridcraft_model::{Sheet, Workbook};

use crate::app::{GridApp, Mapping};
use crate::doc::GridDoc;
use crate::kind::Grid;
use crate::seed::{open_base, seat};
use crate::session::{BaseFile, LiveFail, Peer};
use crate::styles::fnv1a64;

/// Why the service peer ended its own link when its replica could not catch up.
pub const SERVICE_RESYNC_TEXT: &str =
    "The document service fell out of step with the room, so this room is no longer saved. Reopen the file to start a new live session.";

/// The base workbook with the room's changes applied as they arrive, over hub `H` itself.
pub struct GridWorkerLive<H: Hub> {
    core: LiveEditing<H, Grid>,
    map: Mapping,
    session: Session,
    room: String,
    /// The replica fell out of step and the link was ended (done once).
    ended: bool,
}

impl<H: Hub> GridWorkerLive<H> {
    /// Opens `base`, checks its hash when one is given, and joins `room` as `uid`.
    ///
    /// # Errors
    /// [`LiveFail`].
    pub fn start(hub: H, room: &str, base: &BaseFile, access: Access, uid: &str) -> Result<Self, LiveFail> {
        let (wb, seed) = open_base(base)?;
        let core = LiveEditing::<H, Grid>::start(hub, room, &seed, access).map_err(LiveFail::Live)?;
        let mut session = Session::new();
        let map = seat(&mut session, wb, &seed, "live".to_owned(), uid, fnv1a64, base.name)?;
        let source: Arc<dyn gridcraft_calc::VolatileSource> = map.vol.clone();
        session.set_volatile_source(map.doc, Some(source)).map_err(|e| LiveFail::Open(e.to_string()))?;
        Ok(GridWorkerLive { core, map, session, room: room.to_owned(), ended: false })
    }

    /// One turn of the link: takes in the other editors' changes. The editors in the room.
    pub fn pump(&mut self) -> Vec<Peer> {
        let mut app = GridApp { session: &mut self.session, map: &mut self.map };
        let peers = self.core.pump(&mut app);
        if !self.ended && self.core.live().doc().is_some_and(GridDoc::needs_resync) {
            // M1: a replica that cannot catch up ends the link, and nothing is saved from it.
            self.ended = true;
            log::warn!("gridcraft-live: room {}: the service replica fell out of step; the room is no longer saved", self.room);
            self.core.end(SERVICE_RESYNC_TEXT);
        }
        peers
    }

    /// Why this session must not be saved from, if it must not: its replica needs the room's
    /// full state, or its link ended for good. A checkpoint taken then may be behind the room.
    #[must_use]
    pub fn broken(&self) -> Option<String> {
        if let LiveStatus::Broken(why) = self.core.status() {
            return Some(why.clone());
        }
        self.core.live().doc().is_some_and(GridDoc::needs_resync).then(|| SERVICE_RESYNC_TEXT.to_owned())
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
