//! Per-tab favicons for the native tab strip.
//!
//! The icon comes from WebView2's OWN favicon store (`ICoreWebView2_15`): the engine
//! already resolved `<link rel=icon>` / `/favicon.ico` and fetched it with the tab's
//! cookies, so asking it costs no extra request and — unlike a third-party favicon
//! service — tells nobody else what you're browsing. It hands back a PNG stream,
//! which we decode once to RGBA and keep on the tab; the painter box-filters it down
//! into the tab cell each frame (icons are ~16-64px, cells ~13px).

use std::sync::{Arc, Mutex};

/// A decoded favicon: straight (non-premultiplied) RGBA8, row-major, `w * h * 4` bytes.
pub(crate) struct Icon {
    pub(crate) w: usize,
    pub(crate) h: usize,
    pub(crate) rgba: Vec<u8>,
}

/// The slot a tab's favicon lives in: written by the WebView2 favicon callback (UI
/// thread), read by the painter. The inner `Arc` lets a frame take a cheap handle to
/// the pixels instead of copying them out of the lock.
pub(crate) type SharedIcon = Arc<Mutex<Option<Arc<Icon>>>>;

/// Decode a PNG (what WebView2 hands us) into straight RGBA8. `None` for anything
/// that doesn't parse — a missing icon is cosmetic, so every failure is silent.
pub(crate) fn decode_png(bytes: &[u8]) -> Option<Icon> {
    // `Cursor`, not the bare slice: png 0.18's reader wants `Read + Seek`.
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // Expand palette/low-bit-depth images and drop 16-bit channels to 8, so the frame
    // below is always one of the four 8-bit colour types handled here.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let px = w * h;
    let mut rgba = vec![0u8; px * 4];
    match info.color_type {
        png::ColorType::Rgba => rgba.copy_from_slice(&buf[..px * 4]),
        png::ColorType::Rgb => {
            for i in 0..px {
                rgba[i * 4..i * 4 + 3].copy_from_slice(&buf[i * 3..i * 3 + 3]);
                rgba[i * 4 + 3] = 0xff;
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for i in 0..px {
                let (g, a) = (buf[i * 2], buf[i * 2 + 1]);
                rgba[i * 4..i * 4 + 4].copy_from_slice(&[g, g, g, a]);
            }
        }
        png::ColorType::Grayscale => {
            for i in 0..px {
                let g = buf[i];
                rgba[i * 4..i * 4 + 4].copy_from_slice(&[g, g, g, 0xff]);
            }
        }
        // `normalize_to_color8` expands Indexed, so this is unreachable in practice.
        png::ColorType::Indexed => return None,
    }
    Some(Icon { w, h, rgba })
}

/// Blit `icon` into `buf` as a `size`×`size` square with its top-left at (`x`, `y`),
/// clipped to the buffer and alpha-blended over whatever is already there.
///
/// Each destination pixel averages the whole source box that maps to it (a box
/// filter), which is what keeps a 32px icon legible at 13px — nearest-neighbour
/// sampling drops most of the glyph and reads as noise at that scale. Averaging is
/// done in PREMULTIPLIED space so fully transparent source pixels (which often carry
/// an arbitrary colour) can't tint the visible ones.
pub(crate) fn blit(
    icon: &Icon,
    buf: &mut [u32],
    bw: usize,
    bh: usize,
    x: i32,
    y: i32,
    size: usize,
) {
    if size == 0 || icon.w == 0 || icon.h == 0 {
        return;
    }
    for dy in 0..size {
        let py = y + dy as i32;
        if py < 0 || py as usize >= bh {
            continue;
        }
        let (sy0, sy1) = span(dy, size, icon.h);
        for dx in 0..size {
            let px = x + dx as i32;
            if px < 0 || px as usize >= bw {
                continue;
            }
            let (sx0, sx1) = span(dx, size, icon.w);
            let (mut r, mut g, mut b, mut a, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let i = (sy * icon.w + sx) * 4;
                    let sa = icon.rgba[i + 3] as u32;
                    r += icon.rgba[i] as u32 * sa;
                    g += icon.rgba[i + 1] as u32 * sa;
                    b += icon.rgba[i + 2] as u32 * sa;
                    a += sa;
                    n += 1;
                }
            }
            if n == 0 || a == 0 {
                continue;
            }
            // Un-premultiply the averaged colour, then composite at the averaged alpha.
            let (sr, sg, sb) = (r / a, g / a, b / a);
            let alpha = a / n;
            let idx = py as usize * bw + px as usize;
            let dst = buf[idx];
            let inv = 255 - alpha;
            let out_r = (sr * alpha + ((dst >> 16) & 0xff) * inv) / 255;
            let out_g = (sg * alpha + ((dst >> 8) & 0xff) * inv) / 255;
            let out_b = (sb * alpha + (dst & 0xff) * inv) / 255;
            buf[idx] = (out_r << 16) | (out_g << 8) | out_b;
        }
    }
}

