//! File viewer windows. Double-clicking a Markdown, source, plain-text or image file
//! in the Files panel opens it in its own native window (an egui deferred
//! viewport): Markdown rendered by `egui_commonmark` (see `crate::markdown`),
//! source highlighted by `syntect`, plain text as is, images decoded by the
//! `image` crate. Each window re-reads its file when it changes on disk, so it
//! can sit beside an editor as a live preview. Text files open read-only;
//! the toolbar can switch them to a small editor (see `editor`).

use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use egui::{
    Button, CentralPanel, Color32, ColorImage, Context, Frame, Id, Key, Margin, Modal, Modifiers,
    Panel, Rect, RichText, ScrollArea, Sense, Shape, TextEdit, TextStyle, TextureFilter,
    TextureHandle, TextureOptions, Ui, UiBuilder, Vec2, ViewportBuilder, ViewportCommand,
    ViewportId, emath::GuiRounding, pos2, scroll_area, vec2,
};
use egui_commonmark::CommonMarkCache;
use image::{
    DynamicImage, ImageDecoder, ImageReader, ImageResult, imageops::FilterType,
    metadata::Orientation,
};

use crate::{
    files::{format_size, open_with_default_app},
    markdown::{self, Block, Details, is_markdown},
    placement::Placement,
    ui::{
        scratchpad::{self, ScratchpadTab},
        terminal::{TOGGLE_BOOKMARK, take_shortcut},
    },
};

mod editor;
mod gutter;
mod search;
mod selection;
mod syntax;

/// Extensions (lowercase) of plain-text files. A `.lock` file that `syntax`
/// recognizes by name is highlighted instead.
const TEXT_EXTENSIONS: [&str; 2] = ["txt", "lock"];
/// Names (lowercase) of plain-text files that have no extension, or none as
/// far as `Path::extension` is concerned: a leading dot starts the name.
const TEXT_FILE_NAMES: [&str; 3] = [".gitignore", ".gitattributes", "license"];
/// Extensions (lowercase) of images: the formats enabled on the `image` crate
/// in Cargo.toml.
const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "jfif", "bmp"];

/// Zoom limits for images, in screen pixels per image pixel.
const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 32.;

/// Zoomed in, image pixels show as crisp squares rather than a blur, as in
/// most image viewers.
const IMAGE_TEXTURE: TextureOptions = TextureOptions {
    magnification: TextureFilter::Nearest,
    ..TextureOptions::LINEAR
};

/// The longest side, in pixels, the Files panel's preview (see
/// [ImagePreview]) scales an image down to: plenty for a pane at the bottom
/// of a side panel, and quick to upload.
const PREVIEW_MAX_SIDE: u32 = 2048;

/// How often an open window checks its file's modification time.
const RELOAD_POLL: Duration = Duration::from_secs(1);

/// How much of a file a window reads. Every window draws on the UI thread,
/// so laying out a huge log would stall the whole app; the rest is left to
/// "Open with OS default".
const MAX_BYTES: u64 = 1 << 20;

const WINDOW_SIZE: [f32; 2] = [820., 900.];
const MIN_WINDOW_SIZE: [f32; 2] = [300., 200.];
/// New windows open at the last window's size, but no smaller than this.
const MIN_REMEMBERED_SIZE: Vec2 = vec2(500., 400.);

/// Whether `path` is a file type the viewer shows, so a double-click views it
/// here rather than handing it to the OS's default program.
pub fn can_view(path: &Path) -> bool {
    is_markdown(path) || is_text(path) || is_image(path) || syntax::language(path).is_some()
}

fn lowercase(s: &OsStr) -> String {
    s.to_string_lossy().to_lowercase()
}

fn is_text(path: &Path) -> bool {
    path.extension()
        .map(lowercase)
        .is_some_and(|e| TEXT_EXTENSIONS.contains(&e.as_str()))
        || path
            .file_name()
            .map(lowercase)
            .is_some_and(|n| TEXT_FILE_NAMES.contains(&n.as_str()))
}

pub fn is_image(path: &Path) -> bool {
    path.extension()
        .map(lowercase)
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.as_str()))
}

/// How a window renders its file.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Markdown,
    /// Source text, with its canonical syntax extension.
    Code(&'static str),
    /// Plain text, in the monospace font.
    Text,
    Image,
}

impl Kind {
    fn of(path: &Path) -> Self {
        if is_markdown(path) {
            Self::Markdown
        } else if is_image(path) {
            Self::Image
        } else if let Some(language) = syntax::language(path) {
            Self::Code(language)
        } else {
            Self::Text
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Markdown => "Markdown",
            Self::Code(_) => "Code",
            Self::Text => "Text",
            Self::Image => "Image",
        }
    }
}

/// The open viewer windows. Shared with each window's render callback, which
/// egui calls outside the main window's pass, hence the `Arc<Mutex>`.
/// Everything runs on the UI thread, so the lock is never contended.
#[derive(Default)]
pub struct FileViewers {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    docs: Vec<Doc>,
    /// Files to open (or focus and reload, if already open) at the end of the
    /// main window's next pass. Queued rather than opened directly so the
    /// Files panel and links clicked in a viewer window share one path.
    /// `true` opens it in edit mode.
    to_open: Vec<(PathBuf, bool)>,
    /// Where a viewer window last moved or resized to: new ones open there.
    /// Saved between runs.
    placement: Option<Placement>,
    /// One Markdown cache for every window: building it loads syntect's
    /// syntax and theme sets, so it's created on the first Markdown open, not
    /// at startup.
    cache: Option<CommonMarkCache>,
    /// Source of unique viewport ids.
    next_id: u64,
    /// [TOGGLE_BOOKMARK] was pressed in a viewer window, for the main
    /// window's next pass to act on (see [FileViewers::take_bookmark_key]).
    bookmark_key: bool,
    /// The main window was closed while a viewer still has unsaved edits.
    quit_pending: bool,
}

