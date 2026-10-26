//! Decoded placed images, shared by every [`crate::Renderer`] in the process (canvas worker,
//! thumbnails, exports), so each asset is decoded once and each mip level built once.
//!
//! Entries are keyed by `(ptr, len, page, level)` of the asset's encoded bytes (`Arc<Vec<u8>>`) and
//! hold a clone of that `Arc`, so the address can't be reused while cached. Least recently used
//! levels are evicted beyond a memory budget; at most `MAX_BAD` failed assets are remembered.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use vello_cpu::Pixmap;

use crate::decode_pixmap_page;

/// Decoded pixels kept (bytes).
#[cfg(target_arch = "wasm32")]
const BUDGET: usize = 192 << 20;
#[cfg(not(target_arch = "wasm32"))]
const BUDGET: usize = 768 << 20;

/// Failed assets remembered; the oldest is forgotten (and retried if drawn again) beyond this.
const MAX_BAD: usize = 64;

struct Entry {
    _bytes: Arc<Vec<u8>>,
    pm: Arc<Pixmap>,
    stamp: u64,
}

#[derive(Default)]
struct Cache {
    /// Full-resolution `(width, height)` per `(bytes address, bytes len, PDF page)` so `mip` can
    /// compute the target level and look it up directly before decoding level 0.
    dims: HashMap<(usize, usize, u32), (u16, u16, Weak<Vec<u8>>)>,
    /// `(bytes address, bytes len, PDF page, mip level)`.
    map: HashMap<(usize, usize, u32, u8), Entry>,
    bytes: usize,
    clock: u64,
    /// Assets that failed to decode (don't retry every frame).
    /// Value: `(insertion stamp, the encoded bytes keeping the address reserved)`.
    bad: HashMap<(usize, usize, u32), (u64, Arc<Vec<u8>>)>,
    /// The colour settings the pixels were converted with ([`designcraft_color::cms::generation`]).
    colour: u64,
}

impl Cache {
    /// Remember a failed asset, evicting the oldest failures beyond [`MAX_BAD`].
    fn mark_bad(&mut self, key: (usize, usize, u32), bytes: &Arc<Vec<u8>>) {
        self.clock += 1;
        self.bad.insert(key, (self.clock, bytes.clone()));
        while self.bad.len() > MAX_BAD {
            let Some(oldest) = self.bad.iter().min_by_key(|(_, (stamp, _))| *stamp).map(|(k, _)| *k) else { break };
            self.bad.remove(&oldest);
        }
    }

    fn record_dims(&mut self, bytes: &Arc<Vec<u8>>, page: u32, w: u16, h: u16) {
        if self.dims.len() >= 4096 {
            self.dims.retain(|_, (_, _, weak)| weak.strong_count() > 0);
        }
        let key = (Arc::as_ptr(bytes) as usize, bytes.len(), page);
        self.dims.insert(key, (w, h, Arc::downgrade(bytes)));
    }
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn size_of(pm: &Pixmap) -> usize {
    usize::from(pm.width()) * usize::from(pm.height()) * 4
}

fn with<R>(f: impl FnOnce(&mut Cache) -> R) -> R {
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let c = g.get_or_insert_with(Cache::default);
    // Decoding converts CMYK through the working space: new colour settings, new pixels.
    let colour = designcraft_color::cms::generation();
    if c.colour != colour {
        *c = Cache { clock: c.clock, colour, ..Cache::default() };
    }
    f(c)
}

fn lookup(key: (usize, usize, u32, u8)) -> Option<Arc<Pixmap>> {
    with(|c| {
        c.clock += 1;
        let clock = c.clock;
        c.map.get_mut(&key).map(|e| {
            e.stamp = clock;
            e.pm.clone()
        })
    })
}

fn store(bytes: &Arc<Vec<u8>>, page: u32, level: u8, pm: Arc<Pixmap>) {
    with(|c| {
        c.clock += 1;
        if level == 0 {
            c.record_dims(bytes, page, pm.width(), pm.height());
        }
        let key = (Arc::as_ptr(bytes) as usize, bytes.len(), page, level);
        let n = size_of(&pm);
        if let Some(old) = c.map.insert(key, Entry { _bytes: bytes.clone(), pm, stamp: c.clock }) {
            c.bytes = c.bytes.saturating_sub(size_of(&old.pm));
        }
        c.bytes = c.bytes.saturating_add(n);
        if c.bytes > BUDGET {
            let mut v: Vec<(u64, (usize, usize, u32, u8))> = c.map.iter().map(|(k, e)| (e.stamp, *k)).collect();
            v.sort_unstable();
            for (_, k) in v {
                if c.bytes <= BUDGET * 3 / 4 {
                    break;
                }
                if k == key {
                    continue;
                }
                if let Some(e) = c.map.remove(&k) {
                    c.bytes = c.bytes.saturating_sub(size_of(&e.pm));
                }
            }
        }
    });
}

fn target_mip(w0: u16, h0: u16, width_pt: f64, on_screen: f64) -> (u8, u16, u16) {
    let safe_w = if width_pt.is_finite() { width_pt.max(1e-6) } else { 1e-6 };
    let safe_screen = if on_screen.is_finite() { on_screen.max(0.0) } else { 0.0 };
    let ppt = f64::from(w0) / safe_w;
    let (mut mw, mut mh) = (w0, h0);
    let mut level = 0u8;
    while ppt / (1u64 << (level + 1)) as f64 >= safe_screen * 1.2 && mw > 64 && mh > 64 && level < 8 {
        level += 1;
        mw = (mw / 2).max(1);
        mh = (mh / 2).max(1);
    }
    (level, mw, mh)
}

/// Box-filter downsample of premultiplied RGBA8 `src` to `(nw, nh)` in a single SIMD pass.
fn resize_pixmap(src: &Pixmap, nw: u16, nh: u16) -> Pixmap {
    let (nw, nh) = (nw.max(1), nh.max(1));
    if src.width() == nw && src.height() == nh {
        return src.clone();
    }
    let mut dst = Pixmap::new(nw, nh);
    if let (Ok(src_view), Ok(mut dst_view)) = (
        ImageRef::new(u32::from(src.width()), u32::from(src.height()), src.data_as_u8_slice(), PixelType::U8x4),
        Image::from_slice_u8(u32::from(nw), u32::from(nh), dst.data_as_u8_slice_mut(), PixelType::U8x4),
    ) {
        let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Box)).use_alpha(false);
        let _ = Resizer::new().resize(&src_view, &mut dst_view, &opts);
    }
    dst
}

