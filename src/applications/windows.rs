use std::{
    collections::HashMap,
    ffi::c_void,
    mem::size_of,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::null_mut,
    slice,
};

use egui::ColorImage;
use windows::{
    Win32::{
        Storage::EnhancedStorage::PKEY_AppUserModel_ID,
        System::Com::{
            COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
            StructuredStorage::{PropVariantClear, PropVariantToStringAlloc},
        },
        UI::Shell::{
            FOLDERID_AppsFolder, IShellItem, KF_FLAG_DEFAULT,
            PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow},
            SHCreateItemInKnownFolder, SHGetIDListFromObject, SIGDN_NORMALDISPLAY,
        },
    },
    core::{PCWSTR, PWSTR},
};
#[cfg(target_pointer_width = "64")]
use windows_sys::Win32::UI::WindowsAndMessaging::GetClassLongPtrW as get_class_long;
#[cfg(target_pointer_width = "32")]
use windows_sys::Win32::UI::WindowsAndMessaging::GetClassLongW as get_class_long;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, HWND, LPARAM, RECT},
    Graphics::{
        Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
        Gdi::{
            BI_RGB, BITMAPINFO, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC,
            DeleteObject, GdiFlush, SelectObject,
        },
    },
    Storage::{
        FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW},
        Packaging::Appx::GetApplicationUserModelId,
    },
    System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    },
    UI::{
        Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_PIDL, SHGetFileInfoW},
        WindowsAndMessaging::{
            DI_NORMAL, DestroyIcon, DrawIconEx, EnumChildWindows, EnumWindows, GCLP_HICON,
            GCLP_HICONSM, GW_OWNER, GWL_EXSTYLE, GetClassNameW, GetShellWindow, GetWindow,
            GetWindowLongW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, HICON,
            ICON_BIG, ICON_SMALL2, IDI_APPLICATION, IsWindowVisible, LoadIconW, SMTO_ABORTIFHUNG,
            SendMessageTimeoutW, WM_GETICON, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        },
    },
};

use super::{Application, ICON_PIXELS, RunningApps};

pub(super) fn collect(apps: &mut RunningApps<'_>) -> Result<Option<&'static str>, String> {
    // The sampler runs on its own thread. Shell queries on it need a COM apartment.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() }
        .map_err(|e| format!("Could not initialize the Windows Shell: {e}"))?;
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }
    let _apartment = Apartment;

    let mut windows: Vec<HWND> = Vec::new();
    if unsafe {
        EnumWindows(
            Some(collect_window),
            (&mut windows as *mut Vec<HWND>) as LPARAM,
        )
    } == 0
    {
        return Err(format!(
            "Could not list windows: {}",
            std::io::Error::last_os_error()
        ));
    }

    let mut processes = HashMap::new();
    for hwnd in windows {
        let class = class_name(hwnd);
        let mut pid = process_id(hwnd);
        if pid == 0 || class.is_empty() {
            continue;
        }
        if class == "ApplicationFrameWindow" {
            // The outer frame belongs to ApplicationFrameHost. Its CoreWindow belongs to the app.
            let mut child_pid = 0u32;
            unsafe {
                EnumChildWindows(
                    hwnd,
                    Some(find_app_child),
                    (&mut child_pid as *mut u32) as LPARAM,
                );
            }
            if child_pid == 0 {
                continue;
            }
            pid = child_pid;
        }

        let (path, process_app_id) = processes.entry(pid).or_insert_with(|| process_info(pid));
        let app_id = window_app_id(hwnd).or_else(|| process_app_id.clone());
        let id = if let Some(id) = &app_id {
            format!("app:{}", id.to_lowercase())
        } else if let Some(path) = path {
            format!("exe:{}", path.to_string_lossy().to_lowercase())
        } else {
            format!("pid:{pid}")
        };
        apps.add(id, || {
            app_details(hwnd, app_id.as_deref(), path.as_deref(), class)
        });
    }
    Ok(None)
}