/// One viewer window.
struct Doc {
    id: ViewportId,
    path: PathBuf,
    kind: Kind,
    /// The file's contents, or why they couldn't be read.
    content: Result<Content, String>,
    /// The file's size, when `content` holds only the start of it.
    truncated: Option<u64>,
    /// Markdown `content` split at its `<details>` blocks; see
    /// [markdown::split_details]. Empty for plain text and images.
    blocks: Vec<Block>,
    /// Which of those `<details>` sections are open, by index.
    details_open: Vec<bool>,
    /// Source or plain text laid out (source highlighted by a background
    /// task); invalidated on reload.
    layout: syntax::Source,
    /// How many lines text `content` has: what the line numbers count up to.
    lines: usize,
    /// An image being decoded on a background thread.
    decoding: Option<Decoding>,
    zoom: Zoom,
    /// Modification time of the file as last read, to notice edits.
    modified: Option<SystemTime>,
    last_poll: Instant,
    /// Cleared when the user closes the window; the main pass then drops it.
    open: bool,
    /// A one-off message in the toolbar, e.g. a link whose target is missing.
    notice: Option<String>,
    /// Find, for text: the toolbar's second row.
    search: search::Search,
    /// Present only in edit mode; automatic reloads leave it alone.
    editor: Option<editor::Editor>,
    /// What Tab inserts in edit mode, picked in the toolbar: a tab, or 4
    /// spaces as the window opens.
    tab: ScratchpadTab,
    /// An action waiting for Save / Discard / Cancel on an unsaved draft.
    pending: Option<Action>,
    selection: selection::Markdown,
    /// Where the window opened. Fixed: egui moves a window whose builder
    /// changes.
    opened_at: Option<Placement>,
    /// Where the window was last seen, to notice it moving or resizing.
    placement: Option<Placement>,
}

#[derive(Clone, Copy)]
enum Action {
    View,
    Reload,
    Close,
    OpenExternal,
}

/// What a window shows.
enum Content {
    /// Markdown, source or plain text: at most [MAX_BYTES] of the file.
    Text(Arc<str>),
    Image(Picture),
    /// An image whose first decode hasn't finished yet.
    Loading,
}

/// An image decode running on a background thread; see [spawn_decode].
struct Decoding {
    rx: Receiver<Result<Decoded, String>>,
    /// Set when the file changed, or the window moved on to another image,
    /// after this decode began: its result is dropped and the current file
    /// decoded afresh.
    stale: bool,
}

/// An image, uploaded for drawing.
struct Picture {
    texture: TextureHandle,
    /// The image's own size in pixels. Bigger than the texture when that had
    /// to be scaled down to the GPU's largest texture size.
    size: [u32; 2],
}

/// How an image window is zoomed.
#[derive(Default)]
struct Zoom {
    /// Screen pixels per image pixel, as the user set it. `None` fits the
    /// image to the window: shrunk if it's bigger, actual size if not.
    set: Option<f32>,
    /// The scale the image was last drawn at, for the toolbar.
    shown: f32,
}

impl Doc {
    fn new(ctx: &Context, id: ViewportId, path: PathBuf, kind: Kind) -> Self {
        let mut doc = Self {
            id,
            path,
            kind,
            content: Ok(Content::Loading),
            truncated: None,
            blocks: Vec::new(),
            details_open: Vec::new(),
            layout: syntax::Source::default(),
            lines: 0,
            decoding: None,
            zoom: Zoom::default(),
            modified: None,
            last_poll: Instant::now(),
            open: true,
            notice: None,
            search: search::Search::default(),
            editor: None,
            tab: ScratchpadTab::Spaces,
            pending: None,
            selection: selection::Markdown::default(),
            opened_at: None,
            placement: None,
        };
        doc.reload(ctx);
        doc
    }

    fn reload(&mut self, ctx: &Context) {
        self.modified = modified_time(&self.path);
        self.last_poll = Instant::now();
        if self.kind == Kind::Image {
            // The current picture stays up until the new one is ready. One
            // decode at a time, so holding an arrow key over big photos
            // doesn't pile up threads: one already running is redone when
            // it finishes.
            match &mut self.decoding {
                Some(decoding) => decoding.stale = true,
                None => self.start_decode(ctx),
            }
            return;
        }
        let read = read_text(&self.path);
        self.selection = selection::Markdown::default();
        self.layout.clear();
        self.truncated = read.as_ref().ok().and_then(|(_, size)| *size);
        self.lines = read.as_ref().map_or(0, |(text, _)| {
            text.bytes().filter(|&b| b == b'\n').count() + 1
        });
        self.blocks = match (&read, self.kind) {
            (Ok((text, _)), Kind::Markdown) => markdown::split_details(text),
            _ => Vec::new(),
        };
        self.content = read
            .map(|(text, _)| Content::Text(text.into()))
            .map_err(|e| format!("Couldn't read {}: {e}", self.path.display()));
    }

    /// Re-read the file if its modification time changed since it was read.
    /// Polled at most once per [RELOAD_POLL].
    fn reload_if_changed(&mut self, ctx: &Context) {
        if self.editor.is_some() {
            return;
        }
        if self.last_poll.elapsed() < RELOAD_POLL {
            return;
        }
        self.last_poll = Instant::now();
        if modified_time(&self.path) != self.modified {
            self.reload(ctx);
        }
    }

