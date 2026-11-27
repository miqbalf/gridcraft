//! Share: Nextcloud sharing for the open workbook, and nothing else (`craft-share`'s hook).
//!
//! Every Share entry point (the title bar's Share button, command search, the control channel and
//! `window.gridcraft.execute`) runs `file.share`, which comes here. It never exports, mails or
//! opens a dialog of ours:
//! - framed by a Nextcloud page that shares files ([`Services::host_share`]) and showing the file
//!   that page opened: the page opens its sharing for it;
//! - on the desktop, a workbook saved in a folder the Nextcloud desktop client syncs
//!   ([`Services::share_route`]): the browser opens that file's sharing in Nextcloud;
//! - anywhere else: "No Nextcloud connected" in a toast.

use craft_share::ShareRoute;

use super::{Json, SheetApp, json, now_ms};

/// A second file from the page, while its first is still open.
const HOST_ONE_FILE: &str = "Share works only for {name}, the first file opened from Nextcloud";

/// The page could not be asked to share (it went away).
const HOST_UNREACHABLE: &str = "Couldn't reach Nextcloud to share this workbook";

impl SheetApp {
    /// Where Share goes for the active workbook.
    pub fn share_route(&self) -> ShareRoute {
        let Some(doc) = self.session.active() else { return ShareRoute::None };
        let path = doc.path.as_deref();
        if self.services.host_share.is_some() {
            // The page shares the one workbook it opened in the frame; a workbook made or opened
            // in the frame is not that file, even under the same name. Bound by uid alone: on the
            // web GridCraft never sets the page's workbook's path (a name the page answers with
            // only retitles it), and the page always shares the file it opened. DeckCraft also
            // compares the path, because its host saves set it (SH2 fix 1, review I3).
            return if self.services.host_doc == Some(doc.uid) { ShareRoute::NextcloudWebHost } else { ShareRoute::None };
        }
        match (self.share_routes.get(&doc.uid), path) {
            (Some((at, route)), Some(path)) if at == path => route.clone(),
            _ => ShareRoute::None,
        }
    }

    /// Desktop: works out the active workbook's Share route when it is new to the cache (opened,
    /// switched to) or its path changed (Save As). Reading the Nextcloud client's settings is file
    /// I/O, so this runs once per workbook and path, never on every frame.
    pub(crate) fn note_share_route(&mut self) {
        let Some(lookup) = &self.services.share_route else { return };
        let Some(doc) = self.session.active() else { return };
        let Some(path) = doc.path.as_deref() else { return };
        if self.share_routes.get(&doc.uid).is_some_and(|(at, _)| at == path) {
            return;
        }
        let route = lookup(std::path::Path::new(path));
        self.share_routes.insert(doc.uid, (path.to_string(), route));
    }

    /// Web: the hosting page's own open (from [`crate::Services::host_inbox`], or the password
    /// prompt it led to) produced the workbook in `opened` (`file.open`'s result): it is the
    /// page's, and Share asks the page for it alone. The page edits one file per frame: while its
    /// first workbook is still open, a later one is opened but not bound, and a toast says which
    /// file Share is for.
    pub(crate) fn bind_host_open(&mut self, opened: &Json) {
        let docs = self.session.documents();
        let Some(uid) = opened.get("index").and_then(Json::as_u64).and_then(|i| docs.get(usize::try_from(i).ok()?)).map(|d| d.uid) else { return };
        match self.services.host_doc.and_then(|first| docs.iter().find(|d| d.uid == first && d.uid != uid)) {
            Some(first) => {
                let first = first.title.clone();
                self.follow_active_window();
                let text = self.ui.language.tr_fill(HOST_ONE_FILE, &[("name", &first)]);
                self.toast = Some((text, now_ms()));
            }
            None => self.services.host_doc = Some(uid),
        }
    }

    /// The one Share function (`file.share`).
    pub(super) fn share(&mut self) -> Result<Json, String> {
        // Normally already known from the frame the workbook became active in.
        self.note_share_route();
        match self.share_route() {
            ShareRoute::NextcloudWebHost => {
                let post = self.services.host_share.as_ref().ok_or_else(|| craft_share::NO_NEXTCLOUD.to_string())?;
                match post() {
                    Ok(()) => Ok(json!({"shared": "host"})),
                    // Said in a toast, like "No Nextcloud connected"; automation gets the reason.
                    Err(e) => {
                        self.follow_active_window();
                        self.toast = Some((HOST_UNREACHABLE.to_string(), now_ms()));
                        Ok(json!({"shared": false, "message": HOST_UNREACHABLE, "error": e}))
                    }
                }
            }
            ShareRoute::NextcloudDesktop { server, remote_path, .. } => {
                let url = craft_share::sharing_url(&server, &remote_path);
                match &self.services.open_url {
                    Some(open) => {
                        open(&url);
                        Ok(json!({"shared": "browser", "url": url}))
                    }
                    None => Ok(self.no_nextcloud()),
                }
            }
            ShareRoute::None => Ok(self.no_nextcloud()),
        }
    }

    fn no_nextcloud(&mut self) -> Json {
        self.follow_active_window();
        self.toast = Some((craft_share::NO_NEXTCLOUD.to_string(), now_ms()));
        json!({"shared": false, "message": craft_share::NO_NEXTCLOUD})
    }
}
