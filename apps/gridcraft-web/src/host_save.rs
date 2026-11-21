//! Host save payloads and workbook receipts, shared with native regression tests.

use std::sync::Arc;

use gridcraft_model::Workbook;
use gridcraft_ui_egui::SheetApp;

/// Only these outputs replace the editable workbook in the host.
const WORKBOOK_EXTS: &[&str] = &["xlsx"];

pub struct SaveMessage<'a> {
    pub name: &'a str,
    pub bytes: &'a [u8],
    pub export: bool,
    /// Set when the host stores saves and answers each one ([`Receipts`]).
    pub id: Option<&'a str>,
    /// The workbook is saved under a name other than the file the host opened.
    pub save_as: bool,
}

impl<'a> SaveMessage<'a> {
    pub fn new(name: &'a str, bytes: &'a [u8]) -> Self {
        // All other outputs are exports, not necessarily PDFs.
        Self { name, bytes, export: !is_workbook(name), id: None, save_as: false }
    }

    /// The save of a host that answers each one: `id` names it, and `opened` is the name of the
    /// file the host sent (a workbook under another name must not replace that file).
    pub fn answered(mut self, id: &'a str, opened: Option<&str>) -> Self {
        self.id = Some(id);
        self.save_as = !self.export && opened.is_some_and(|o| o != self.name);
        self
    }
}

fn is_workbook(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| WORKBOOK_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// Preserve the bridge's existing rule: only a workbook receipt marks the document saved.
pub fn sync_saved(saved: &[String], app: &mut SheetApp) {
    if let Some(name) = saved.iter().rev().find(|name| is_workbook(name))
        && let Some(doc) = app.session.active_mut()
    {
        doc.saved = doc.wb.clone();
        if doc.path.is_none() {
            doc.title = name.clone();
        }
    }
}

/// What the host answered to a save that carried an id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Stored; `name` is set when the host kept it under another name (a conflict copy, Save As).
    Saved { id: String, name: Option<String> },
    /// Not stored.
    Failed { id: String, error: String },
}

/// A save handed to the host that has not been answered yet.
struct Waiting {
    id: String,
    name: String,
    /// The workbook that went out: the document it came from and what it held. An export holds none.
    held: Option<(u64, Arc<Workbook>)>,
}

/// The saves sent to a host that stores them and answers each (`host.saves`). A workbook counts as
/// saved only when its `saved` arrives; a `save-failed` leaves it unsaved and says why.
#[derive(Default)]
pub struct Receipts {
    #[cfg(target_arch = "wasm32")]
    confirms: bool,
    next: u64,
    waiting: Vec<Waiting>,
}

impl Receipts {
    /// The host said it stores saves and answers each.
    #[cfg(target_arch = "wasm32")]
    pub fn set_confirms(&mut self) {
        self.confirms = true;
    }

    #[cfg(target_arch = "wasm32")]
    pub fn confirms(&self) -> bool {
        self.confirms
    }

    /// A new id for the file `name` that is about to go to the host.
    pub fn start(&mut self, name: &str) -> String {
        self.next += 1;
        let id = format!("save-{}", self.next);
        self.waiting.push(Waiting { id: id.clone(), name: name.to_string(), held: None });
        id
    }

    /// The post for `id` failed, so no answer will come.
    #[cfg(target_arch = "wasm32")]
    pub fn cancel(&mut self, id: &str) {
        self.waiting.retain(|w| w.id != id);
    }

    /// After the frame that handed workbooks over: remember what the active workbook held then, so
    /// a `saved` later marks exactly that as saved and not edits made in between.
    pub fn hold(&mut self, app: &SheetApp) {
        let Some(doc) = app.session.active() else { return };
        for w in self.waiting.iter_mut().filter(|w| w.held.is_none() && is_workbook(&w.name)) {
            w.held = Some((doc.uid, doc.wb.clone()));
        }
    }