    fn start_decode(&mut self, ctx: &Context) {
        let max_side = ctx.input(|i| i.max_texture_side) as u32;
        self.decoding = Some(Decoding {
            rx: spawn_decode(ctx, self.id, self.path.clone(), max_side),
            stale: false,
        });
    }

    /// Show the image from [spawn_decode], once it's ready.
    fn receive_image(&mut self, ctx: &Context) {
        let Some(decoding) = &self.decoding else {
            return;
        };
        let decoded = match decoding.rx.try_recv() {
            Ok(decoded) => decoded,
            Err(TryRecvError::Empty) => return,
            // The decoder panicked.
            Err(TryRecvError::Disconnected) => {
                Err(format!("Couldn't decode {}", self.path.display()))
            }
        };
        if decoding.stale {
            self.start_decode(ctx);
            return;
        }
        self.decoding = None;
        self.content = decoded.map(|d| {
            Content::Image(Picture {
                texture: ctx.load_texture(self.path.to_string_lossy(), d.pixels, IMAGE_TEXTURE),
                size: d.size,
            })
        });
    }

    /// Show the image `step` places after this one in its folder (before
    /// it, if negative); see [neighbor_image].
    fn navigate(&mut self, ctx: &Context, step: isize) {
        let Some(path) = neighbor_image(&self.path, step) else {
            return;
        };
        if path == self.path {
            return;
        }
        self.path = path;
        self.zoom = Zoom::default();
        self.notice = None;
        self.reload(ctx);
        // The main window's pass sets titles too, but it may be idle.
        ctx.send_viewport_cmd_to(self.id, ViewportCommand::Title(self.title()));
    }

    fn title(&self) -> String {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string());
        let changed = if self.dirty() { " *" } else { "" };
        let mode = if self.editor.is_some() {
            "Edit"
        } else {
            self.kind.name()
        };
        format!("{name}{changed} - {mode}")
    }

    fn dirty(&self) -> bool {
        self.editor.as_ref().is_some_and(editor::Editor::dirty)
    }

    /// Whether edit mode is available: the whole of a text file is loaded.
    fn can_edit(&self) -> bool {
        self.kind != Kind::Image
            && self.truncated.is_none()
            && matches!(self.content, Ok(Content::Text(_)))
    }

    fn start_editing(&mut self, ctx: &Context) {
        match editor::Editor::load(&self.path) {
            Ok(editor) => {
                self.editor = Some(editor);
                editor::Editor::reset_state(ctx, self.id);
                self.notice = None;
            }
            Err(e) => self.notice = Some(e),
        }
        ctx.request_repaint();
    }

    fn save(&mut self, ctx: &Context) -> bool {
        let Some(editor) = &mut self.editor else {
            return true;
        };
        match editor.save(&self.path) {
            Ok(()) => {
                self.reload(ctx);
                self.notice = None;
                ctx.memory_mut(|m| m.request_focus(editor::id(self.id)));
                ctx.request_repaint();
                true
            }
            Err(e) => {
                self.notice = Some(e);
                false
            }
        }
    }

    fn request(&mut self, ctx: &Context, action: Action) {
        if self.dirty() {
            self.pending = Some(action);
        } else {
            self.perform(ctx, action);
        }
    }

    fn perform(&mut self, ctx: &Context, action: Action) {
        self.pending = None;
        match action {
            Action::View => {
                self.editor = None;
                self.reload(ctx);
                self.notice = None;
            }
            Action::Reload => {
                let editing = self.editor.take().is_some();
                self.reload(ctx);
                self.notice = None;
                if editing {
                    self.start_editing(ctx);
                }
            }
            Action::Close => self.open = false,
            Action::OpenExternal => match open_with_default_app(&self.path) {
                Ok(()) => self.open = false,
                Err(e) => self.notice = Some(format!("Couldn't open: {e}")),
            },
        }
        ctx.request_repaint();
        ctx.request_repaint_of(ViewportId::ROOT);
    }

    fn confirm_pending(&mut self, ctx: &Context) {
        let Some(action) = self.pending else {
            return;
        };
        let mut save = false;
        let mut discard = false;
        let mut cancel = false;
        let modal = Modal::new(Id::new(("viewer_unsaved", self.id))).show(ctx, |ui| {
            ui.set_max_width(420.);
            ui.heading("Save changes?");
            ui.label(format!(
                "{} has unsaved edits.",
                self.path.file_name().unwrap_or_default().to_string_lossy()
            ));
            if let Some(error) = &self.notice {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            ui.horizontal(|ui| {
                save = ui.button("Save").clicked();
                discard = ui.button("Discard edits").clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if discard || (save && self.save(ctx)) {
            self.perform(ctx, action);
        } else if cancel || modal.should_close() {
            self.pending = None;
            ctx.memory_mut(|m| m.request_focus(editor::id(self.id)));
        }
    }
}

fn modified_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The image `step` places after `path` in its folder (before it, if
/// negative), in the Files panel's order: by name, ignoring case. Wraps
/// around at the ends. `path` itself needn't still exist.
fn neighbor_image(path: &Path, step: isize) -> Option<PathBuf> {
    let key = |p: &Path| {
        let name = p.file_name().unwrap_or_default().to_string_lossy();
        // Then by exact name, so names differing only in case keep an order.
        (name.to_lowercase(), name.into_owned())
    };
    let mut images: Vec<PathBuf> = fs::read_dir(path.parent()?)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_dir()))
        .map(|e| e.path())
        .filter(|p| is_image(p))
        .collect();
    if images.is_empty() {
        return None;
    }
    images.sort_by_cached_key(|p| key(p));
    let target = match images.binary_search_by_key(&key(path), |p| key(p)) {
        Ok(i) => i as isize + step,
        // Gone from the folder: `i` is where it was.
        Err(i) => i as isize + step - isize::from(step > 0),
    };
    let i = target.rem_euclid(images.len() as isize) as usize;
    Some(images.swap_remove(i))
}

/// Read the text file at `path`. A file bigger than [MAX_BYTES] is cut at
/// the last line break within that, and its size returned alongside.
fn read_text(path: &Path) -> io::Result<(String, Option<u64>)> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 <= MAX_BYTES {
        return Ok((decode(&bytes), None));
    }
    bytes.truncate(MAX_BYTES as usize);
    let mut text = decode(&bytes);
    // Drop the partial last line, and with it any character cut in two.
    if let Some(end) = text.rfind('\n') {
        text.truncate(end + 1);
    }
    let size = fs::metadata(path).map_or(MAX_BYTES, |m| m.len());
    Ok((text, Some(size)))
}

/// A text file's contents: UTF-16 if it starts with a UTF-16 byte order mark
/// (as Windows PowerShell's `>` writes), otherwise UTF-8. Lossy, so a stray
/// bad byte doesn't hide the whole file.
fn decode(bytes: &[u8]) -> String {
    let utf16 = |rest: &[u8], from_bytes: fn([u8; 2]) -> u16| {
        let units = rest.as_chunks::<2>().0.iter().map(|&c| from_bytes(c));
        char::decode_utf16(units)
            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect()
    };
    match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes),
        _ => {
            let text = String::from_utf8_lossy(bytes);
            text.strip_prefix('\u{feff}').unwrap_or(&text).to_string()
        }
    }
}

