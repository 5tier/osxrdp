use std::collections::VecDeque;
use std::num::{NonZeroU16, NonZeroUsize};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use ironrdp_graphics::diff::find_different_rects_sub;
use ironrdp_server::{BitmapUpdate, DesktopSize, DisplayUpdate, PixelFormat};
use tracing::debug;

use crate::display::AspectMode;

struct PrevFrame {
    data: Bytes,
    stride: usize,
    width: usize,
    height: usize,
}

/// Processes raw BGRA frames into RDP display updates.
///
/// Owns the letterbox transform, dirty-region detection, and the sub-region
/// update queue. Callers push raw pixel data in; updates drain one at a time.
pub struct BgraFramePipeline {
    target_size: DesktopSize,
    aspect_mode: AspectMode,
    prev: Option<PrevFrame>,
    pending: VecDeque<BitmapUpdate>,
}

impl BgraFramePipeline {
    pub fn new(target_size: DesktopSize, aspect_mode: AspectMode) -> Self {
        Self {
            target_size,
            aspect_mode,
            prev: None,
            pending: VecDeque::new(),
        }
    }

    pub fn set_target(&mut self, size: DesktopSize) {
        self.target_size = size;
    }

    /// Clear frame history and pending updates. Call after stream recreation.
    pub fn reset(&mut self) {
        self.prev = None;
        self.pending.clear();
    }

    /// Drain one queued update. Returns `None` when all updates for the last
    /// pushed frame have been consumed.
    pub fn pop_update(&mut self) -> Option<DisplayUpdate> {
        self.pending.pop_front().map(DisplayUpdate::Bitmap)
    }

    /// Process a raw BGRA frame. Queues zero or more updates that can be
    /// drained via `pop_update`.
    pub fn push_frame(&mut self, data: &[u8], src_w: usize, src_h: usize, src_stride: usize) {
        let (data, w, h, stride) = if self.needs_crop_scale(src_w, src_h) {
            self.letterbox_bgra(data, src_w, src_h, src_stride)
        } else {
            (Bytes::copy_from_slice(data), src_w, src_h, src_stride)
        };

        let new_bitmap = match make_bitmap(data.clone(), w, h, stride) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("make_bitmap error: {e}");
                return;
            }
        };

        // First frame or resolution change → full refresh
        let needs_full = match &self.prev {
            None => {
                debug!("First frame at {}×{} (full refresh)", w, h);
                true
            }
            Some(p) if p.width != w || p.height != h => {
                debug!("Resolution change: {}×{} → {}×{} (full refresh)", p.width, p.height, w, h);
                true
            }
            _ => false,
        };

        if needs_full {
            self.prev = Some(PrevFrame { data, stride, width: w, height: h });
            self.pending.push_back(new_bitmap);
            return;
        }

        let prev = self.prev.as_ref().unwrap();

        let diffs = find_different_rects_sub::<4>(
            &prev.data, prev.stride, prev.width, prev.height,
            &data, stride, w, h, 0, 0,
        );

        self.prev = Some(PrevFrame { data, stride, width: w, height: h });

        if diffs.is_empty() {
            return;
        }

        debug!("{} dirty rect(s) this frame", diffs.len());

        // Large change — send the whole frame rather than many small rects.
        let total_dirty: u64 = diffs.iter().map(|r| r.width as u64 * r.height as u64).sum();
        if total_dirty > (w as u64 * h as u64) / 2 {
            self.pending.push_back(new_bitmap);
            return;
        }

        for rect in &diffs {
            let Some(rw) = NonZeroU16::new(rect.width as u16) else { continue };
            let Some(rh) = NonZeroU16::new(rect.height as u16) else { continue };
            if let Some(sub) = new_bitmap.sub(rect.x as u16, rect.y as u16, rw, rh) {
                self.pending.push_back(sub);
            }
        }
    }

    fn needs_crop_scale(&self, frame_w: usize, frame_h: usize) -> bool {
        if self.aspect_mode != AspectMode::Fit {
            return false;
        }
        let t = self.target_size;
        frame_w != t.width as usize || frame_h != t.height as usize
    }

    fn letterbox_bgra(
        &self,
        src: &[u8],
        src_w: usize,
        src_h: usize,
        src_stride: usize,
    ) -> (Bytes, usize, usize, usize) {
        let dst_w = self.target_size.width as usize;
        let dst_h = self.target_size.height as usize;
        let (scaled_x, scaled_y, scaled_w, scaled_h) =
            compute_letterbox_rect(src_w, src_h, dst_w, dst_h);

        let dst_stride = dst_w * 4;
        let mut dst = vec![0u8; dst_h * dst_stride];

        // Fill with opaque black (BGRA: B=0 G=0 R=0 A=255).
        for pixel in dst.chunks_exact_mut(4) {
            pixel[3] = 255;
        }

        // Nearest-neighbour scale into the letterbox region.
        for dy in 0..scaled_h {
            let src_y = dy * src_h / scaled_h;
            let src_row = src_y * src_stride;
            let dst_row = (scaled_y + dy) * dst_stride;
            for dx in 0..scaled_w {
                let src_x = dx * src_w / scaled_w;
                let si = src_row + src_x * 4;
                let di = dst_row + (scaled_x + dx) * 4;
                dst[di..di + 4].copy_from_slice(&src[si..si + 4]);
            }
        }

        (Bytes::from(dst), dst_w, dst_h, dst_stride)
    }
}