fn decode_uncached(bytes: &Arc<Vec<u8>>, page: u32) -> Option<Pixmap> {
    let key = (Arc::as_ptr(bytes) as usize, bytes.len(), page);
    if with(|c| c.bad.contains_key(&key)) {
        return None;
    }
    // Decode outside the lock: other threads keep drawing cached images meanwhile.
    match decode_pixmap_page(bytes, page) {
        Some(pm) => {
            let (w, h) = (pm.width(), pm.height());
            with(|c| c.record_dims(bytes, page, w, h));
            Some(pm)
        }
        None => {
            with(|c| c.mark_bad(key, bytes));
            None
        }
    }
}

/// Full-resolution decode of `bytes` (cached).
pub fn decoded(bytes: &Arc<Vec<u8>>, page: u32) -> Option<Arc<Pixmap>> {
    let id = Arc::as_ptr(bytes) as usize;
    let len = bytes.len();
    if let Some(pm) = lookup((id, len, page, 0)) {
        return Some(pm);
    }
    let pm = Arc::new(decode_uncached(bytes, page)?);
    store(bytes, page, 0, pm.clone());
    Some(pm)
}

/// The mip level of `bytes` closest above `on_screen` device pixels per point for a graphic
/// `width_pt` points wide.
pub fn mip(bytes: &Arc<Vec<u8>>, page: u32, width_pt: f64, on_screen: f64) -> Option<Arc<Pixmap>> {
    let id = Arc::as_ptr(bytes) as usize;
    let len = bytes.len();
    let weak_probe = Arc::downgrade(bytes);
    let cached_dims = with(|c| {
        if c.bad.contains_key(&(id, len, page)) {
            return Some(None);
        }
        c.dims.get(&(id, len, page)).and_then(|(w, h, weak)| weak.ptr_eq(&weak_probe).then_some(Some((*w, *h))))
    });
    if let Some(dims_opt) = cached_dims {
        let (w0, h0) = dims_opt?;
        let (level, mw, mh) = target_mip(w0, h0, width_pt, on_screen);
        if let Some(pm) = lookup((id, len, page, level)) {
            return Some(pm);
        }
        if level == 0 {
            return decoded(bytes, page);
        }
        let scaled = if let Some(full) = lookup((id, len, page, 0)) {
            Arc::new(resize_pixmap(&full, mw, mh))
        } else {
            let full = decode_uncached(bytes, page)?;
            Arc::new(resize_pixmap(&full, mw, mh))
        };
        store(bytes, page, level, scaled.clone());
        return Some(scaled);
    }

    let full = decode_uncached(bytes, page)?;
    let (level, mw, mh) = target_mip(full.width(), full.height(), width_pt, on_screen);
    if level == 0 {
        let pm = Arc::new(full);
        store(bytes, page, 0, pm.clone());
        Some(pm)
    } else {
        let scaled = Arc::new(resize_pixmap(&full, mw, mh));
        store(bytes, page, level, scaled.clone());
        Some(scaled)
    }
}

/// Evict cached entries whose underlying `Arc<Vec<u8>>` is held *only* by this cache (no open
/// document or undo snapshot references the asset any longer).
pub fn trim_unused_images() {
    with(|c| {
        let mut internal_refs: HashMap<usize, usize> = HashMap::new();
        for e in c.map.values() {
            *internal_refs.entry(Arc::as_ptr(&e._bytes) as usize).or_insert(0) += 1;
        }
        for (_, b) in c.bad.values() {
            *internal_refs.entry(Arc::as_ptr(b) as usize).or_insert(0) += 1;
        }
        c.map.retain(|_, e| {
            let own = internal_refs.get(&(Arc::as_ptr(&e._bytes) as usize)).copied().unwrap_or(0);
            let keep = Arc::strong_count(&e._bytes) > own;
            if !keep {
                c.bytes = c.bytes.saturating_sub(size_of(&e.pm));
            }
            keep
        });
        c.bad.retain(|_, (_, b)| {
            let own = internal_refs.get(&(Arc::as_ptr(b) as usize)).copied().unwrap_or(0);
            Arc::strong_count(b) > own
        });
        c.dims.retain(|_, (_, _, weak)| weak.strong_count() > 0);
    });
}