/// An image decoded by [decode_image], ready to upload.
struct Decoded {
    pixels: ColorImage,
    /// The image's own size, before any scaling down.
    size: [u32; 2],
}

/// Decode the image at `path` on a background thread, scaled down if a side
/// is longer than `max_side`, then repaint window `id` to show it: a big
/// photo takes long enough to stall every window.
fn spawn_decode(
    ctx: &Context,
    id: ViewportId,
    path: PathBuf,
    max_side: u32,
) -> Receiver<Result<Decoded, String>> {
    let (tx, rx) = mpsc::channel();
    let ctx = ctx.clone();
    thread::spawn(move || {
        let decoded = decode_image(&path, max_side)
            .map_err(|e| format!("Couldn't read {}: {e}", path.display()));
        // Fails if the window closed meanwhile: nobody's waiting.
        if tx.send(decoded).is_ok() {
            ctx.request_repaint_of(id);
        }
    });
    rx
}

/// Decode the image at `path`, its format sniffed from the contents rather
/// than the extension. It's turned upright per its EXIF orientation, as phone
/// photos need, and scaled down if a side is longer than `max_side`.
fn decode_image(path: &Path, max_side: u32) -> ImageResult<Decoded> {
    let mut decoder = ImageReader::open(path)?
        .with_guessed_format()?
        .into_decoder()?;
    // A malformed EXIF block isn't worth refusing the picture over.
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    let size = [image.width(), image.height()];
    if size[0].max(size[1]) > max_side {
        image = image.resize(max_side, max_side, FilterType::Triangle);
    }
    let rgba = image.into_rgba8();
    let pixels = ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    );
    Ok(Decoded { pixels, size })
}

/// The Files panel's preview of the selected image, under its details.
/// Decoded in the background like a viewer window's image, but scaled down
/// to at most [PREVIEW_MAX_SIDE].
#[derive(Default)]
pub struct ImagePreview {
    /// The image previewed, and its modification time as selected, so an
    /// edit (which re-reads the selection's details) re-reads it too. `None`
    /// when the selection isn't an image.
    file: Option<(PathBuf, Option<SystemTime>)>,
    /// `file`'s picture, or why it couldn't be read. `None` until decoded.
    picture: Option<Result<Picture, String>>,
    decoding: Option<Decoding>,
}

impl ImagePreview {
    /// Preview `file`, an image's path and modification time, or nothing.
    /// Call every pass: a new file starts decoding, and a finished decode is
    /// picked up.
    pub fn set(&mut self, ctx: &Context, file: Option<(&Path, Option<SystemTime>)>) {
        let current = self.file.as_ref().map(|(p, m)| (p.as_path(), *m));
        if current != file {
            self.file = file.map(|(p, m)| (p.to_path_buf(), m));
            self.picture = None;
            // One decode at a time, as in a viewer window, so arrowing
            // through a folder of photos doesn't pile up threads: one already
            // running is redone when it finishes.
            match &mut self.decoding {
                Some(decoding) => decoding.stale = true,
                None => self.start_decode(ctx),
            }
        }
        self.receive(ctx);
    }

    fn start_decode(&mut self, ctx: &Context) {
        let Some((path, _)) = &self.file else {
            return;
        };
        let max_side = ctx.input(|i| i.max_texture_side) as u32;
        let rx = spawn_decode(
            ctx,
            ctx.viewport_id(),
            path.clone(),
            max_side.min(PREVIEW_MAX_SIDE),
        );
        self.decoding = Some(Decoding { rx, stale: false });
    }

    /// Show the picture from [spawn_decode], once it's ready.
    fn receive(&mut self, ctx: &Context) {
        let Some(decoding) = &self.decoding else {
            return;
        };
        let decoded = match decoding.rx.try_recv() {
            Ok(decoded) => decoded,
            Err(TryRecvError::Empty) => return,
            // The decoder panicked.
            Err(TryRecvError::Disconnected) => Err("Couldn't decode the image".to_owned()),
        };
        let stale = decoding.stale;
        self.decoding = None;
        if stale {
            self.start_decode(ctx);
            return;
        }
        self.picture = Some(decoded.map(|d| Picture {
            texture: ctx.load_texture("file_preview", d.pixels, IMAGE_TEXTURE),
            size: d.size,
        }));
    }