unsafe extern "system" fn collect_window(hwnd: HWND, data: LPARAM) -> i32 {
    // SAFETY: EnumWindows invokes the callback synchronously with our Vec pointer.
    unsafe {
        if IsWindowVisible(hwnd) == 0 || hwnd == GetShellWindow() {
            return 1;
        }
        let style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if style & WS_EX_TOOLWINDOW != 0
            || style & WS_EX_APPWINDOW == 0
                && (style & WS_EX_NOACTIVATE != 0 || !GetWindow(hwnd, GW_OWNER).is_null())
        {
            return 1;
        }
        let mut cloaked = 0u32;
        let mut rect = RECT::default();
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            (&mut cloaked as *mut u32).cast(),
            size_of::<u32>() as u32,
        );
        if cloaked == 0
            && GetWindowRect(hwnd, &mut rect) != 0
            && rect.left < rect.right
            && rect.top < rect.bottom
            && !matches!(
                class_name(hwnd).as_str(),
                "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
            )
        {
            (*(data as *mut Vec<HWND>)).push(hwnd);
        }
        1
    }
}

unsafe extern "system" fn find_app_child(hwnd: HWND, data: LPARAM) -> i32 {
    if class_name(hwnd) == "Windows.UI.Core.CoreWindow" {
        // SAFETY: EnumChildWindows passes our output pointer synchronously.
        unsafe { *(data as *mut u32) = process_id(hwnd) };
        return 0;
    }
    1
}

fn process_id(hwnd: HWND) -> u32 {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    pid
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..len as usize])
}

fn process_info(pid: u32) -> (Option<PathBuf>, Option<String>) {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return (None, None);
    }
    let mut buf = vec![0u16; 32768];
    let mut len = buf.len() as u32;
    let path = (unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) } != 0)
        .then(|| {
            use std::os::windows::ffi::OsStringExt;
            PathBuf::from(std::ffi::OsString::from_wide(&buf[..len as usize]))
        });
    let id = process_app_id(handle);
    unsafe { CloseHandle(handle) };
    (path, id)
}

fn process_app_id(handle: HANDLE) -> Option<String> {
    let mut buf = [0u16; 256];
    let mut len = buf.len() as u32;
    let result = unsafe { GetApplicationUserModelId(handle, &mut len, buf.as_mut_ptr()) };
    (result == 0 && len > 1).then(|| String::from_utf16_lossy(&buf[..len as usize - 1]))
}

fn window_app_id(hwnd: HWND) -> Option<String> {
    unsafe {
        let store: IPropertyStore =
            SHGetPropertyStoreForWindow(windows::Win32::Foundation::HWND(hwnd)).ok()?;
        let mut value = store.GetValue(&PKEY_AppUserModel_ID).ok()?;
        let id = PropVariantToStringAlloc(&value).ok().and_then(shell_string);
        let _ = PropVariantClear(&mut value);
        id
    }
}

fn shell_string(value: PWSTR) -> Option<String> {
    if value.is_null() {
        return None;
    }
    // The Shell allocates these strings with CoTaskMemAlloc.
    let text = unsafe { value.to_string().ok() };
    unsafe { CoTaskMemFree(Some(value.0.cast())) };
    text.filter(|s| !s.trim().is_empty())
}

fn app_details(hwnd: HWND, id: Option<&str>, path: Option<&Path>, class: String) -> Application {
    let item = id.and_then(|id| {
        let id: Vec<_> = id.encode_utf16().chain([0]).collect();
        unsafe {
            SHCreateItemInKnownFolder::<_, IShellItem>(
                &FOLDERID_AppsFolder,
                KF_FLAG_DEFAULT,
                PCWSTR(id.as_ptr()),
            )
            .ok()
        }
    });
    let path_wide = path.map(|path| {
        path.as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>()
    });
    let name = item
        .as_ref()
        .and_then(|item| unsafe {
            item.GetDisplayName(SIGDN_NORMALDISPLAY)
                .ok()
                .and_then(shell_string)
        })
        .or_else(|| path_wide.as_deref().and_then(file_description))
        .or_else(|| {
            path.and_then(Path::file_stem)
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| {
            let mut title = [0u16; 512];
            let len = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) };
            if len > 0 {
                String::from_utf16_lossy(&title[..len as usize])
            } else {
                class
            }
        });
    let icon = item
        .as_ref()
        .and_then(|item| unsafe {
            let pidl = SHGetIDListFromObject(item).ok()?;
            let icon = shell_icon(pidl.cast(), SHGFI_PIDL);
            CoTaskMemFree(Some(pidl.cast()));
            icon
        })
        .or_else(|| {
            path_wide
                .as_ref()
                .and_then(|path| shell_icon(path.as_ptr(), 0))
        })
        .or_else(|| window_icon(hwnd))
        .or_else(|| draw_icon(unsafe { LoadIconW(null_mut(), IDI_APPLICATION) }));
    Application { name, icon }
}