/// Drop every decoded image.
pub fn clear() {
    with(|c| {
        c.dims.clear();
        c.map.clear();
        c.bad.clear();
        c.bytes = 0;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_assets_are_capped_and_oldest_is_dropped() {
        let mut c = Cache::default();
        let buffers: Vec<Arc<Vec<u8>>> = (0..MAX_BAD + 10).map(|i| Arc::new(vec![i as u8; 4])).collect();
        for b in &buffers {
            c.mark_bad((Arc::as_ptr(b) as usize, b.len(), 0), b);
        }
        assert_eq!(c.bad.len(), MAX_BAD);
        let first = &buffers[0];
        assert!(!c.bad.contains_key(&(Arc::as_ptr(first) as usize, first.len(), 0)));
        assert_eq!(Arc::strong_count(first), 1, "evicted buffer must be released");
        let last = &buffers[MAX_BAD + 9];
        assert!(c.bad.contains_key(&(Arc::as_ptr(last) as usize, last.len(), 0)));
    }

    #[test]
    fn mip_lookup_hits_directly_without_redecoding_evicted_level_zero() {
        // Synthetic asset whose raw bytes are intentionally undecodable: if `mip` tried to decode
        // level 0 when level 0 is absent, it would return `None`. With `dims` + level 2 cached,
        // `mip` must hit level 2 directly without touching the decoder.
        let fake_bytes = Arc::new(vec![0xde, 0xad, 0xbe, 0xef, 0x01]);
        let id = Arc::as_ptr(&fake_bytes) as usize;
        let len = fake_bytes.len();
        let cached_mip = Arc::new(Pixmap::new(64, 64));
        with(|c| {
            c.record_dims(&fake_bytes, 0, 256, 256);
        });
        store(&fake_bytes, 0, 2, cached_mip.clone());
        // Ensure level 0 is not in `map`.
        assert!(lookup((id, len, 0, 0)).is_none());

        // 256 px across 64 pt = 4 px/pt; at on_screen = 0.5 -> level 2 (64x64).
        let got = mip(&fake_bytes, 0, 64.0, 0.5).expect("direct mip hit without level 0");
        assert!(Arc::ptr_eq(&got, &cached_mip));

        // Also verify real PNG SIMD downsample through `mip`.
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(256, 256, image::Rgba([40, 120, 200, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let real = Arc::new(png);
        let real_id = Arc::as_ptr(&real) as usize;
        let real_len = real.len();
        let m = mip(&real, 0, 64.0, 0.5).expect("decoded and SIMD-downsampled mip");
        assert_eq!((m.width(), m.height()), (64, 64));
        assert!(lookup((real_id, real_len, 0, 0)).is_none(), "level 0 should not bloat cache when zoomed out");
        assert!(lookup((real_id, real_len, 0, 2)).is_some());
    }

    #[test]
    fn trim_unused_images_evicts_dropped_assets_across_multiple_levels_and_keeps_live_ones() {
        let dropped = Arc::new(vec![7u8; 16]);
        let dropped_weak = Arc::downgrade(&dropped);
        let live = Arc::new(vec![9u8; 24]);

        with(|c| {
            c.record_dims(&dropped, 0, 256, 256);
            c.record_dims(&live, 0, 256, 256);
        });
        store(&dropped, 0, 0, Arc::new(Pixmap::new(32, 32)));
        store(&dropped, 0, 2, Arc::new(Pixmap::new(8, 8)));
        store(&live, 0, 0, Arc::new(Pixmap::new(16, 16)));
        store(&live, 0, 1, Arc::new(Pixmap::new(8, 8)));

        let dropped_key0 = (Arc::as_ptr(&dropped) as usize, dropped.len(), 0, 0);
        let dropped_key2 = (Arc::as_ptr(&dropped) as usize, dropped.len(), 0, 2);
        let live_key0 = (Arc::as_ptr(&live) as usize, live.len(), 0, 0);
        let live_key1 = (Arc::as_ptr(&live) as usize, live.len(), 0, 1);

        drop(dropped);
        // `Cache` itself still holds two strong refs (level 0 and level 2), so naive strong_count > 1 would leak!
        assert_eq!(dropped_weak.strong_count(), 2);

        trim_unused_images();

        assert_eq!(dropped_weak.strong_count(), 0, "multi-level dropped asset must be evicted");
        assert!(lookup(dropped_key0).is_none());
        assert!(lookup(dropped_key2).is_none());
        assert!(lookup(live_key0).is_some(), "live asset level 0 must remain cached");
        assert!(lookup(live_key1).is_some(), "live asset level 1 must remain cached");
    }
}