    /// The previewed image's size in pixels, once it's decoded.
    pub fn size(&self) -> Option<[u32; 2]> {
        match &self.picture {
            Some(Ok(picture)) => Some(picture.size),
            _ => None,
        }
    }

    /// Draw the preview in all the space left in `ui`: as big as fits
    /// without changing its shape, across the middle and at the top. A
    /// spinner while it decodes.
    pub fn show(&self, ui: &mut Ui) {
        if self.file.is_none() {
            return;
        }
        let picture = match &self.picture {
            Some(Ok(picture)) => picture,
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
                return;
            }
            None => {
                ui.spinner();
                return;
            }
        };
        let space = ui.available_rect_before_wrap();
        let size = vec2(picture.size[0] as f32, picture.size[1] as f32);
        let scale = (space.width() / size.x).min(space.height() / size.y);
        if !(scale > 0.) {
            return;
        }
        let size = size * scale;
        let min = pos2(space.center().x - size.x / 2., space.top());
        let rect = Rect::from_min_size(min, size).round_to_pixels(ui.ctx().pixels_per_point());
        ui.allocate_rect(space, Sense::hover());
        let uv = Rect::from_min_max(pos2(0., 0.), pos2(1., 1.));
        ui.painter()
            .image(picture.texture.id(), rect, uv, Color32::WHITE);
    }
}

impl FileViewers {
    /// Open `path` in a viewer window, or focus and reload its window if it's
    /// already open. Takes effect in [Self::show], later this pass.
    pub fn open(&self, path: PathBuf) {
        self.lock().to_open.push((path, false));
    }

    /// As [Self::open], in edit mode when the file allows it.
    pub fn edit(&self, path: PathBuf) {
        self.lock().to_open.push((path, true));
    }

    /// Keep each open window alive. Call once per pass of the main window:
    /// egui closes any viewport that isn't shown in its parent's pass.
    pub fn show(&self, ctx: &Context) {
        let windows: Vec<(ViewportId, String, Option<Placement>)> = {
            let mut inner = self.lock();
            for (path, edit) in std::mem::take(&mut inner.to_open) {
                inner.open_now(ctx, path, edit);
            }
            inner.docs.retain(|d| d.open);
            if ctx.input(|i| i.viewport().close_requested()) && inner.docs.iter().any(Doc::dirty) {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                inner.quit_pending = true;
            }
            inner.confirm_quit(ctx);
            inner
                .docs
                .iter()
                .map(|d| (d.id, d.title(), d.opened_at))
                .collect()
        };

        // The lock is released first: without native multi-window support
        // egui runs the callback right here, embedded in the main window.
        for (id, title, opened_at) in windows {
            let mut builder = ViewportBuilder::default()
                .with_title(title)
                .with_inner_size(WINDOW_SIZE)
                .with_min_inner_size(MIN_WINDOW_SIZE)
                .with_icon(crate::app_icon());
            if let Some(p) = opened_at {
                builder = builder
                    .with_position(p.pos)
                    .with_inner_size(p.size.max(MIN_REMEMBERED_SIZE));
            }
            let inner = Arc::clone(&self.inner);
            ctx.show_viewport_deferred(id, builder, move |ui, _class| {
                lock(&inner).show_window(ui, id);
            });
        }
    }

    /// Whether [TOGGLE_BOOKMARK] was pressed in a viewer window since last
    /// asked, so it works from these windows as from the main one.
    pub fn take_bookmark_key(&self) -> bool {
        std::mem::take(&mut self.lock().bookmark_key)
    }

    /// Where new windows open: where one was last moved or resized to.
    pub fn placement(&self) -> Option<Placement> {
        self.lock().placement
    }

    /// Set where new windows open, e.g. as saved by the last run.
    pub fn set_placement(&self, placement: Option<Placement>) {
        self.lock().placement = placement;
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
    }
}

/// A panic mid-render can't leave [Inner] half-updated in a way that matters,
/// so recover from poisoning rather than taking every window down with it.
fn lock(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(|e| e.into_inner())
}