fn compute_letterbox_rect(
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
) -> (usize, usize, usize, usize) {
    let src_ratio = src_w as f64 / src_h as f64;
    let dst_ratio = dst_w as f64 / dst_h as f64;

    if (src_ratio - dst_ratio).abs() < 0.001 {
        (0, 0, dst_w, dst_h)
    } else if src_ratio > dst_ratio {
        let scaled_h = (dst_w as f64 / src_ratio).round() as usize & !1;
        let y = (dst_h.saturating_sub(scaled_h)) / 2;
        (0, y, dst_w, scaled_h)
    } else {
        let scaled_w = (dst_h as f64 * src_ratio).round() as usize & !1;
        let x = (dst_w.saturating_sub(scaled_w)) / 2;
        (x, 0, scaled_w, dst_h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::AspectMode;
    use ironrdp_server::{DesktopSize, DisplayUpdate};

    fn sz(w: u16, h: u16) -> DesktopSize {
        DesktopSize { width: w, height: h }
    }

    fn solid_bgra(w: usize, h: usize, b: u8, g: u8, r: u8) -> Vec<u8> {
        let mut data = vec![0u8; h * w * 4];
        for pixel in data.chunks_exact_mut(4) {
            pixel[0] = b;
            pixel[1] = g;
            pixel[2] = r;
            pixel[3] = 255;
        }
        data
    }

    fn push(p: &mut BgraFramePipeline, data: &[u8], w: usize, h: usize) {
        p.push_frame(data, w, h, w * 4);
    }

    fn pixel_at(data: &[u8], stride: usize, x: usize, y: usize) -> [u8; 4] {
        let off = y * stride + x * 4;
        [data[off], data[off + 1], data[off + 2], data[off + 3]]
    }

    // ─── Behavior 1: first push → full bitmap ───────────────────────────────

    #[test]
    fn first_frame_produces_full_bitmap() {
        let mut p = BgraFramePipeline::new(sz(4, 4), AspectMode::Native);
        let frame = solid_bgra(4, 4, 0, 0, 255);
        push(&mut p, &frame, 4, 4);
        let update = p.pop_update().expect("first frame must produce an update");
        let DisplayUpdate::Bitmap(b) = update else { panic!("expected Bitmap, got {update:?}") };
        assert_eq!((b.x, b.y), (0, 0));
        assert_eq!(b.width.get(), 4);
        assert_eq!(b.height.get(), 4);
    }

    // ─── Behavior 2: identical second frame → no update ─────────────────────

    #[test]
    fn identical_frame_produces_no_update() {
        let mut p = BgraFramePipeline::new(sz(4, 4), AspectMode::Native);
        let frame = solid_bgra(4, 4, 0, 0, 255);
        push(&mut p, &frame, 4, 4);
        p.pop_update();

        push(&mut p, &frame, 4, 4);
        assert!(p.pop_update().is_none());
    }

    // ─── Behavior 3: >50% pixels change → single full bitmap ────────────────

    #[test]
    fn large_change_produces_single_full_bitmap() {
        let (w, h) = (128usize, 128usize);
        let mut p = BgraFramePipeline::new(sz(w as u16, h as u16), AspectMode::Native);
        let frame1 = solid_bgra(w, h, 0, 0, 0);
        push(&mut p, &frame1, w, h);
        p.pop_update();

        let frame2 = solid_bgra(w, h, 0, 0, 255);
        push(&mut p, &frame2, w, h);

        let update = p.pop_update().expect("full change must produce update");
        let DisplayUpdate::Bitmap(b) = update else { panic!("expected Bitmap") };
        assert_eq!(b.width.get(), w as u16);
        assert_eq!(b.height.get(), h as u16);
        assert!(p.pop_update().is_none(), "fast path must not queue sub-regions");
    }

    // ─── Behavior 4: small change → sub-region covering the changed tile ────

    #[test]
    fn small_change_produces_sub_region_update() {
        let (w, h) = (128usize, 128usize);
        let stride = w * 4;
        let mut p = BgraFramePipeline::new(sz(w as u16, h as u16), AspectMode::Native);
        let frame1 = solid_bgra(w, h, 0, 0, 0);
        push(&mut p, &frame1, w, h);
        p.pop_update();

        // Change one pixel in tile (1,1) — x=65, y=65
        let mut frame2 = frame1.clone();
        frame2[65 * stride + 65 * 4] = 255;

        push(&mut p, &frame2, w, h);
        let update = p.pop_update().expect("small change must produce update");
        let DisplayUpdate::Bitmap(b) = update else { panic!("expected Bitmap") };

        assert!(b.width.get() < w as u16, "expected sub-region, not full frame");
        // Changed pixel (65,65) must be inside the reported rect
        assert!(b.x <= 65 && b.x + b.width.get() > 65);
        assert!(b.y <= 65 && b.y + b.height.get() > 65);
    }

    // ─── Behavior 5: multiple dirty tiles → drain one per pop_update() ──────

    #[test]
    fn sub_regions_drain_one_per_call() {
        let (w, h) = (128usize, 128usize);
        let stride = w * 4;
        let mut p = BgraFramePipeline::new(sz(w as u16, h as u16), AspectMode::Native);
        let frame1 = solid_bgra(w, h, 0, 0, 0);
        push(&mut p, &frame1, w, h);
        p.pop_update();

        // Dirty tile (0,0) via pixel (0,0) and tile (1,1) via pixel (64,64).
        // Total dirty = 64*64 + 64*64 = 8192 = 128*128/2, which is NOT > half,
        // so the pipeline must queue two sub-regions rather than one full frame.
        let mut frame2 = frame1.clone();
        frame2[0 * stride + 0 * 4] = 255;
        frame2[64 * stride + 64 * 4] = 255;

        push(&mut p, &frame2, w, h);

        let first = p.pop_update().expect("first sub-region");
        let second = p.pop_update().expect("second sub-region");
        assert!(p.pop_update().is_none(), "no further updates expected");

        let (DisplayUpdate::Bitmap(b1), DisplayUpdate::Bitmap(b2)) = (first, second) else {
            panic!("expected two Bitmap updates");
        };
        assert_ne!((b1.x, b1.y), (b2.x, b2.y), "sub-regions must be at different positions");
    }

    // ─── Behavior 6: source dimension change → full refresh ─────────────────

    #[test]
    fn dimension_change_triggers_full_refresh() {
        let mut p = BgraFramePipeline::new(sz(64, 64), AspectMode::Native);
        let frame1 = solid_bgra(64, 64, 0, 0, 0);
        push(&mut p, &frame1, 64, 64);
        p.pop_update();

        let frame2 = solid_bgra(128, 128, 0, 0, 255);
        push(&mut p, &frame2, 128, 128);

        let update = p.pop_update().expect("dimension change must produce update");
        let DisplayUpdate::Bitmap(b) = update else { panic!("expected Bitmap") };
        assert_eq!(b.width.get(), 128);
        assert_eq!(b.height.get(), 128);
    }

    // ─── Behavior 7: reset() → next push treated as first frame ─────────────

    #[test]
    fn reset_makes_next_push_a_full_refresh() {
        let mut p = BgraFramePipeline::new(sz(4, 4), AspectMode::Native);
        let frame = solid_bgra(4, 4, 0, 0, 0);
        push(&mut p, &frame, 4, 4);
        p.pop_update();

        p.reset();

        // Same pixel data — but after reset it is a "first frame"
        push(&mut p, &frame, 4, 4);
        assert!(p.pop_update().is_some(), "post-reset push must produce an update");
    }

    // ─── Behavior 8: Fit mode, non-matching ratio → black bars ──────────────

    #[test]
    fn fit_mode_nonmatching_ratio_produces_black_bars() {
        // Source 100×100 (1:1), target 200×100 (2:1).
        // Source is narrower than target → pillarbox: bars at x∈[0,50) and x∈[150,200).
        let mut p = BgraFramePipeline::new(sz(200, 100), AspectMode::Fit);
        let frame = solid_bgra(100, 100, 0, 0, 255); // solid red
        push(&mut p, &frame, 100, 100);

        let update = p.pop_update().expect("expected update");
        let DisplayUpdate::Bitmap(b) = update else { panic!("expected Bitmap") };
        assert_eq!(b.width.get(), 200);
        assert_eq!(b.height.get(), 100);

        let stride = b.stride.get();
        // Left bar at x=0 must be opaque black
        assert_eq!(pixel_at(&b.data, stride, 0, 0), [0, 0, 0, 255]);
        // First content pixel at x=50 must be red (BGRA: B=0, G=0, R=255, A=255)
        assert_eq!(pixel_at(&b.data, stride, 50, 0), [0, 0, 255, 255]);
    }

    // ─── Behavior 9: Fit mode, matching ratio → no black bars ───────────────

    #[test]
    fn fit_mode_matching_ratio_has_no_bars() {
        // Source 100×50 (2:1), target 200×100 (2:1) → ratios match → no bars.
        let mut p = BgraFramePipeline::new(sz(200, 100), AspectMode::Fit);
        let frame = solid_bgra(100, 50, 0, 0, 255); // solid red
        push(&mut p, &frame, 100, 50);

        let update = p.pop_update().expect("expected update");
        let DisplayUpdate::Bitmap(b) = update else { panic!("expected Bitmap") };
        assert_eq!(b.width.get(), 200);
        assert_eq!(b.height.get(), 100);

        let stride = b.stride.get();
        // x=0 must be content (red), not a black bar
        assert_eq!(pixel_at(&b.data, stride, 0, 0), [0, 0, 255, 255]);
    }

    // ─── Behavior 10: Native mode → output has source dimensions ────────────

    #[test]
    fn native_mode_ignores_target_size() {
        // Source 300×200, target 100×100 — Native mode bypasses letterboxing.
        let mut p = BgraFramePipeline::new(sz(100, 100), AspectMode::Native);
        let frame = solid_bgra(300, 200, 0, 0, 255);
        push(&mut p, &frame, 300, 200);

        let update = p.pop_update().expect("expected update");
        let DisplayUpdate::Bitmap(b) = update else { panic!("expected Bitmap") };
        assert_eq!(b.width.get(), 300, "Native mode must use source width");
        assert_eq!(b.height.get(), 200, "Native mode must use source height");
    }
}

fn make_bitmap(data: Bytes, w: usize, h: usize, stride: usize) -> Result<BitmapUpdate> {
    Ok(BitmapUpdate {
        x: 0,
        y: 0,
        width: NonZeroU16::new(w as u16).ok_or_else(|| anyhow!("zero width"))?,
        height: NonZeroU16::new(h as u16).ok_or_else(|| anyhow!("zero height"))?,
        format: PixelFormat::BgrA32,
        data,
        stride: NonZeroUsize::new(stride).ok_or_else(|| anyhow!("zero stride"))?,
    })
}
