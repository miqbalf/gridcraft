//! GridCraft's document service for `craft-live-worker`: opens an `.xlsx` into a live room and
//! writes the room's workbook back from a checkpoint.

use std::sync::Arc;

use craft_collab::Hub;
use craft_live::{Access, LiveEditing, SERVICE_UID, uid_of};
use gridcraft_engine::{DocState, Session, Trust, io};
use gridcraft_live::{BaseFile, Grid, GridWorkerLive, volatile_for, workbook_from_state};
use gridcraft_model::Sheet;

/// The uid the service peer writes under (it never writes).
const WORKER_UID: &str = "craft_office";

/// The log target of the routine save line. The worker's own logger shows warnings only;
/// [`install_log`] adds this one target at info.
pub const SAVE_LOG: &str = "gridcraft_live_worker::save";

struct WorkerLog;

impl log::Log for WorkerLog {
    fn enabled(&self, m: &log::Metadata<'_>) -> bool {
        m.level() <= log::Level::Warn || (m.level() <= log::Level::Info && m.target() == SAVE_LOG)
    }
    fn log(&self, r: &log::Record<'_>) {
        if self.enabled(r.metadata()) {
            eprintln!("live-worker {} {}: {}", r.level(), r.target(), r.args());
        }
    }
    fn flush(&self) {}
}

static LOG: WorkerLog = WorkerLog;

/// Installs the stderr logger before `serve` would: warnings from everywhere, and the save line
/// at info. (`serve` keeps a logger that is already installed.)
pub fn install_log() {
    if log::set_logger(&LOG).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}

pub struct GridService;

/// One workbook open in a room, as the worker keeps it.
pub struct GridDoc<H: Hub> {
    live: GridWorkerLive<H>,
}

impl craft_live_worker::DocumentService for GridService {
    type Doc<H: Hub> = GridDoc<H>;

    fn accepts(&self, file_name: &str) -> bool {
        // `.xlsx` only: any other format would be rewritten as xlsx and lose what it holds.
        std::path::Path::new(file_name).extension().is_some_and(|e| e.eq_ignore_ascii_case("xlsx"))
    }

    fn pinned_base(&self) -> bool {
        true
    }

    fn open<H: Hub>(&self, hub: H, room: &str, file_name: &str, bytes: &[u8]) -> Result<GridDoc<H>, String> {
        // `RoomFiles::base` already checked the base against its hash.
        let base = BaseFile { bytes, name: file_name, expected_sha256: None };
        let live = GridWorkerLive::start(hub, room, &base, Access::Edit, WORKER_UID).map_err(|e| e.to_string())?;
        Ok(GridDoc { live })
    }
}

impl<H: Hub> craft_live_worker::LiveDocument<H> for GridDoc<H> {
    type Kind = Grid;
    type Snapshot = Vec<Arc<Sheet>>;

    fn live(&self) -> &LiveEditing<H, Grid> {
        self.live.core()
    }

    fn live_mut(&mut self) -> &mut LiveEditing<H, Grid> {
        self.live.core_mut()
    }

    fn pump(&mut self) -> usize {
        self.live.pump().iter().filter(|p| uid_of(&p.id) != SERVICE_UID).count()
    }

    fn snapshot(&self) -> Self::Snapshot {
        self.live.sheets()
    }

    fn unchanged(&self, a: &Self::Snapshot, b: &Self::Snapshot) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| Arc::ptr_eq(x, y))
    }

    fn same(&self, a: &Self::Snapshot, b: &Self::Snapshot) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| Arc::ptr_eq(x, y) || **x == **y)
    }

    fn save(&self, state: &[u8], name: &str) -> Result<Vec<u8>, String> {
        // The state is the room's overlay on the base; the parts the model does not hold come
        // from the base the workbook was opened from.
        // A session that fell out of step must never overwrite the file with what it holds.
        if let Some(why) = self.live.broken() {
            return Err(format!("not saved: {why}"));
        }
        log::info!(target: SAVE_LOG, "saving; the room's overlay state is {} bytes", state.len());
        let base = self.live.base();
        let source = volatile_for(base, state).map_err(|e| format!("the room's state did not load: {e}"))?;
        let wb = workbook_from_state(base, state, Some(source.clone())).map_err(|e| format!("the room's state did not apply: {e}"))?;
        // A full recalculation with the room's shared random numbers.
        let mut s = Session::new();
        s.add_document(DocState::new(wb, None, "save".to_owned(), Trust::FromFile));
        let uid = s.active().map(|d| d.uid).ok_or("no workbook")?;
        s.set_volatile_source(uid, Some(source)).map_err(|e| e.to_string())?;
        let wb = s.active().map(|d| d.wb.clone()).ok_or("no workbook")?;
        io::save_bytes(&wb, name).map_err(|e| e.to_string())
    }
}