impl Inner {
    fn confirm_quit(&mut self, ctx: &Context) {
        if !self.quit_pending {
            return;
        }
        let mut save = false;
        let mut discard = false;
        let mut cancel = false;
        let modal = Modal::new(Id::new("viewer_unsaved_quit")).show(ctx, |ui| {
            ui.set_max_width(480.);
            ui.heading("Save changes before quitting?");
            for doc in self.docs.iter().filter(|d| d.dirty()) {
                ui.label(doc.path.display().to_string());
                if let Some(error) = &doc.notice {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
            }
            ui.horizontal(|ui| {
                save = ui.button("Save all").clicked();
                discard = ui.button("Discard edits and quit").clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if save {
            let mut saved = true;
            for doc in self.docs.iter_mut().filter(|d| d.dirty()) {
                saved &= doc.save(ctx);
            }
            if !saved {
                return;
            }
        }
        if save || discard {
            for doc in &mut self.docs {
                doc.editor = None;
            }
            self.quit_pending = false;
            ctx.send_viewport_cmd(ViewportCommand::Close);
        } else if cancel || modal.should_close() {
            self.quit_pending = false;
        }
    }

    fn open_now(&mut self, ctx: &Context, path: PathBuf, edit: bool) {
        let kind = Kind::of(&path);
        // Compare canonical paths so `./README.md` and `docs/../README.md`
        // find the same window.
        let path = fs::canonicalize(&path).map(strip_verbatim).unwrap_or(path);
        if let Some(doc) = self.docs.iter_mut().find(|d| d.path == path && d.open) {
            if doc.editor.is_none() {
                doc.reload(ctx);
                if edit && doc.can_edit() {
                    doc.start_editing(ctx);
                }
            }
            ctx.send_viewport_cmd_to(doc.id, ViewportCommand::Focus);
            return;
        }
        let id = ViewportId::from_hash_of(("file_viewer", self.next_id));
        self.next_id += 1;
        let mut doc = Doc::new(ctx, id, path, kind);
        doc.opened_at = self.placement.map(|p| p.on_screen(ctx));
        if edit && doc.can_edit() {
            doc.start_editing(ctx);
        }
        self.docs.push(doc);
    }

    /// Render the window for viewport `id`. Runs in that viewport's own pass.
    fn show_window(&mut self, ui: &mut Ui, id: ViewportId) {
        let Self {
            docs,
            to_open,
            cache,
            bookmark_key,
            placement,
            ..
        } = self;
        let Some(doc) = docs.iter_mut().find(|d| d.id == id) else {
            return;
        };
        let ctx = ui.ctx().clone();
        let title = doc.title();

        // Only a change counts, so the last window moved or resized wins,
        // not whichever happens to draw last.
        if let Some(now) = Placement::of(&ctx)
            && doc.placement != Some(now)
        {
            doc.placement = Some(now);
            *placement = Some(now);
        }

        // The main window acts on it, in its next pass.
        if take_shortcut(&ctx, TOGGLE_BOOKMARK) {
            *bookmark_key = true;
            ctx.request_repaint_of(ViewportId::ROOT);
        }

        if ctx.input(|i| i.viewport().close_requested()) {
            if doc.dirty() {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            }
            doc.request(&ctx, Action::Close);
            if !doc.open {
                return;
            }
        }

        if doc.pending.is_none() && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F5)) {
            doc.request(&ctx, Action::Reload);
        } else {
            doc.reload_if_changed(&ctx);
        }
        let save_key = doc.editor.is_some()
            && doc.pending.is_none()
            && ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::S));
        if doc.kind == Kind::Image {
            let step = ctx.input_mut(|i| {
                isize::from(i.consume_key(Modifiers::NONE, Key::ArrowRight))
                    - isize::from(i.consume_key(Modifiers::NONE, Key::ArrowLeft))
            });
            if step != 0 {
                doc.navigate(&ctx, step);
            }
        }
        if doc.kind != Kind::Image && doc.pending.is_none() {
            doc.search.keys(&ctx);
        }
        doc.receive_image(&ctx);
        // Keep polling for edits while the window is idle.
        ctx.request_repaint_after(RELOAD_POLL);

        // Panel and scroll state live in context-wide memory, so salt their
        // ids with the viewport's: each window keeps its own scroll position.
        Panel::top(Id::new(("viewer_toolbar", id)))
            .resizable(false)
            .show(ui, |ui| toolbar(doc, ui, id));

        let mut margin = Margin::symmetric(16, 8);
        if matches!(doc.kind, Kind::Code(_) | Kind::Text) || doc.editor.is_some() {
            // The line numbers' gutter starts at the window's edge.
            margin.left = 0;
        }
        CentralPanel::default()
            .frame(Frame::central_panel(ui.style()).inner_margin(margin))
            .show(ui, |ui| {
                if let Some(editor) = &mut doc.editor {
                    editor.show(ui, doc.kind, doc.tab, &mut doc.search, id);
                    return;
                }
                match &doc.content {
                    Ok(Content::Text(text)) if doc.kind == Kind::Markdown => {
                        ScrollArea::vertical()
                            .id_salt(("viewer_scroll", id))
                            .animated(false)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                let cache = cache.get_or_insert_with(CommonMarkCache::default);
                                let top_left = ui.max_rect().min;
                                let width = ui.available_width();
                                let mut details = Details::Shown(&mut doc.details_open);
                                // Selection must survive labels scrolling off
                                // screen; the viewer keeps its own endpoints.
                                ui.style_mut().interaction.selectable_labels = false;
                                markdown::show(
                                    ui,
                                    text,
                                    &doc.blocks,
                                    &doc.path,
                                    cache,
                                    id,
                                    &mut details,
                                );
                                let layout = search::Layout::markdown(
                                    text,
                                    width,
                                    ui.min_rect().height(),
                                    &doc.details_open,
                                );
                                doc.selection.show(ui, top_left, layout, id, |ui| {
                                    let mut details = Details::Measured(&doc.details_open);
                                    markdown::show(
                                        ui,
                                        text,
                                        &doc.blocks,
                                        &doc.path,
                                        cache,
                                        id,
                                        &mut details,
                                    );
                                });
                                let layout = search::Layout::markdown(
                                    text,
                                    width,
                                    ui.min_rect().height(),
                                    &doc.details_open,
                                );
                                doc.search.markdown(ui, top_left, layout, id, |ui| {
                                    let mut details = Details::Measured(&doc.details_open);
                                    markdown::show(
                                        ui,
                                        text,
                                        &doc.blocks,
                                        &doc.path,
                                        cache,
                                        id,
                                        &mut details,
                                    );
                                });
                            });
                    }
                    Ok(Content::Text(text)) => {
                        let (kind, lines) = (doc.kind, doc.lines);
                        show_text(ui, text, kind, lines, &mut doc.layout, &mut doc.search, id);
                    }
                    Ok(Content::Image(picture)) => show_image(ui, picture, &mut doc.zoom, id),
                    Ok(Content::Loading) => {
                        ui.spinner();
                    }
                    Err(e) => {
                        ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
                    }
                }
            });

