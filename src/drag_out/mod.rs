//! Dragging Files rows out of the window: onto Explorer, the desktop, a web
//! page's upload box, an editor, or any other program that takes files
//! dropped from a file manager.
//!
//! This uses OS-specific APIs: One for Windows, another for Linux/X11, and another for Linux/Wayland.

/// What became of a drag handed to the OS.
#[derive(Debug)]
pub enum Outcome {
    /// It's over: dropped on another program (`dropped`), or called off,
    /// with Escape or by letting go where nothing takes files.
    Ended { dropped: bool },
    /// The pointer came back over the window, so the drag carries on there.
    /// If the button was let go of meanwhile, egui is told, so it's dropped
    /// where the pointer is. (On Wayland: it was dropped on the window, and
    /// egui has it.)
    Returned,
}

pub use os::{Drag, Window, start};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as os;
#[cfg(any(target_os = "linux", test))]
mod protocol;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "linux")]
mod x11;

/// Before each frame, with its input (see [eframe::App::raw_input_hook]):
/// on Wayland, our part of winit's connection is looked after, and egui is
/// given the pointer's moves over the window while the compositor has a
/// drag. Nothing elsewhere.
pub fn raw_input_hook(window: Option<Window>, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
    #[cfg(target_os = "linux")]
    if let Some(window) = window {
        os::raw_input_hook(window, ctx, raw_input);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (window, ctx, raw_input);
}

#[cfg(windows)]
mod os {
    use std::{
        cell::Cell,
        ffi::c_void,
        os::windows::ffi::OsStrExt,
        path::{Path, PathBuf},
        ptr::{null, null_mut},
    };

    use windows_sys::{
        Win32::{
            Foundation::{
                DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, E_NOINTERFACE,
                GlobalFree, HWND, LPARAM, POINT, RECT, S_OK, WPARAM,
            },
            Graphics::Gdi::ScreenToClient,
            System::{
                Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL},
                Memory::{GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock},
                Ole::{
                    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE,
                    DROPEFFECT_NONE, OleInitialize, OleUninitialize,
                },
                SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS},
            },
            UI::{
                Input::KeyboardAndMouse::{GetAsyncKeyState, SetCapture, VK_LBUTTON, VK_RBUTTON},
                Shell::{
                    Common::ITEMIDLIST, DROPFILES, ILFree, SHCreateDataObject, SHDoDragDrop,
                    SHParseDisplayName,
                },
                WindowsAndMessaging::{
                    GA_ROOT, GetAncestor, GetClientRect, GetCursorPos, GetSystemMetrics,
                    PostMessageW, SM_SWAPBUTTON, WM_LBUTTONUP, WM_MOUSEMOVE, WindowFromPoint,
                },
            },
        },
        core::{BOOL, GUID, HRESULT},
    };

    use super::Outcome;

    const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
    const IID_IDATAOBJECT: GUID = GUID::from_u128(0x0000010e_0000_0000_c000_000000000046);
    const IID_IDROPSOURCE: GUID = GUID::from_u128(0x00000121_0000_0000_c000_000000000046);

    /// The main window, which drags start from: its `HWND`.
    #[derive(Clone, Copy)]
    pub struct Window(isize);

    impl Window {
        pub fn of(frame: &eframe::Frame) -> Option<Self> {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            match frame.window_handle().ok()?.as_raw() {
                RawWindowHandle::Win32(h) => Some(Self(h.hwnd.get())),
                _ => None,
            }
        }

        fn hwnd(self) -> HWND {
            self.0 as HWND
        }
    }

    /// Drag `paths` with the shell's drag and drop, from where the pointer
    /// is, until they're dropped or the pointer comes back over `window`.
    /// Blocks until then: Windows runs the drag in a loop of its own, while
    /// winit holds on to the window's events (see [Outcome]). So the
    /// [Drag] returned is already over.
    pub fn start(window: Window, paths: &[PathBuf], _ctx: &egui::Context) -> Drag {
        Drag(Some(run(window, paths)))
    }

    /// A drag handed to the shell (see [start]), over by the time it's had.
    pub struct Drag(Option<Result<Outcome, String>>);

    impl Drag {
        /// How it ended (and then nothing).
        pub fn poll(&mut self) -> Option<Result<Outcome, String>> {
            self.0.take()
        }

        /// Nothing to call off: it's over.
        pub fn cancel(&self) {}

        /// The shell shows its own.
        pub fn cursor(&self) -> Option<egui::CursorIcon> {
            None
        }
    }

    fn run(window: Window, paths: &[PathBuf]) -> Result<Outcome, String> {
        let hwnd = window.hwnd();
        // Let go of already, where we didn't see it (in another window):
        // there's no drag left to hand over.
        if !primary_down() {
            sync_button(hwnd);
            return Ok(Outcome::Ended { dropped: false });
        }

        // winit has already initialized OLE on this thread, for files
        // dropped on the window; this just makes sure.
        let hr = unsafe { OleInitialize(null()) };
        if hr < 0 {
            return Err(format!("drag: can't start OLE (error {hr:#010x})"));
        }

        let end = drag(hwnd, paths);
        unsafe { OleUninitialize() };

        if matches!(end, Ok(DragEnd::Returned)) {
            // Show egui where the pointer came back, so it carries on
            // without waiting for it to move.
            let keys = if primary_down() { MK_LBUTTON } else { 0 };
            post_mouse(hwnd, WM_MOUSEMOVE, keys);
        }
        sync_button(hwnd);
        end.map(|end| match end {
            DragEnd::Returned => Outcome::Returned,
            end => Outcome::Ended {
                dropped: matches!(end, DragEnd::Dropped),
            },
        })
    }

    /// Bring egui and winit up to date with the primary button, after the
    /// shell's drag loop had the mouse: they still think it's held. If it's
    /// been let go of (the loop took that), tell them. If it's still held,
    /// take back the mouse capture winit had for it, which the loop took,
    /// so its release reaches us wherever it happens.
    fn sync_button(hwnd: HWND) {
        if primary_down() {
            unsafe { SetCapture(hwnd) };
        } else {
            post_mouse(hwnd, WM_LBUTTONUP, 0);
        }
    }

    /// How [drag] ended.
    enum DragEnd {
        Dropped,
        /// With Escape, or let go of where nothing takes files.
        CalledOff,
        /// The pointer came back over our window.
        Returned,
    }

    /// The drag itself.
    fn drag(hwnd: HWND, paths: &[PathBuf]) -> Result<DragEnd, String> {
        let data = data_object(paths)?;
        let source = DropSource::new(hwnd);
        // Whatever the target would do with Explorer's: as there, it moves
        // them by default on the same drive, unless Ctrl / Shift / Alt
        // says otherwise.
        let allowed = DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK;
        let mut effect: DROPEFFECT = DROPEFFECT_NONE;
        // No window for the drag image, so the shell draws its own.
        let hr = unsafe { SHDoDragDrop(null_mut(), data.0, source.cast(), allowed, &mut effect) };
        let returned = unsafe { (*source).returned.get() };

        unsafe { DropSource::release(source.cast()) };
        match hr {
            DRAGDROP_S_DROP => Ok(DragEnd::Dropped),
            DRAGDROP_S_CANCEL if returned => Ok(DragEnd::Returned),
            DRAGDROP_S_CANCEL => Ok(DragEnd::CalledOff),
            _ => Err(format!(
                "drag: the shell's drag and drop failed (error {hr:#010x})"
            )),
        }
    }

    /// Whether the primary mouse button is down right now, whatever the
    /// window's messages have said.
    fn primary_down() -> bool {
        let swapped = unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0;
        let key = if swapped { VK_RBUTTON } else { VK_LBUTTON };
        unsafe { GetAsyncKeyState(key.into()) < 0 }
    }

    /// Post `msg`, a mouse message, to `hwnd` as if the pointer were where
    /// it is now, with the buttons in `keys` held.
    fn post_mouse(hwnd: HWND, msg: u32, keys: MODIFIERKEYS_FLAGS) {
        let mut pt = POINT { x: 0, y: 0 };
        unsafe {
            GetCursorPos(&mut pt);
            ScreenToClient(hwnd, &mut pt);
        }
        let lparam = ((pt.y as u16 as u32) << 16 | pt.x as u16 as u32) as LPARAM;
        unsafe { PostMessageW(hwnd, msg, keys as WPARAM, lparam) };
    }

    /// Whether the pointer is over `hwnd`'s client area, the part winit
    /// gives egui the pointer in, and not some other window in front of it.
    fn pointer_over(hwnd: HWND) -> bool {
        let mut pt = POINT { x: 0, y: 0 };
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        unsafe {
            if GetCursorPos(&mut pt) == 0 {
                return false;
            }
            let under = WindowFromPoint(pt);
            if under.is_null() || GetAncestor(under, GA_ROOT) != hwnd {
                return false;
            }
            if ScreenToClient(hwnd, &mut pt) == 0 || GetClientRect(hwnd, &mut rect) == 0 {
                return false;
            }
        }
        (rect.left..rect.right).contains(&pt.x) && (rect.top..rect.bottom).contains(&pt.y)
    }

    /// A COM object's `IUnknown` methods, the start of every interface's
    /// table.
    #[repr(C)]
    struct UnknownVtbl {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
    }

    /// `IDataObject`'s table, as far as `SetData`.
    #[repr(C)]
    pub(super) struct DataObjectVtbl {
        unknown: UnknownVtbl,
        pub(super) get_data:
            unsafe extern "system" fn(*mut c_void, *const FORMATETC, *mut StgMedium) -> HRESULT,
        _get_data_here: usize,
        _query_get_data: usize,
        _get_canonical_format_etc: usize,
        set_data: unsafe extern "system" fn(
            *mut c_void,
            *const FORMATETC,
            *const StgMedium,
            BOOL,
        ) -> HRESULT,
    }

    /// `STGMEDIUM`, holding an `HGLOBAL`: all we hand over or take.
    #[repr(C)]
    pub(super) struct StgMedium {
        pub(super) tymed: u32,
        pub(super) handle: *mut c_void,
        pub(super) release: *mut c_void,
    }

    /// The format for a file list in an `HGLOBAL`.
    pub(super) const HDROP_FORMAT: FORMATETC = FORMATETC {
        cfFormat: CF_HDROP,
        ptd: null_mut(),
        dwAspect: DVASPECT_CONTENT,
        lindex: -1,
        tymed: TYMED_HGLOBAL as u32,
    };

    /// A COM interface pointer we hold a reference to, released on drop.
    pub(super) struct Com(pub(super) *mut c_void);

    impl Drop for Com {
        fn drop(&mut self) {
            unsafe {
                let vtbl = *(self.0 as *const *const UnknownVtbl);
                ((*vtbl).release)(self.0);
            }
        }
    }

    /// The shell's own data object for `paths`, as Explorer drags: a list
    /// of shell items, for Explorer and the like, and a plain file list
    /// (`CF_HDROP`), which browsers and most other programs read. Absolute
    /// paths, from any folders (Deep search finds them in many).
    pub(super) fn data_object(paths: &[PathBuf]) -> Result<Com, String> {
        /// Each item's ID list, freed on drop.
        struct Pidls(Vec<*const ITEMIDLIST>);
        impl Drop for Pidls {
            fn drop(&mut self) {
                for &p in &self.0 {
                    unsafe { ILFree(p) };
                }
            }
        }

        let mut pidls = Pidls(Vec::with_capacity(paths.len()));
        for path in paths {
            let mut pidl = null_mut();
            let wide = to_wide(path);
            let hr =
                unsafe { SHParseDisplayName(wide.as_ptr(), null_mut(), &mut pidl, 0, null_mut()) };
            if hr < 0 {
                return Err(format!(
                    "drag: can't find {} (error {hr:#010x})",
                    path.display()
                ));
            }
            pidls.0.push(pidl);
        }

        // Relative to the desktop (no folder), the root of the absolute
        // ID lists. It copies them.
        let mut obj = null_mut();
        let n = pidls.0.len() as u32;
        let hr = unsafe {
            SHCreateDataObject(
                null(),
                n,
                pidls.0.as_ptr(),
                null_mut(),
                &IID_IDATAOBJECT,
                &mut obj,
            )
        };
        if hr < 0 || obj.is_null() {
            return Err(format!(
                "drag: can't make the shell data object (error {hr:#010x})"
            ));
        }
        let obj = Com(obj);
        // It doesn't make the file list from them itself, as a folder's own
        // does, so it's given one.
        set_file_list(&obj, paths)?;
        Ok(obj)
    }

    /// Give the data object `obj` a `CF_HDROP` file list of `paths`: a
    /// `DROPFILES` header, then each path NUL-terminated, and another NUL
    /// to end the list.
    fn set_file_list(obj: &Com, paths: &[PathBuf]) -> Result<(), String> {
        let mut list: Vec<u16> = Vec::new();
        for path in paths {
            list.extend(path.as_os_str().encode_wide());
            list.push(0);
        }
        list.push(0);

        let header = size_of::<DROPFILES>();
        let mem = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, header + 2 * list.len()) };
        if mem.is_null() {
            return Err("drag: out of memory for the file list".into());
        }
        unsafe {
            let ptr = GlobalLock(mem).cast::<u8>();
            if ptr.is_null() {
                GlobalFree(mem);
                return Err("drag: can't lock the file list's memory".into());
            }
            ptr.cast::<DROPFILES>().write_unaligned(DROPFILES {
                pFiles: header as u32,
                pt: POINT { x: 0, y: 0 },
                fNC: 0,
                fWide: 1,
            });
            std::ptr::copy_nonoverlapping(
                list.as_ptr().cast::<u8>(),
                ptr.add(header),
                2 * list.len(),
            );
            GlobalUnlock(mem);
        }

        // Handed over (`TRUE`): the data object frees it.
        let medium = StgMedium {
            tymed: TYMED_HGLOBAL as u32,
            handle: mem,
            release: null_mut(),
        };
        let hr = unsafe {
            let vtbl = *(obj.0 as *const *const DataObjectVtbl);
            ((*vtbl).set_data)(obj.0, &HDROP_FORMAT, &medium, 1)
        };
        if hr < 0 {
            unsafe { GlobalFree(mem) };
            return Err(format!("drag: can't add the file list (error {hr:#010x})"));
        }
        Ok(())
    }

    fn to_wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    /// Our `IDropSource`: tells the shell when the drag is done, as its
    /// default one would, and also calls it off when the pointer comes back
    /// over our window, recording that in `returned`.
    #[repr(C)]
    struct DropSource {
        vtbl: &'static DropSourceVtbl,
        refs: Cell<u32>,
        window: HWND,
        returned: Cell<bool>,
    }

    #[repr(C)]
    struct DropSourceVtbl {
        unknown: UnknownVtbl,
        query_continue_drag:
            unsafe extern "system" fn(*mut DropSource, BOOL, MODIFIERKEYS_FLAGS) -> HRESULT,
        give_feedback: unsafe extern "system" fn(*mut DropSource, DROPEFFECT) -> HRESULT,
    }

    static DROP_SOURCE_VTBL: DropSourceVtbl = DropSourceVtbl {
        unknown: UnknownVtbl {
            query_interface: DropSource::query_interface,
            add_ref: DropSource::add_ref,
            release: DropSource::release,
        },
        query_continue_drag: DropSource::query_continue_drag,
        give_feedback: DropSource::give_feedback,
    };

    // Called only on this thread, by the drag loop running in [drag]: OLE's
    // drag and drop is single-threaded.
    impl DropSource {
        /// A new one, with the one reference [drag] holds.
        fn new(window: HWND) -> *mut Self {
            Box::into_raw(Box::new(Self {
                vtbl: &DROP_SOURCE_VTBL,
                refs: Cell::new(1),
                window,
                returned: Cell::new(false),
            }))
        }

        unsafe extern "system" fn query_interface(
            this: *mut c_void,
            iid: *const GUID,
            out: *mut *mut c_void,
        ) -> HRESULT {
            let iid = unsafe { &*iid };
            let ours = [IID_IUNKNOWN, IID_IDROPSOURCE].iter().any(|i| {
                (i.data1, i.data2, i.data3, i.data4) == (iid.data1, iid.data2, iid.data3, iid.data4)
            });
            if !ours {
                unsafe { *out = null_mut() };
                return E_NOINTERFACE;
            }
            unsafe {
                Self::add_ref(this);
                *out = this;
            }
            S_OK
        }

        unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
            let refs = unsafe { &(*this.cast::<Self>()).refs };
            refs.set(refs.get() + 1);
            refs.get()
        }

        unsafe extern "system" fn release(this: *mut c_void) -> u32 {
            let refs = unsafe { &(*this.cast::<Self>()).refs }.get() - 1;
            unsafe { (*this.cast::<Self>()).refs.set(refs) };
            if refs == 0 {
                drop(unsafe { Box::from_raw(this.cast::<Self>()) });
            }
            refs
        }

        unsafe extern "system" fn query_continue_drag(
            this: *mut Self,
            escape: BOOL,
            keys: MODIFIERKEYS_FLAGS,
        ) -> HRESULT {
            let this = unsafe { &*this };
            if escape != 0 {
                DRAGDROP_S_CANCEL
            } else if pointer_over(this.window) {
                this.returned.set(true);
                DRAGDROP_S_CANCEL
            } else if keys & MK_LBUTTON == 0 {
                DRAGDROP_S_DROP
            } else {
                S_OK
            }
        }

        unsafe extern "system" fn give_feedback(_this: *mut Self, _effect: DROPEFFECT) -> HRESULT {
            DRAGDROP_S_USEDEFAULTCURSORS
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod os {
    use std::{convert::Infallible, path::PathBuf};

    use super::Outcome;

    /// Never made: dragging out of the window isn't done here yet.
    #[derive(Clone, Copy)]
    pub struct Window(Infallible);

    impl Window {
        pub fn of(_frame: &eframe::Frame) -> Option<Self> {
            None
        }
    }

    /// Never made either.
    pub struct Drag(Infallible);

    impl Drag {
        pub fn poll(&mut self) -> Option<Result<Outcome, String>> {
            match self.0 {}
        }

        pub fn cancel(&self) {
            match self.0 {}
        }

        pub fn cursor(&self) -> Option<egui::CursorIcon> {
            match self.0 {}
        }
    }

    pub fn start(window: Window, _paths: &[PathBuf], _ctx: &egui::Context) -> Drag {
        match window.0 {}
    }
}