/// The half-open source range `[lo, hi)` that destination index `d` of `dst_len`
/// covers in a `src_len`-long axis. Always at least one sample wide, so upscaling a
/// tiny icon still reads a pixel instead of an empty box.
fn span(d: usize, dst_len: usize, src_len: usize) -> (usize, usize) {
    let lo = d * src_len / dst_len;
    let hi = (d + 1) * src_len / dst_len;
    (lo, hi.max(lo + 1).min(src_len))
}

/// Ask WebView2 for this webview's current favicon, and for every later change.
///
/// Best-effort, exactly like the other COM glue: an engine handle we can't get, an
/// interface the installed runtime is too old for, or a failed registration simply
/// means this tab shows no icon. Each delivery decodes into `slot` and posts
/// [`UserEvent::Redraw`](crate::UserEvent::Redraw) so the strip repaints.
#[cfg(windows)]
pub(crate) fn install(
    webview: &wry::WebView,
    slot: SharedIcon,
    proxy: tao::event_loop::EventLoopProxy<crate::UserEvent>,
) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2, ICoreWebView2_15};
    use webview2_com::FaviconChangedEventHandler;
    use windows_core::Interface;
    use wry::WebViewExtWindows;

    let Ok(core) = (unsafe { webview.controller().CoreWebView2() }) else { return };
    let Ok(c15) = core.cast::<ICoreWebView2_15>() else { return };
    // The page may already have one (a restored/cached document fires no change event).
    fetch(&c15, &slot, &proxy);
    let handler = FaviconChangedEventHandler::create(Box::new(
        move |sender: Option<ICoreWebView2>, _| {
            if let Some(c15) = sender.and_then(|s| s.cast::<ICoreWebView2_15>().ok()) {
                fetch(&c15, &slot, &proxy);
            }
            Ok(())
        },
    ));
    let mut token = 0i64;
    let _ = unsafe { c15.add_FaviconChanged(&handler, &mut token) };
}

/// One favicon read: clear the slot when the page declares no icon, else pull the PNG
/// and decode it. Async — the completion handler runs later on the UI thread.
#[cfg(windows)]
fn fetch(
    c15: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_15,
    slot: &SharedIcon,
    proxy: &tao::event_loop::EventLoopProxy<crate::UserEvent>,
) {
    use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_FAVICON_IMAGE_FORMAT_PNG;
    use webview2_com::{take_pwstr, GetFaviconCompletedHandler};
    use windows_core::PWSTR;

    // No declared icon → drop the previous page's, rather than leaving it on a site
    // that has none.
    let uri = unsafe {
        let mut p = PWSTR::null();
        if c15.FaviconUri(&mut p).is_err() {
            return;
        }
        take_pwstr(p)
    };
    if uri.is_empty() {
        if let Ok(mut g) = slot.lock() {
            if g.take().is_some() {
                let _ = proxy.send_event(crate::UserEvent::Redraw);
            }
        }
        return;
    }
    let (slot, proxy) = (slot.clone(), proxy.clone());
    let handler = GetFaviconCompletedHandler::create(Box::new(move |hr, stream| {
        let Some(icon) = hr
            .is_ok()
            .then_some(stream)
            .flatten()
            .and_then(|s| decode_png(&read_stream(&s)))
        else {
            return Ok(());
        };
        if let Ok(mut g) = slot.lock() {
            *g = Some(Arc::new(icon));
        }
        let _ = proxy.send_event(crate::UserEvent::Redraw);
        Ok(())
    }));
    let _ = unsafe { c15.GetFavicon(COREWEBVIEW2_FAVICON_IMAGE_FORMAT_PNG, &handler) };
}