        if save_key && doc.editor.is_some() && doc.pending.is_none() {
            doc.save(&ctx);
        }
        if doc.kind == Kind::Markdown && doc.editor.is_none() {
            let base_dir = doc.path.parent().unwrap_or(Path::new(""));
            for path in markdown::take_local_links(&ctx, base_dir) {
                open_link(doc, path, to_open, &ctx);
            }
        }
        doc.confirm_pending(&ctx);
        let new_title = doc.title();
        if new_title != title {
            ctx.send_viewport_cmd(ViewportCommand::Title(new_title));
        }
    }
}

/// Show source or plain text, its `lines` lines numbered down its left like
/// an editor's, in viewer window `window`, with `search`'s matches
/// highlighted. Source scrolls sideways; plain text wraps to the window
/// instead.
fn show_text(
    ui: &mut Ui,
    text: &Arc<str>,
    kind: Kind,
    lines: usize,
    layout: &mut syntax::Source,
    search: &mut search::Search,
    window: ViewportId,
) {
    search.find_in_text(ui.ctx(), text);
    let language = match kind {
        Kind::Code(language) => Some(language),
        _ => None,
    };
    let scroll = match language {
        Some(_) => ScrollArea::both(),
        None => ScrollArea::vertical(),
    };
    let space = ui.available_rect_before_wrap();
    let (gutter, body) = space.split_left_right_at_x(space.left() + gutter::width(ui, lines));
    let output = ui
        .scope_builder(UiBuilder::new().max_rect(body), |ui| {
            scroll
                .id_salt(("viewer_scroll", window))
                .animated(false)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let wrap_width = match language {
                        Some(_) => f32::INFINITY,
                        None => ui.available_width(),
                    };
                    // Highlights go under the text, so they're drawn first.
                    let behind = ui.painter().add(Shape::Noop);
                    let mut buffer = text.as_ref();
                    let mut layouter = |ui: &Ui, _: &dyn egui::TextBuffer, _| {
                        layout.galley(ui, text, language, wrap_width)
                    };
                    // An immutable TextBuffer keeps the current view read-only
                    // while retaining a selection outside the visible rows.
                    let output = TextEdit::multiline(&mut buffer)
                        .id(Id::new(("viewer_text", window)))
                        .font(TextStyle::Monospace)
                        .cursor_at_end(false)
                        .desired_width(f32::INFINITY)
                        .min_size(ui.available_size())
                        .margin(0)
                        .frame(Frame::NONE)
                        .layouter(&mut layouter)
                        .show(ui);
                    selection::text(ui, &output.response, language.is_some());
                    let (galley, pos) = (output.galley, output.galley_pos);
                    search.paint_text(ui, &galley, pos, behind);
                    (galley, pos)
                })
        })
        .inner;
    let (galley, pos) = output.inner;
    // Level with the text, not the sideways scroll bar under it.
    let gutter = Rect::from_x_y_ranges(gutter.x_range(), output.inner_rect.y_range());
    gutter::paint(ui, gutter, &galley, pos);
}

/// Path, notices, and the Reload / Open externally buttons, plus previous /
/// next and zoom controls for an image, or the find bar under them for text.
/// In viewer window `window`.
fn toolbar(doc: &mut Doc, ui: &mut Ui, window: ViewportId) {
    ui.horizontal_wrapped(|ui| {
        if doc.kind != Kind::Image {
            let editing = doc.editor.is_some();
            if ui
                .add_enabled(editing, Button::selectable(!editing, "View"))
                .clicked()
            {
                doc.request(ui.ctx(), Action::View);
            }
            if ui
                .add_enabled(
                    !editing && doc.can_edit(),
                    Button::selectable(editing, "Edit"),
                )
                .on_hover_text("Edit the complete text file; files open in read-only View mode")
                .clicked()
            {
                doc.start_editing(ui.ctx());
            }
            if editing {
                if ui
                    .add_enabled(doc.dirty(), Button::new("Save"))
                    .on_hover_text("Save changes (Ctrl+S; Cmd+S on Mac)")
                    .clicked()
                {
                    doc.save(ui.ctx());
                }
                let (label, hover) = if doc.dirty() {
                    ("Discard edits", "Discard the draft and return to View mode")
                } else {
                    ("View only mode", "Return to View mode")
                };
                if ui.button(label).on_hover_text(hover).clicked() {
                    doc.perform(ui.ctx(), Action::View);
                }
                if doc.dirty() {
                    ui.colored_label(ui.visuals().warn_fg_color, "Unsaved edits");
                }
                ui.separator();
                // Tab and 4 spaces, not Focus term: the CLI input is in the
                // main window.
                let modes = &scratchpad::TAB_MODES[..2];
                if let Some(tab) = scratchpad::tab_mode_control(ui, doc.tab, modes) {
                    doc.tab = tab;
                    // Back to the text, from the button.
                    ui.ctx().memory_mut(|m| m.request_focus(editor::id(window)));
                }
            }
            ui.separator();
        }
        if doc.kind == Kind::Image {
            if ui
                .button("⬅")
                .on_hover_text("Previous image in this folder (Left arrow)")
                .clicked()
            {
                doc.navigate(ui.ctx(), -1);
            }
            if ui
                .button("➡")
                .on_hover_text("Next image in this folder (Right arrow)")
                .clicked()
            {
                doc.navigate(ui.ctx(), 1);
            }
            ui.separator();
        }
        if ui
            .button("⟳ Reload")
            .on_hover_text("Re-read the file (F5)")
            .clicked()
        {
            doc.request(ui.ctx(), Action::Reload);
        }
        if ui
            .button("Open with OS default")
            .on_hover_text("Open with the default program for this file type")
            .clicked()
        {
            doc.request(ui.ctx(), Action::OpenExternal);
        }
        if let Ok(Content::Image(picture)) = &doc.content {
            zoom_controls(ui, picture, &mut doc.zoom);
        }
        ui.label(RichText::new(doc.path.display().to_string()).weak());
    });
    if doc.kind != Kind::Image {
        doc.search.bar(ui, window);
    }
    if let Some(size) = doc.truncated {
        let msg = format!(
            "Showing the first {} MB of {}. Open externally to see it all.",
            MAX_BYTES >> 20,
            format_size(size)
        );
        ui.label(RichText::new(msg).color(ui.visuals().warn_fg_color));
    }
    if let Ok(Content::Image(picture)) = &doc.content {
        let [w, h] = picture.texture.size();
        if [w as u32, h as u32] != picture.size {
            let msg = format!(
                "Showing at {w} × {h}, the largest image the graphics card takes. \
                 Open with the default application to see it all."
            );
            ui.label(RichText::new(msg).color(ui.visuals().warn_fg_color));
        }
    }
    if let Some(notice) = &doc.notice {
        ui.label(RichText::new(notice).color(ui.visuals().error_fg_color));
    }
}

