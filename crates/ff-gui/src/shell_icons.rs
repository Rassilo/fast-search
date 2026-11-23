//! Windows: the icons Explorer shows, from the shell. A file's icon is
//! asked for by its extension alone (no disk access, so it is quick and
//! works for any of millions of entries); a drive's from the drive.

#![allow(non_snake_case, clippy::upper_case_acronyms)]

use std::ffi::c_void;
use std::path::Path;

type HANDLE = *mut c_void;

#[repr(C)]
struct SHFILEINFOW {
    hIcon: HANDLE,
    iIcon: i32,
    dwAttributes: u32,
    szDisplayName: [u16; 260],
    szTypeName: [u16; 80],
}

#[repr(C)]
struct BITMAPINFOHEADER {
    biSize: u32,
    biWidth: i32,
    biHeight: i32,
    biPlanes: u16,
    biBitCount: u16,
    biCompression: u32,
    biSizeImage: u32,
    biXPelsPerMeter: i32,
    biYPelsPerMeter: i32,
    biClrUsed: u32,
    biClrImportant: u32,
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn SHGetFileInfoW(path: *const u16, attrs: u32, info: *mut SHFILEINFOW, size: u32, flags: u32) -> usize;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn DrawIconEx(dc: HANDLE, x: i32, y: i32, icon: HANDLE, w: i32, h: i32, step: u32, brush: HANDLE, flags: u32) -> i32;
    fn DestroyIcon(icon: HANDLE) -> i32;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateCompatibleDC(dc: HANDLE) -> HANDLE;
    fn DeleteDC(dc: HANDLE) -> i32;
    fn CreateDIBSection(dc: HANDLE, info: *const BITMAPINFOHEADER, usage: u32, bits: *mut *mut u8, section: HANDLE, offset: u32) -> HANDLE;
    fn SelectObject(dc: HANDLE, obj: HANDLE) -> HANDLE;
    fn DeleteObject(obj: HANDLE) -> i32;
    fn GdiFlush() -> i32;
}

/// The shell's icon for an entry, as `size` x `size` BGRA pixels (alpha
/// not premultiplied). `name` gives the type; for a drive, `real` names
/// the drive itself. None if the shell has none.
pub fn icon(name: &str, is_dir: bool, real: Option<&Path>, size: usize) -> Option<Vec<[u8; 4]>> {
    const SHGFI_ICON: u32 = 0x100;
    const SHGFI_SMALLICON: u32 = 0x1;
    const SHGFI_USEFILEATTRIBUTES: u32 = 0x10;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    let (path, flags) = match real {
        Some(real) => (crate::winsys::wide(real.as_os_str()), 0),
        None => (name.encode_utf16().chain(Some(0)).collect(), SHGFI_USEFILEATTRIBUTES),
    };
    let attrs = if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
    // The small icon for list sizes, the large one to scale down from.
    let which = if size <= 20 { SHGFI_SMALLICON } else { 0 };
    // SAFETY: a NUL-terminated path and a properly sized output struct.
    let icon = unsafe {
        let mut info: SHFILEINFOW = std::mem::zeroed();
        if SHGetFileInfoW(path.as_ptr(), attrs, &mut info, size_of::<SHFILEINFOW>() as u32, SHGFI_ICON | which | flags) == 0 {
            return None;
        }
        info.hIcon
    };
    if icon.is_null() {
        return None;
    }
    let pixels = render(icon, size);
    // SAFETY: the icon the shell handed us, ours to destroy.
    unsafe { DestroyIcon(icon) };
    pixels
}

/// Draws `icon` at `size` on black and on white; the difference gives
/// each pixel's opacity, whatever kind of icon it is.
fn render(icon: HANDLE, size: usize) -> Option<Vec<[u8; 4]>> {
    let info = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: size as i32,
        biHeight: -(size as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: 0,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    const DI_NORMAL: u32 = 3;
    // SAFETY: a bitmap of size x size made here, its pixels read within
    // bounds, every object freed.
    unsafe {
        let dc = CreateCompatibleDC(std::ptr::null_mut());
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(dc, &info, 0, &mut bits, std::ptr::null_mut(), 0);
        if dc.is_null() || bitmap.is_null() {
            DeleteDC(dc);
            return None;
        }
        let old = SelectObject(dc, bitmap);
        let mut on = [Vec::new(), Vec::new()];
        for (k, background) in [0u8, 255].into_iter().enumerate() {
            let pixels = std::slice::from_raw_parts_mut(bits, size * size * 4);
            pixels.fill(background);
            DrawIconEx(dc, 0, 0, icon, size as i32, size as i32, 0, std::ptr::null_mut(), DI_NORMAL);
            GdiFlush();
            on[k] = pixels.to_vec();
        }
        SelectObject(dc, old);
        DeleteObject(bitmap);
        DeleteDC(dc);
        let [black, white] = on;
        Some(
            black
                .chunks_exact(4)
                .zip(white.chunks_exact(4))
                .map(|(b, w)| {
                    // Opacity from how much the background shows through
                    // (green, the most precise channel), then the colour
                    // without the black it was mixed with.
                    let alpha = 255 - (w[1] as i32 - b[1] as i32).clamp(0, 255);
                    let un = |c: u8| if alpha == 0 { 0 } else { (c as i32 * 255 / alpha).min(255) as u8 };
                    [un(b[0]), un(b[1]), un(b[2]), alpha as u8]
                })
                .collect(),
        )
    }
}
