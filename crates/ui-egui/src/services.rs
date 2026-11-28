use super::printing;

type PickSave = Box<dyn Fn(&str) -> Option<String>>;
type OpenUrl = Box<dyn Fn(&str)>;
type CopyHtml = Box<dyn FnMut(&str, &str) -> Result<(), String>>;
type Download = Box<dyn Fn(&str, &[u8])>;
type PrintPdf = Box<dyn Fn(&[u8])>;
/// Hands the copy of unmerged live edits (name, bytes) to the host page as a new file. `Ok(Some(id))`
/// when the page will answer it ([`crate::SheetApp::copy_answered`]), `Ok(None)` when the host
/// downloaded it instead (a page that does not answer saves), `Err` when it could not be handed
/// over.
pub type SaveCopy = Box<dyn FnMut(&str, &[u8]) -> Result<Option<String>, String>>;
type HostShare = Box<dyn Fn() -> Result<(), String>>;
type ShareRoute = Box<dyn Fn(&std::path::Path) -> craft_share::ShareRoute>;

/// Host services the platform shell provides (file pickers, opening URLs).
#[derive(Default)]
pub struct Services {
    /// Desktop: discover configured printers on a worker.
    pub printers: Option<printing::Discover>,
    /// Desktop: submit the PDF with the chosen settings on a worker.
    pub print_native: Option<printing::Submit>,
    pub pick_open: Option<Box<dyn Fn() -> Option<String>>>,
    pub pick_save: Option<PickSave>,
    /// Native picture picker (PNG/JPEG), separate from workbook opening.
    pub pick_picture: Option<Box<dyn Fn() -> Option<String>>>,
    /// Browser picture picker. Each dialog owns its inbox, so canceled picks cannot
    /// insert into a subsequent dialog or open the image as a workbook.
    pub pick_picture_async: Option<Box<dyn Fn(Inbox)>>,
    pub open_url: Option<OpenUrl>,
    /// Publish HTML and its plain-text alternative together. `Ok` means the host owns the
    /// write (including any asynchronous fallback); an immediate error uses egui's text path.
    pub copy_html: Option<CopyHtml>,
    /// Read the HTML flavour of the system clipboard when the user pastes text, so cells the
    /// source spreadsheet recorded as numbers stay numbers ("Rp10,000.00" → 10000). `None` from
    /// the host pastes the plain text alone.
    pub paste_html: Option<Box<dyn FnMut() -> Option<String>>>,
    /// Read the system clipboard's plain text for the ribbon and menu Paste commands (Cmd/Ctrl+V
    /// gets it from egui's `Event::Paste`). `None` from the host, or no service (web), pastes
    /// GridCraft's own clipboard.
    pub paste_text: Option<Box<dyn FnMut() -> Option<String>>>,
    /// Read the picture on the system clipboard for the ribbon and menu Paste commands when it
    /// holds no text (Cmd/Ctrl+V gets it from egui's `Event::PasteImage`).
    pub paste_image: Option<Box<dyn FnMut() -> Option<image::RgbaImage>>>,
    /// Web: deliver a file to the user (name, bytes).
    pub download: Option<Download>,
    /// Web: start an asynchronous file pick; the bytes arrive later in `inbox`.
    pub open_async: Option<Box<dyn Fn()>>,
    /// Files delivered asynchronously (name, bytes), opened on the next frame.
    pub inbox: Option<Inbox>,
    /// Web: hand PDF bytes to the browser's print flow directly (no download,
    /// no intermediate file) — opens the system print dialog on that PDF.
    pub print: Option<PrintPdf>,
    /// Live co-editing: the session the host started on this window's workbook (it opened the
    /// workbook as the session's active document). [`crate::live`] pumps it every frame and
    /// routes the window's commands through it.
    pub live: Option<crate::live::Live>,
    /// Desktop live: the room's document service saves the live workbook, so Save on it writes
    /// nothing here and says so, AutoSave leaves it alone, and it counts as saved once the room
    /// has every edit ([`crate::live`]). Save As and the exports still write local files.
    pub room_saves: bool,
    /// Web host: hand the copy of unmerged live edits to the page as a new file beside the
    /// original, also while live (the workbook itself is the room's).
    pub save_copy: Option<SaveCopy>,
    /// Web: the browser download for a copy of unmerged edits the page could not store.
    pub download_copy: Option<Download>,
    /// Web: the page hosting GridCraft shares files itself (`gridcraft:host` with `share: true`).
    /// Share posts `gridcraft:share` to it, for the workbook it opened ([`Services::host_doc`]).
    pub host_share: Option<HostShare>,
    /// Web: the id (`DocState::uid`) of the workbook the hosting page opened in the frame: the one
    /// its own open from [`Services::host_inbox`] produced. Share asks the page only while that
    /// workbook is the active one, whatever it is named.
    pub host_doc: Option<u64>,
    /// Web: files the hosting page sent (name, bytes), opened on the next frame like
    /// [`Services::inbox`]; the workbook such an open produces becomes [`Services::host_doc`].
    pub host_inbox: Option<Inbox>,
    /// Desktop: where Share goes for a workbook saved at a path (`craft_share::desktop_route`:
    /// Nextcloud sharing in the browser for a file in a folder the Nextcloud client syncs).
    pub share_route: Option<ShareRoute>,
}

/// Shared queue of files read asynchronously (browser file picker, drag and drop).
pub type Inbox = std::sync::Arc<std::sync::Mutex<Vec<(String, Vec<u8>)>>>;

/// Which parts of the app's own window chrome are drawn. Everything is on by default; a host
/// that embeds the sheet in its own window (and owns files itself) can turn off the title bar
/// and the File tab, whose Backstage view then never opens. Not persisted: it describes the
/// host, not a user preference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chrome {
    /// The title bar: AutoSave, Home, Save, Undo/Redo, the file name, Comments, Share, Search and
    /// the theme switch.
    pub title_bar: bool,
    /// The ribbon's File tab and the Backstage view it opens (also reached by keyboard and
    /// commands such as `ui.backstage`).
    pub file_tab: bool,
}

impl Default for Chrome {
    fn default() -> Self {
        Chrome { title_bar: true, file_tab: true }
    }
}