/// Drain a COM stream into a byte vector. Capped so a hostile/huge "icon" can't be
/// read into memory without bound — real favicons are a few KB.
#[cfg(windows)]
fn read_stream(stream: &windows061::Win32::System::Com::IStream) -> Vec<u8> {
    const CAP: usize = 1 << 20;
    let mut out = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let mut read = 0u32;
        let hr = unsafe {
            stream.Read(chunk.as_mut_ptr() as *mut core::ffi::c_void, chunk.len() as u32, Some(&mut read))
        };
        if hr.is_err() || read == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..read as usize]);
        if out.len() >= CAP {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2×2 icon: opaque red, opaque blue, and two fully transparent pixels whose
    /// (green) colour must not bleed into the result.
    fn checker() -> Icon {
        Icon {
            w: 2,
            h: 2,
            rgba: vec![
                0xff, 0x00, 0x00, 0xff, 0x00, 0x00, 0xff, 0xff, // red, blue
                0x00, 0xff, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00, // transparent green ×2
            ],
        }
    }

    #[test]
    fn span_covers_every_source_pixel_exactly_once() {
        // Downscale 32 → 13: the spans must tile [0, 32) with no gap or overlap.
        let (dst, src) = (13usize, 32usize);
        let mut at = 0;
        for d in 0..dst {
            let (lo, hi) = span(d, dst, src);
            assert_eq!(lo, at, "gap/overlap at {d}");
            assert!(hi > lo);
            at = hi;
        }
        assert_eq!(at, src);
        // Upscaling still reads one real pixel per destination pixel.
        for d in 0..8 {
            let (lo, hi) = span(d, 8, 2);
            assert!(hi == lo + 1 && hi <= 2);
        }
    }

    #[test]
    fn blit_averages_and_ignores_transparent_pixels() {
        let mut buf = vec![0u32; 4];
        // The whole 2×2 icon collapses into one pixel: half red + half blue at 50%
        // alpha over black. The transparent green pixels contribute alpha but no hue.
        blit(&checker(), &mut buf, 2, 2, 0, 0, 1);
        let p = buf[0];
        let (r, g, b) = ((p >> 16) & 0xff, (p >> 8) & 0xff, p & 0xff);
        assert_eq!(g, 0, "transparent green bled into the result");
        assert!(r > 0 && b > 0 && r.abs_diff(b) <= 1, "red/blue should average evenly: {r},{b}");
        // 50% coverage over black ≈ a quarter of full intensity on each channel.
        assert!((50..=80).contains(&r), "unexpected alpha compositing: {r}");
        // Pixels outside the 1×1 blit are untouched.
        assert_eq!(&buf[1..], &[0, 0, 0]);
    }

    #[test]
    fn blit_clips_instead_of_panicking() {
        let mut buf = vec![0u32; 16];
        blit(&checker(), &mut buf, 4, 4, -2, -2, 3); // straddling the top-left corner
        blit(&checker(), &mut buf, 4, 4, 3, 3, 8); // running off the bottom-right
        blit(&checker(), &mut buf, 4, 4, 0, 0, 0); // degenerate size
    }

    #[test]
    fn decode_png_rejects_junk() {
        assert!(decode_png(b"").is_none());
        assert!(decode_png(b"not a png at all").is_none());
    }

    /// Encode a 2×1 image in `color`/`depth` and hand back the PNG bytes.
    fn encode(color: png::ColorType, depth: png::BitDepth, data: &[u8], palette: Option<Vec<u8>>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, 2, 1);
        enc.set_color(color);
        enc.set_depth(depth);
        if let Some(p) = palette {
            enc.set_palette(p);
        }
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(data).unwrap();
        writer.finish().unwrap();
        out
    }

    /// Every colour type WebView2 might hand back must come out as straight RGBA —
    /// palette and 16-bit images included, which only works because the decoder asks
    /// png for the normalized 8-bit form.
    #[test]
    fn decode_png_normalizes_every_colour_type_to_rgba() {
        let red_then_blue = [0xffu8, 0, 0, 0xff, 0, 0, 0xff, 0xff];
        let cases: Vec<Vec<u8>> = vec![
            // Straight RGBA.
            encode(png::ColorType::Rgba, png::BitDepth::Eight, &red_then_blue, None),
            // RGB (no alpha channel) — must come out fully opaque.
            encode(png::ColorType::Rgb, png::BitDepth::Eight, &[0xff, 0, 0, 0, 0, 0xff], None),
            // Palette — png expands it for us.
            encode(
                png::ColorType::Indexed,
                png::BitDepth::Eight,
                &[0, 1],
                Some(vec![0xff, 0, 0, 0, 0, 0xff]),
            ),
            // 16-bit RGB — stripped down to 8.
            encode(
                png::ColorType::Rgb,
                png::BitDepth::Sixteen,
                &[0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff],
                None,
            ),
        ];
        for (i, bytes) in cases.iter().enumerate() {
            let icon = decode_png(bytes).unwrap_or_else(|| panic!("case {i} failed to decode"));
            assert_eq!((icon.w, icon.h), (2, 1), "case {i}");
            assert_eq!(icon.rgba, red_then_blue, "case {i} is not opaque red then blue");
        }
    }

    /// Grayscale-with-alpha keeps its transparency (the channel the tab strip needs to
    /// composite an icon over the bar).
    #[test]
    fn decode_png_keeps_grayscale_alpha() {
        let bytes = encode(png::ColorType::GrayscaleAlpha, png::BitDepth::Eight, &[0x80, 0xff, 0x40, 0x00], None);
        let icon = decode_png(&bytes).unwrap();
        assert_eq!(icon.rgba, [0x80, 0x80, 0x80, 0xff, 0x40, 0x40, 0x40, 0x00]);
    }
}
