//! Save As = Export: every Save As and Export entry point ends in [`SheetApp::save_as_copy`]
//! (`craft-share`'s shared flow, `craft_share::export`).
//!
//! The entry points: the File tab's Save As and Export pages, the title bar's Save As… and
//! Export items, F12, Cmd/Ctrl+Shift+S (also in a text box), the key tips, command search, the
//! macOS menu bar and the control channel (`file.saveAs`, `file.exportPdf`, `file.exportCsv` and
//! `file.exportHtml` without a `path`). A `path` is automation writing a file there, as before.
//!
//! A Save As is always a copy: the workbook keeps its file, its name and its unsaved changes.
//! - Desktop: the system save dialog, offering the chosen type, then the file is written there.
//! - Web, framed by a Nextcloud page or not: the copy downloads to the user's computer. It is
//!   never handed to the hosting page; only a plain Save of the page's workbook is.

use craft_share::export::{Delivery, ExportFormat, ExportList, Runtime, SaveAsPlan};

use super::{Json, SheetApp, backstage, json};

/// Every format GridCraft writes, in the order Save As lists them. The labels are the File tab's
/// existing strings (translation keys); the ids are `file.saveBytes` formats, and `pdf`.
pub const SAVE_AS: ExportList = ExportList::new(&[
    ExportFormat::new("xlsx", "Excel Workbook (*.xlsx)", "xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
    ExportFormat::new("csv", "CSV UTF-8 (Comma delimited) (*.csv)", "csv", "text/csv"),
    ExportFormat::new("tsv", "Text (Tab delimited) (*.tsv)", "tsv", "text/tab-separated-values"),
    ExportFormat::new("json", "JSON (*.json)", "json", "application/json"),
    ExportFormat::new("html", "Web Page (*.html)", "html", "text/html"),
    ExportFormat::new("pdf", "PDF (*.pdf)", "pdf", "application/pdf"),
])
// Read-only types GridCraft opens: their extension gives way to the chosen one.
.replacing(&["xls", "xlt", "xlsb", "xlsm", "xltx", "xltm", "ods", "htm", "tab", "txt", "slk"]);

/// The name a workbook with no title gets.
const FALLBACK_NAME: &str = "Book1";

impl SheetApp {
    /// Where this window runs, for Save As: what the shell said, else a window with a save picker
    /// is the desktop and one without is the web.
    pub fn save_as_runtime(&self) -> Runtime {
        self.services.runtime.unwrap_or(if self.services.pick_save.is_some() || self.services.pick_save_as.is_some() {
            Runtime::Desktop
        } else {
            Runtime::Web
        })
    }

    /// The one Save As / Export function: a copy of the active workbook in format `id` (from
    /// [`SAVE_AS`]), downloaded on the web and written where the save dialog says on the desktop.
    /// The workbook's file, name and unsaved state are left alone. `{"cancelled": true}` when the
    /// dialog was cancelled.
    pub fn save_as_copy(&mut self, id: &str) -> Result<Json, String> {
        self.save_as_copy_with(id, json!({}))
    }

    /// [`Self::save_as_copy`] with the PDF options (`range`, `sheets`, `fitToPage`) of
    /// `file.exportPdf`.
    pub(super) fn save_as_copy_with(&mut self, id: &str, pdf_params: Json) -> Result<Json, String> {
        self.settle_drafts()?;
        let plan = self.save_as_plan(id)?;
        match plan.delivery {
            Delivery::Download => {
                let bytes = self.copy_bytes(plan.format, pdf_params)?;
                self.deliver_download(&plan.file_name, plan.format.mime, &bytes)?;
                Ok(json!({"name": plan.file_name, "delivered": "download", "bytes": bytes.len()}))
            }
            Delivery::SaveDialog => {
                let lang = self.ui.language;
                let picked = if let Some(pick) = &self.services.pick_save_as {
                    pick(&plan.file_name, lang.tr(plan.format.label), plan.format.ext)
                } else if let Some(pick) = &self.services.pick_save {
                    pick(&plan.file_name)
                } else {
                    return Err("Saving a copy needs a file picker, which this window doesn't have.".into());
                };
                let Some(picked) = picked else { return Ok(json!({"cancelled": true})) };
                let to = SAVE_AS.picked_path(&picked, id).map_err(|e| e.to_string())?;
                if to.renamed && std::path::Path::new(&to.path).exists() {
                    return Err(format!("{} already exists; choose another name.", to.path));
                }
                let bytes = self.copy_bytes(plan.format, pdf_params)?;
                gridcraft_engine::io::write_file(&to.path, &bytes).map_err(|e| e.to_string())?;
                Ok(json!({"path": to.path, "delivered": "file", "bytes": bytes.len()}))
            }
        }
    }

    /// The format, name and delivery of a copy in format `id`. A workbook with a file is named
    /// after its file; one without (a new workbook, or one a page or the browser handed over) after
    /// its title, where `/` is part of the name, not a folder.
    pub fn save_as_plan(&self, id: &str) -> Result<SaveAsPlan, String> {
        let doc = self.session.active().ok_or("There is no workbook to save.")?;
        let file_name = match &doc.path {
            Some(path) => SAVE_AS.file_name(path, FALLBACK_NAME, id),
            None => SAVE_AS.file_name_from_title(&doc.display_title(), FALLBACK_NAME, id),
        }
        .map_err(|e| e.to_string())?;
        let format = SAVE_AS.get(id).ok_or_else(|| format!("unknown Save As format “{id}”"))?;
        Ok(SaveAsPlan { format, file_name, delivery: self.save_as_runtime().save_as_delivery() })
    }

    /// [`Self::save_as_copy`] for a menu, button or key: an error shows in a message box.
    /// Returns whether a copy was delivered (not cancelled, no error).
    pub fn save_as_copy_or_alert(&mut self, id: &str) -> bool {
        match self.save_as_copy(id) {
            Ok(r) => r.get("cancelled").is_none(),
            Err(e) => {
                self.alert(e);
                false
            }
        }
    }

    /// Save As with no type chosen yet (F12, Cmd/Ctrl+Shift+S, the title bar's Save As…, key
    /// tips, command search, `file.saveAs` without a path): the File tab's Save As page, where the
    /// type is picked. A window without the File tab saves a workbook copy (`.xlsx`) at once.
    pub(super) fn open_save_as(&mut self, params: &Json) {
        if let Some(id) = params.get("format").and_then(Json::as_str).filter(|f| !f.is_empty()) {
            self.save_as_copy_or_alert(id);
        } else if self.chrome.file_tab {
            self.open_backstage(backstage::Page::SaveAs);
        } else {
            self.save_as_copy_or_alert("xlsx");
        }
    }

    /// The bytes of the copy: the engine's encoder for workbook formats, the print for PDF.
    fn copy_bytes(&mut self, format: ExportFormat, pdf_params: Json) -> Result<Vec<u8>, String> {
        self.store_view_state();
        let r = if format.id == "pdf" {
            let mut params = if pdf_params.is_object() { pdf_params } else { json!({}) };
            if let Json::Object(map) = &mut params {
                map.remove("path");
            }
            self.session.run("file.exportPdf", params)?
        } else {
            self.session.run("file.saveBytes", json!({"format": format.id}))?
        };
        r.get("base64")
            .and_then(Json::as_str)
            .and_then(gridcraft_engine::io::base64_decode)
            .ok_or_else(|| "The copy could not be encoded.".to_string())
    }

    /// A browser download, never the hosting page: [`crate::Services::download_copy`]. A page
    /// that does not store saves may fall back to the plain download.
    fn deliver_download(&self, name: &str, mime: &str, bytes: &[u8]) -> Result<(), String> {
        if let Some(download) = &self.services.download_copy {
            return download(name, mime, bytes);
        }
        match &self.services.download {
            Some(download) if self.save_as_runtime() != Runtime::WebHosted => {
                download(name, bytes);
                Ok(())
            }
            _ => Err("Downloading a copy is not available in this window.".into()),
        }
    }
}