fn file_description(path: &[u16]) -> Option<String> {
    let size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), null_mut()) };
    if size == 0 {
        return None;
    }
    let mut buffer = vec![0u32; (size as usize).div_ceil(size_of::<u32>())];
    if unsafe { GetFileVersionInfoW(path.as_ptr(), 0, size, buffer.as_mut_ptr().cast()) } == 0 {
        return None;
    }
    let query = |key: &str| {
        let wide: Vec<_> = key.encode_utf16().chain([0]).collect();
        let (mut value, mut len) = (null_mut(), 0);
        (unsafe { VerQueryValueW(buffer.as_ptr().cast(), wide.as_ptr(), &mut value, &mut len) }
            != 0
            && !value.is_null()
            && len != 0)
            .then_some((value, len as usize))
    };
    let translations = query(r"\VarFileInfo\Translation").map_or(&[][..], |(p, len)| {
        // The translation table is a series of (language, code page) pairs.
        unsafe { slice::from_raw_parts(p.cast::<[u16; 2]>(), len / 4) }
    });
    translations
        .iter()
        .copied()
        .chain([[0x0409, 0x04b0]])
        .find_map(|[lang, page]| {
            let (value, len) = query(&format!(
                r"\StringFileInfo\{lang:04x}{page:04x}\FileDescription"
            ))?;
            let text =
                String::from_utf16_lossy(unsafe { slice::from_raw_parts(value.cast(), len) });
            let text = text.trim_end_matches('\0').trim();
            (!text.is_empty()).then(|| text.to_owned())
        })
}

fn shell_icon(path_or_pidl: *const u16, flags: u32) -> Option<ColorImage> {
    let mut info = SHFILEINFOW::default();
    if unsafe {
        SHGetFileInfoW(
            path_or_pidl,
            0,
            &mut info,
            size_of::<SHFILEINFOW>() as u32,
            flags | SHGFI_ICON,
        )
    } == 0
    {
        return None;
    }
    let image = draw_icon(info.hIcon);
    unsafe { DestroyIcon(info.hIcon) };
    image
}

fn window_icon(hwnd: HWND) -> Option<ColorImage> {
    for size in [ICON_BIG, ICON_SMALL2] {
        let mut handle = 0;
        if unsafe {
            SendMessageTimeoutW(
                hwnd,
                WM_GETICON,
                size as usize,
                0,
                SMTO_ABORTIFHUNG,
                30,
                &mut handle,
            )
        } != 0
            && let Some(image) = draw_icon(handle as HICON)
        {
            return Some(image);
        }
    }
    [GCLP_HICON, GCLP_HICONSM]
        .into_iter()
        .find_map(|index| draw_icon(unsafe { get_class_long(hwnd, index) } as HICON))
}

fn draw_icon(icon: HICON) -> Option<ColorImage> {
    if icon.is_null() {
        return None;
    }
    // Draw against black and white to get alpha for both 32-bit and older mask icons.
    let size = ICON_PIXELS as i32;
    let mut info = BITMAPINFO::default();
    info.bmiHeader.biSize = size_of_val(&info.bmiHeader) as u32;
    info.bmiHeader.biWidth = size;
    info.bmiHeader.biHeight = -size;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB;
    unsafe {
        let dc = CreateCompatibleDC(null_mut());
        if dc.is_null() {
            return None;
        }
        let mut bits: *mut c_void = null_mut();
        let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        let image = if bitmap.is_null() {
            None
        } else {
            let previous = SelectObject(dc, bitmap);
            let len = (size * size * 4) as usize;
            let render = |bg| {
                bits.cast::<u8>().write_bytes(bg, len);
                let drawn = DrawIconEx(dc, 0, 0, icon, size, size, 0, null_mut(), DI_NORMAL);
                GdiFlush();
                (drawn != 0).then(|| slice::from_raw_parts(bits.cast::<u8>(), len).to_vec())
            };
            let image = render(0).zip(render(255)).map(|(black, white)| {
                let pixels = black
                    .chunks_exact(4)
                    .zip(white.chunks_exact(4))
                    .map(|(b, w)| {
                        let alpha = 255 - w[0].abs_diff(b[0]);
                        egui::Color32::from_rgba_premultiplied(
                            b[2].min(alpha),
                            b[1].min(alpha),
                            b[0].min(alpha),
                            alpha,
                        )
                    })
                    .collect();
                ColorImage::new([size as usize; 2], pixels)
            });
            SelectObject(dc, previous);
            DeleteObject(bitmap);
            image
        };
        DeleteDC(dc);
        image
    }
}