    /// Act on the host's answer. One for a save that is not waiting (unknown, repeated) changes
    /// nothing. Returns the name the host stored a workbook under, when that is its file now.
    pub fn answer(&mut self, app: &mut SheetApp, answer: Answer) -> Option<String> {
        let (Answer::Saved { id, .. } | Answer::Failed { id, .. }) = &answer;
        let at = self.waiting.iter().position(|w| w.id == *id)?;
        let waiting = self.waiting.remove(at);
        match answer {
            Answer::Failed { error, .. } => {
                let text = app.ui.language.tr("Couldn't save to Nextcloud: {error}").replace("{error}", &error);
                app.message = Some(("GridCraft".into(), text));
                None
            }
            Answer::Saved { name, .. } => {
                // An export is a copy: the workbook stays as it was.
                let (uid, held) = waiting.held?;
                let name = name.unwrap_or(waiting.name);
                if let Some(doc) = app.session.document_mut(uid) {
                    doc.saved = held;
                    if doc.path.is_none() {
                        doc.title = name.clone();
                    }
                }
                Some(name)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gridcraft_engine::Session;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn save_payload_marks_exports_and_preserves_name_and_bytes() {
        let bytes = [0, 1, 127, 128, 255];
        for (name, export) in [("finance.xlsx", false), ("finance.XLSX", false), ("finance.pdf", true), ("finance.PDF", true)] {
            let message = SaveMessage::new(name, &bytes);
            assert_eq!(message.export, export, "{name}");
            assert_eq!(message.name, name);
            assert_eq!(message.bytes, bytes.as_slice());
            assert_eq!(message.bytes.as_ptr(), bytes.as_ptr(), "the payload borrows the original bytes");
        }
    }

    #[test]
    fn other_outputs_are_exports_not_workbook_saves() {
        for name in ["finance.csv", "finance.bin", "finance", "finance.xlsx.pdf", "finance.xlsx.bak"] {
            assert!(SaveMessage::new(name, &[]).export, "{name}");
        }
        assert!(!SaveMessage::new("finance.pdf.xlsx", &[]).export);
    }

    fn edited_app() -> Result<SheetApp, String> {
        let mut session = Session::new();
        session.new_workbook();
        session.execute("cell.set", json!({"cell": "A1", "input": "42"})).map_err(|e| e.to_string())?;
        Ok(SheetApp::new(session, Default::default()))
    }

    #[test]
    fn pdf_receipts_leave_the_workbook_dirty_and_its_title_unchanged() -> Result<(), String> {
        for name in ["finance.pdf", "finance.PDF"] {
            let mut app = edited_app()?;
            let doc = app.session.active().ok_or("no active document")?;
            assert!(doc.is_dirty());
            let title = doc.title.clone();
            let saved = doc.saved.clone();
            sync_saved(&[name.to_string()], &mut app);
            let doc = app.session.active().ok_or("no active document")?;
            assert!(doc.is_dirty(), "{name}");
            assert!(Arc::ptr_eq(&saved, &doc.saved), "an export must not replace the saved snapshot");
            assert_eq!(doc.title, title);
        }
        Ok(())
    }

    #[test]
    fn workbook_receipts_mark_saved_and_later_exports_do_not_replace_them() -> Result<(), String> {
        for name in ["finance.xlsx", "finance.XLSX"] {
            let mut app = edited_app()?;
            assert!(app.session.active().ok_or("no active document")?.is_dirty());
            sync_saved(&[name.to_string(), "finance.pdf".to_string()], &mut app);
            let doc = app.session.active().ok_or("no active document")?;
            assert!(!doc.is_dirty());
            assert!(Arc::ptr_eq(&doc.saved, &doc.wb));
            assert_eq!(doc.title, name);
            app.session.execute("cell.set", json!({"cell": "A1", "input": "43"})).map_err(|e| e.to_string())?;
            sync_saved(&["finance.PDF".to_string()], &mut app);
            assert!(app.session.active().ok_or("no active document")?.is_dirty(), "a subsequent export leaves new edits unsaved");
        }
        Ok(())
    }

    fn sent(app: &SheetApp, receipts: &mut Receipts, name: &str) -> String {
        let id = receipts.start(name);
        receipts.hold(app);
        id
    }

    fn saved(id: &str, name: Option<&str>) -> Answer {
        Answer::Saved { id: id.into(), name: name.map(Into::into) }
    }

    #[test]
    fn a_workbook_is_saved_only_when_the_host_says_so_and_takes_its_name() -> Result<(), String> {
        let mut app = edited_app()?;
        let mut receipts = Receipts::default();
        let id = sent(&app, &mut receipts, "Budget.xlsx");
        assert!(app.session.active().ok_or("no document")?.is_dirty(), "handing the bytes over is not saving them");
        assert_eq!(receipts.answer(&mut app, saved(&id, Some("Budget (conflict).xlsx"))).as_deref(), Some("Budget (conflict).xlsx"));
        let doc = app.session.active().ok_or("no document")?;
        assert!(!doc.is_dirty());
        assert_eq!(doc.title, "Budget (conflict).xlsx");
        Ok(())
    }

    #[test]
    fn a_refused_save_stays_unsaved_and_says_why() -> Result<(), String> {
        let mut app = edited_app()?;
        let mut receipts = Receipts::default();
        let id = sent(&app, &mut receipts, "Budget.xlsx");
        receipts.answer(&mut app, Answer::Failed { id, error: "the file is locked".into() });
        assert!(app.session.active().ok_or("no document")?.is_dirty());
        let (_, text) = app.message.clone().ok_or("no message")?;
        assert_eq!(text, "Couldn't save to Nextcloud: the file is locked");
        Ok(())
    }

    #[test]
    fn an_answer_counts_once_and_only_for_its_own_save() -> Result<(), String> {
        let mut app = edited_app()?;
        let mut receipts = Receipts::default();
        let id = sent(&app, &mut receipts, "Budget.xlsx");
        receipts.answer(&mut app, saved("save-99", None));
        assert!(app.session.active().ok_or("no document")?.is_dirty(), "an unknown id changes nothing");
        receipts.answer(&mut app, saved(&id, None));
        assert!(!app.session.active().ok_or("no document")?.is_dirty());
        app.session.execute("cell.set", json!({"cell": "A1", "input": "43"})).map_err(|e| e.to_string())?;
        receipts.answer(&mut app, saved(&id, None));
        assert!(app.session.active().ok_or("no document")?.is_dirty(), "a repeated answer does not save a later edit");
        receipts.answer(&mut app, Answer::Failed { id, error: "late".into() });
        assert!(app.message.is_none(), "nor does a late failure of a save already answered");
        Ok(())
    }

    #[test]
    fn work_done_while_the_host_stores_the_save_stays_unsaved() -> Result<(), String> {
        let mut app = edited_app()?;
        let mut receipts = Receipts::default();
        let id = sent(&app, &mut receipts, "Budget.xlsx");
        app.session.execute("cell.set", json!({"cell": "A1", "input": "44"})).map_err(|e| e.to_string())?;
        receipts.answer(&mut app, saved(&id, None));
        assert!(app.session.active().ok_or("no document")?.is_dirty(), "the stored file lacks the later edit");
        Ok(())
    }

    #[test]
    fn a_stored_export_does_not_save_the_workbook() -> Result<(), String> {
        let mut app = edited_app()?;
        let mut receipts = Receipts::default();
        let id = sent(&app, &mut receipts, "Budget.pdf");
        receipts.answer(&mut app, saved(&id, None));
        assert!(app.session.active().ok_or("no document")?.is_dirty());
        Ok(())
    }

    #[test]
    fn a_workbook_under_another_name_is_a_new_file_not_the_opened_one() {
        let bytes = [1, 2, 3];
        assert!(!SaveMessage::new("Budget.xlsx", &bytes).answered("save-1", Some("Budget.xlsx")).save_as);
        assert!(SaveMessage::new("Other.xlsx", &bytes).answered("save-1", Some("Budget.xlsx")).save_as);
        assert!(!SaveMessage::new("Budget.pdf", &bytes).answered("save-1", Some("Budget.xlsx")).save_as, "an export is its own copy");
        assert_eq!(SaveMessage::new("Budget.xlsx", &bytes).answered("save-7", None).id, Some("save-7"));
    }
}