/// Fit / 100% toggles, and the image's size and zoom.
fn zoom_controls(ui: &mut Ui, picture: &Picture, zoom: &mut Zoom) {
    ui.separator();
    if ui
        .selectable_label(zoom.set.is_none(), "Fit")
        .on_hover_text("Shrink to fit the window. Ctrl+scroll to zoom, drag to pan")
        .clicked()
    {
        zoom.set = None;
    }
    if ui
        .selectable_label(zoom.set == Some(1.), "100%")
        .on_hover_text("Actual size: one image pixel per screen pixel")
        .clicked()
    {
        zoom.set = Some(1.);
    }
    let [w, h] = picture.size;
    ui.label(format!("{w} × {h} at {:.0}%", zoom.shown * 100.));
    ui.separator();
}

/// Draw `picture` centred in viewer window `window`, at `zoom`. Ctrl+scroll
/// (or pinch) zooms about the pointer; scrolling or dragging pans.
fn show_image(ui: &mut Ui, picture: &Picture, zoom: &mut Zoom, window: ViewportId) {
    let ppp = ui.ctx().pixels_per_point();
    // Its size in points at 100%: one image pixel per screen pixel.
    let actual = vec2(picture.size[0] as f32, picture.size[1] as f32) / ppp;
    let viewport = ui.available_rect_before_wrap();
    let fit = (viewport.width() / actual.x)
        .min(viewport.height() / actual.y)
        .min(1.);
    let mut scale = zoom.set.unwrap_or(fit);

    let scroll_salt = ("viewer_scroll", window);
    let mut scroll = ScrollArea::both()
        .id_salt(scroll_salt)
        .auto_shrink([false, false]);
    let factor = ui.input(|i| i.zoom_delta());

    let pointer = ui
        .ctx()
        .pointer_hover_pos()
        .filter(|p| viewport.contains(*p));

    if let Some(pointer) = pointer
        && factor != 1.
    {
        let new_scale = (scale * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        // Scroll so the image point under the pointer stays there.
        let offset = scroll_area::State::load(ui.ctx(), ui.make_persistent_id(scroll_salt))
            .map_or(Vec2::ZERO, |s| s.offset);

        let at = pointer - viewport.min;
        let point = (offset + at - padding(actual * scale, viewport.size())) / scale;
        let offset = point * new_scale + padding(actual * new_scale, viewport.size()) - at;

        scroll = scroll.scroll_offset(offset.max(Vec2::ZERO));
        scale = new_scale;
        zoom.set = Some(new_scale);
    }

    if zoom.shown != scale {
        zoom.shown = scale;
        // For the toolbar, already drawn this pass.
        ui.ctx().request_repaint();
    }

    let size = actual * scale;
    // Fitted, the image is never bigger than the viewport, so float error
    // mustn't add scroll bars.
    let content = match zoom.set {
        Some(_) => size.max(viewport.size()),
        None => viewport.size(),
    };

    scroll.show(ui, |ui| {
        let (rect, _) = ui.allocate_exact_size(content, Sense::hover());
        let image_rect = Rect::from_center_size(rect.center(), size).round_to_pixels(ppp);
        let uv = Rect::from_min_max(pos2(0., 0.), pos2(1., 1.));
        ui.painter()
            .image(picture.texture.id(), image_rect, uv, Color32::WHITE);
    });
}

/// The margin around an image of `size` centred in a viewport of
/// `viewport`: none along an axis where it overflows.
fn padding(size: Vec2, viewport: Vec2) -> Vec2 {
    ((viewport - size) / 2.).max(Vec2::ZERO)
}

/// Open `path`, a local file linked from `doc`: in another viewer window if
/// it's a type the viewer shows, otherwise (e.g. a folder) with its
/// default program.
fn open_link(doc: &mut Doc, path: PathBuf, to_open: &mut Vec<(PathBuf, bool)>, ctx: &Context) {
    if !path.exists() {
        doc.notice = Some(format!("Link target not found: {}", path.display()));
    } else if can_view(&path) {
        to_open.push((path, false));
        // The main window's pass is what opens it.
        ctx.request_repaint_of(ViewportId::ROOT);
    } else if let Err(e) = open_with_default_app(&path) {
        doc.notice = Some(format!("Couldn't open {}: {e}", path.display()));
    }
}

/// `fs::canonicalize` on Windows returns `\\?\C:\...` paths, which display
/// badly and which some programs refuse. Drop the prefix when it's a plain
/// drive path.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}
