//! Window screenshot capture for `dv --automation`.
//!
//! Agents verify UI work by asking the running app to screenshot itself
//! (JSON command `{"cmd":"screenshot","path":"..."}`) rather than relying on
//! desktop screenshot tools, which are flaky about which window is on top and
//! whether it's actually `dv`'s window. Finding the window by *this process's*
//! PID (instead of title matching or "whatever's focused") eliminates that
//! class of flakiness entirely.
//!
//! Capture goes through `PrintWindow` with `PW_RENDERFULLCONTENT` first,
//! because GPUI renders via DirectComposition — plain `BitBlt` against a
//! window's DC only sees what GDI itself painted, which for a DirectComposition
//! surface is nothing. `PW_RENDERFULLCONTENT` tells `PrintWindow` to composite
//! the actual presented frame. On the (rare, e.g. remote desktop / odd driver)
//! chance that still comes back blank, we fall back to a plain screen-space
//! `BitBlt` of the window's client rect.

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::path::Path;

    use anyhow::{Context, Result, anyhow, bail};
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT, TRUE};
    use windows_sys::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLACKNESS, BitBlt, ClientToScreen,
        CreateCompatibleBitmap, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC,
        GetDIBits, HBITMAP, HDC, HGDIOBJ, PatBlt, ReleaseDC, SRCCOPY, SelectObject,
    };
    use windows_sys::Win32::Storage::Xps::{PW_CLIENTONLY, PrintWindow};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClientRect, GetWindowThreadProcessId, IsWindowVisible,
    };

    /// Not yet in the `windows-sys` version pinned here; tells `PrintWindow`
    /// to composite the real presented frame instead of whatever GDI thinks
    /// the window painted (see module docs — required for DirectComposition
    /// surfaces like GPUI's).
    const PW_RENDERFULLCONTENT: u32 = 2;

    /// RAII guard releasing a DC obtained via `GetDC(hwnd)`.
    struct WindowDc {
        hwnd: HWND,
        dc: HDC,
    }

    impl Drop for WindowDc {
        fn drop(&mut self) {
            // SAFETY: `dc` was returned by `GetDC(self.hwnd)` and is released
            // at most once (guard is only constructed once per DC).
            unsafe {
                ReleaseDC(self.hwnd, self.dc);
            }
        }
    }

    /// RAII guard for a memory DC obtained via `CreateCompatibleDC`.
    struct MemDc(HDC);

    impl Drop for MemDc {
        fn drop(&mut self) {
            // SAFETY: `0` was returned by `CreateCompatibleDC` and owned solely
            // by this guard.
            unsafe {
                DeleteDC(self.0);
            }
        }
    }

    /// RAII guard for a GDI bitmap obtained via `CreateCompatibleBitmap`.
    struct Bitmap(HBITMAP);

    impl Drop for Bitmap {
        fn drop(&mut self) {
            // SAFETY: `0` was returned by `CreateCompatibleBitmap` and owned
            // solely by this guard. `Selection` guards (declared after the
            // bitmap, so dropped before it) guarantee the bitmap is no
            // longer selected into any DC — `DeleteObject` on a selected
            // bitmap fails and leaks.
            unsafe {
                DeleteObject(self.0);
            }
        }
    }

    /// RAII guard for `SelectObject`: restores the DC's previous object on
    /// drop, on *every* path — early `?` returns included. Two invariants
    /// depend on it: a bitmap must be deselected before `DeleteObject`
    /// (else it leaks) and before `GetDIBits` (documented API requirement).
    struct Selection {
        dc: HDC,
        old: HGDIOBJ,
    }

    impl Selection {
        fn new(dc: HDC, object: HGDIOBJ) -> Self {
            // SAFETY: caller supplies a valid DC and a compatible, valid
            // GDI object (enforced by the only call sites below).
            let old = unsafe { SelectObject(dc, object) };
            Self { dc, old }
        }
    }

    impl Drop for Selection {
        fn drop(&mut self) {
            // SAFETY: `old` is exactly what `SelectObject` reported as
            // previously selected into this still-live DC.
            unsafe {
                SelectObject(self.dc, self.old);
            }
        }
    }

    /// Out-parameter for `EnumWindows`, passed through as an `LPARAM`.
    struct FindState {
        target_pid: u32,
        found: HWND,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is a pointer to a `FindState` living on
        // `find_main_window`'s stack for the entire duration of the
        // `EnumWindows` call that invokes this callback.
        let state = unsafe { &mut *(lparam as *mut FindState) };

        let mut pid: u32 = 0;
        // SAFETY: `hwnd` is a valid handle supplied by `EnumWindows`; `pid` is
        // a valid out-pointer to a local.
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut pid);
        }
        if pid != state.target_pid {
            return TRUE; // keep enumerating
        }

        // SAFETY: `hwnd` is a valid handle supplied by `EnumWindows`.
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            return TRUE;
        }

        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: `hwnd` is valid; `rect` is a valid out-pointer to a local.
        let ok = unsafe { GetClientRect(hwnd, &mut rect) };
        if ok == 0 || rect.right <= rect.left || rect.bottom <= rect.top {
            return TRUE;
        }

        state.found = hwnd;
        0 // stop enumerating — found it
    }

    /// Find this process's first visible top-level window with a non-empty
    /// client area.
    fn find_main_window() -> Result<HWND> {
        let mut state = FindState {
            // SAFETY: no preconditions.
            target_pid: unsafe { GetCurrentProcessId() },
            found: std::ptr::null_mut(),
        };

        // SAFETY: `enum_proc` matches `WNDENUMPROC`'s signature; `state` is a
        // valid pointer for the duration of this call (it outlives the call
        // on the stack).
        unsafe {
            EnumWindows(Some(enum_proc), &mut state as *mut FindState as LPARAM);
        }

        if state.found.is_null() {
            bail!("no visible top-level window found for the current process");
        }
        Ok(state.found)
    }

    /// Read back `w`x`h` 32bpp top-down BGRA pixels from `bitmap`. The
    /// bitmap must NOT be selected into any DC when this runs (documented
    /// `GetDIBits` requirement — the `Selection` guards ensure it).
    fn read_bgra(mem_dc: HDC, bitmap: HBITMAP, w: i32, h: i32) -> Result<Vec<u8>> {
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // negative = top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [windows_sys::Win32::Graphics::Gdi::RGBQUAD {
                rgbBlue: 0,
                rgbGreen: 0,
                rgbRed: 0,
                rgbReserved: 0,
            }; 1],
        };

        let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
        // SAFETY: `mem_dc`/`bitmap` are valid GDI handles owned by the caller;
        // `buf` is sized exactly `w * h * 4` bytes for 32bpp top-down output
        // matching `info`; `info` is a valid, fully-initialized `BITMAPINFO`.
        let scan_lines = unsafe {
            GetDIBits(
                mem_dc,
                bitmap,
                0,
                h as u32,
                buf.as_mut_ptr() as *mut c_void,
                &mut info,
                DIB_RGB_COLORS,
            )
        };
        if scan_lines == 0 {
            bail!("GetDIBits returned no scan lines");
        }
        Ok(buf)
    }

    /// `true` if every pixel in a BGRA buffer is fully black and fully
    /// transparent — i.e. `PrintWindow` handed back nothing useful. A real
    /// render is never this uniform since app backgrounds aren't pure
    /// transparent black.
    fn is_blank(bgra: &[u8]) -> bool {
        bgra.iter().all(|&b| b == 0)
    }

    /// Capture `hwnd`'s client area into a PNG at `path`. Returns
    /// `(width, height)` in physical pixels.
    pub fn capture_hwnd_png(hwnd: HWND, path: &Path) -> Result<(u32, u32)> {
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: `hwnd` is a caller-supplied valid window handle; `rect` is a
        // valid out-pointer to a local.
        if unsafe { GetClientRect(hwnd, &mut rect) } == 0 {
            bail!("GetClientRect failed");
        }
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        if w <= 0 || h <= 0 {
            bail!("window has an empty client area ({w}x{h})");
        }

        // SAFETY: `hwnd` is a valid window handle.
        let window_dc = unsafe { GetDC(hwnd) };
        if window_dc.is_null() {
            bail!("GetDC(hwnd) failed");
        }
        let window_dc = WindowDc {
            hwnd,
            dc: window_dc,
        };

        // SAFETY: `window_dc.dc` is a valid, just-obtained DC.
        let mem_dc = unsafe { CreateCompatibleDC(window_dc.dc) };
        if mem_dc.is_null() {
            bail!("CreateCompatibleDC failed");
        }
        let mem_dc = MemDc(mem_dc);

        // Deliberately compatible with the *window* DC (not the mem DC) —
        // otherwise this yields a 1-bit monochrome bitmap.
        // SAFETY: `window_dc.dc` is valid; `w`/`h` are checked positive above.
        let bitmap = unsafe { CreateCompatibleBitmap(window_dc.dc, w, h) };
        if bitmap.is_null() {
            bail!("CreateCompatibleBitmap failed");
        }
        let bitmap = Bitmap(bitmap);

        let printed = {
            let _selected = Selection::new(mem_dc.0, bitmap.0);
            // A fresh compatible bitmap holds *undefined* pixels, so a
            // PrintWindow that "succeeds" without rendering (the RDP/odd-
            // driver case) would otherwise hand back garbage that defeats
            // the blank check below. Zero it deterministically first.
            // SAFETY: `mem_dc.0` has `bitmap.0` selected; `w`/`h` are the
            // bitmap's own checked-positive dimensions.
            if unsafe { PatBlt(mem_dc.0, 0, 0, w, h, BLACKNESS) } == 0 {
                bail!("PatBlt (bitmap clear) failed");
            }
            // SAFETY: `hwnd` is valid; `mem_dc.0` has a bitmap selected into
            // it sized to the window's client area.
            unsafe { PrintWindow(hwnd, mem_dc.0, PW_CLIENTONLY | PW_RENDERFULLCONTENT) }
        }; // bitmap deselected here — GetDIBits requires that

        let mut bgra = if printed != 0 {
            read_bgra(mem_dc.0, bitmap.0, w, h)?
        } else {
            Vec::new()
        };

        if printed == 0 || is_blank(&bgra) {
            // Fallback: screen-space BitBlt of the window's client rect.
            let mut origin = POINT { x: 0, y: 0 };
            // SAFETY: `hwnd` is valid; `origin` is a valid out-pointer.
            if unsafe { ClientToScreen(hwnd, &mut origin) } == 0 {
                bail!("ClientToScreen failed");
            }

            // SAFETY: `null_mut()` requests the whole-screen DC, a documented
            // use of `GetDC`.
            let screen_dc = unsafe { GetDC(std::ptr::null_mut()) };
            if screen_dc.is_null() {
                bail!("GetDC(NULL) failed for screen fallback");
            }
            let screen_dc = WindowDc {
                hwnd: std::ptr::null_mut(),
                dc: screen_dc,
            };

            {
                let _selected = Selection::new(mem_dc.0, bitmap.0);
                // SAFETY: `mem_dc.0` and `screen_dc.dc` are both valid DCs;
                // `w`/`h` are the same positive dimensions used to create
                // `bitmap`.
                let blitted = unsafe {
                    BitBlt(
                        mem_dc.0,
                        0,
                        0,
                        w,
                        h,
                        screen_dc.dc,
                        origin.x,
                        origin.y,
                        SRCCOPY,
                    )
                };
                if blitted == 0 {
                    bail!("BitBlt fallback failed");
                }
            } // deselected again before reading back
            bgra = read_bgra(mem_dc.0, bitmap.0, w, h)?;
        }

        // BGRA -> RGBA, alpha forced opaque (DirectComposition-sourced alpha
        // isn't meaningful once flattened into a plain screenshot).
        for px in bgra.chunks_exact_mut(4) {
            px.swap(0, 2);
            px[3] = 255;
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent directory for {}", path.display()))?;
        }
        write_png(path, w as u32, h as u32, &bgra)?;

        Ok((w as u32, h as u32))
    }

    fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
        let file =
            std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let writer = std::io::BufWriter::new(file);

        let mut encoder = png::Encoder::new(writer, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().context("writing PNG header")?;
        writer
            .write_image_data(rgba)
            .context("writing PNG image data")?;
        Ok(())
    }

    /// Capture this process's own top-level window and encode it as a PNG.
    pub fn capture_window_png(path: &Path) -> Result<(u32, u32)> {
        let hwnd = find_main_window().map_err(|e| anyhow!("{e:#}"))?;
        capture_hwnd_png(hwnd, path)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

        #[test]
        #[ignore = "captures the live desktop"]
        fn captures_desktop_window() {
            let path = std::env::temp_dir().join("dv_capture_test_desktop.png");

            // SAFETY: `GetDesktopWindow` never fails and returns a handle
            // valid for the process's lifetime.
            let hwnd = unsafe { GetDesktopWindow() };

            let (w, h) = capture_hwnd_png(hwnd, &path).expect("capture should succeed");
            assert!(w > 100, "unexpectedly small width: {w}");
            assert!(h > 100, "unexpectedly small height: {h}");

            let file = std::fs::File::open(&path).expect("png should exist");
            let decoder = png::Decoder::new(file);
            let mut reader = decoder.read_info().expect("valid png header");
            let mut buf = vec![0u8; reader.output_buffer_size()];
            let info = reader.next_frame(&mut buf).expect("valid png frame");
            let pixels = &buf[..info.buffer_size()];

            let non_black = pixels
                .chunks_exact(4)
                .filter(|px| px[0] != 0 || px[1] != 0 || px[2] != 0)
                .count();
            let total = pixels.len() / 4;
            assert!(
                non_black * 100 >= total,
                "expected at least 1% non-black pixels, got {non_black}/{total}"
            );

            println!("wrote desktop capture to {}", path.display());
        }
    }
}

#[cfg(windows)]
pub use imp::capture_window_png;

#[cfg(not(windows))]
pub fn capture_window_png(_path: &std::path::Path) -> anyhow::Result<(u32, u32)> {
    Err(anyhow::anyhow!(
        "window capture is only implemented on Windows"
    ))
}
