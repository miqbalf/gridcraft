//! Which closure delivers what on the web, kept out of the wasm-only shell so it is tested
//! natively.
//!
//! - A Save As or Export copy downloads to the user's computer (`craft_web_host::web::download`),
//!   framed by a Nextcloud page or not: [`Services::download_copy`].
//! - A plain Save of the workbook downloads too ([`Services::download`]), unless a hosting page
//!   stores saves: then [`hosted`] makes only this one the page's poster (`gridcraft:save`).
//!
//! A copy is never handed to the page: the app delivers copies through `download_copy` alone and
//! refuses when it is missing (`SheetApp::save_as_copy`).

use gridcraft_ui_egui::Services;
use gridcraft_ui_egui::craft_share::export::Runtime;

/// Delivers a copy: (name, MIME type, bytes).
pub type CopyFn = Box<dyn Fn(&str, &str, &[u8]) -> Result<(), String>>;
/// Delivers a plain Save of the workbook: (name, bytes).
pub type SaveFn = Box<dyn Fn(&str, &[u8])>;

/// The browser shell: copies download with `copy`, plain saves with `save`.
pub fn browser(services: &mut Services, copy: CopyFn, save: SaveFn) {
    services.runtime = Some(Runtime::Web);
    services.download_copy = Some(copy);
    services.download = Some(save);
}

/// A hosting page: plain saves go to the page with `post`. Copies keep downloading.
pub fn hosted(services: &mut Services, post: SaveFn) {
    services.download = Some(post);
}

/// Where the app runs once the page said whether it stores saves (`gridcraft:host` `saves`).
pub fn host_runtime(page_stores_saves: bool) -> Runtime {
    Runtime::web(page_stores_saves)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::{Arc, Mutex};

    use gridcraft_ui_egui::{SAVE_AS, Services, SheetApp};
    use serde_json::json;

    use super::*;

    #[derive(Default)]
    struct Out {
        downloads: Vec<String>,
        posts: Vec<String>,
    }

    /// The services the web shell builds, framed by a page when `framed`, with what each
    /// delivery did recorded in `out`.
    fn shell(out: &Arc<Mutex<Out>>, framed: Option<bool>) -> Services {
        let mut services = Services::default();
        let (c, s) = (out.clone(), out.clone());
        browser(
            &mut services,
            Box::new(move |name, _, _| {
                c.lock().unwrap().downloads.push(name.into());
                Ok(())
            }),
            Box::new(move |name, _| s.lock().unwrap().downloads.push(name.into())),
        );
        if let Some(saves) = framed {
            let p = out.clone();
            hosted(&mut services, Box::new(move |name, _| p.lock().unwrap().posts.push(name.into())));
            services.runtime = Some(host_runtime(saves));
        }
        services
    }

    fn app(services: Services) -> SheetApp {
        let mut session = gridcraft_engine::Session::new();
        session.new_workbook();
        let mut app = SheetApp::new(session, services);
        app.run("cell.set", json!({"cell": "A1", "input": "42"})).unwrap();
        app
    }

    #[test]
    fn a_hosting_page_gets_plain_saves_and_never_a_save_as_copy() {
        for saves in [false, true] {
            let out = Arc::new(Mutex::new(Out::default()));
            let mut app = app(shell(&out, Some(saves)));
            for f in SAVE_AS.formats() {
                app.save_as_copy(f.id).unwrap();
            }
            // Automation's PDF with no path downloads a copy too.
            app.run("file.exportPdf", json!({})).unwrap();
            {
                let o = out.lock().unwrap();
                assert_eq!(o.downloads.len(), SAVE_AS.formats().len() + 1, "saves: {saves}: every copy downloads");
                assert!(o.posts.is_empty(), "saves: {saves}: no copy is posted to the page");
            }
            app.run("file.save", json!({})).unwrap();
            assert_eq!(out.lock().unwrap().posts.len(), 1, "saves: {saves}: plain Save goes to the page");
        }
    }

    #[test]
    fn unframed_copies_and_plain_saves_both_download() {
        let out = Arc::new(Mutex::new(Out::default()));
        let mut app = app(shell(&out, None));
        app.save_as_copy("csv").unwrap();
        app.run("file.save", json!({})).unwrap();
        let o = out.lock().unwrap();
        assert_eq!(o.downloads.len(), 2);
        assert!(o.posts.is_empty());
    }

    #[test]
    fn without_a_copy_download_save_as_refuses_and_posts_nothing() {
        let out = Arc::new(Mutex::new(Out::default()));
        let mut services = shell(&out, Some(false));
        services.download_copy = None;
        let mut app = app(services);
        assert!(app.save_as_copy("xlsx").is_err(), "a missing copy download is an error");
        assert!(out.lock().unwrap().posts.is_empty(), "never the page's poster");
    }
}
