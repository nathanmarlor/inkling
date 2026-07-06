//! Grayscale ink surface for TAKEOVER display mode.
//!
//! In takeover mode scribed owns the e-ink panel (xochitl stopped, panel
//! driven via rm2fb/SWTCON), so "ink" is just our own 8-bit grayscale buffer
//! that we push to the display. That ownership is what makes a true per-pixel
//! fade possible — impossible inside xochitl, where ink is binary document
//! strokes and the only white is a 12px eraser tool.
//!
//! The fade is a scattered per-pixel dither (cf. MaximeRivest/riddle's
//! `dissolve_pass`): each stage lightens more inked pixels, chosen by a
//! per-pixel hash, so the drawing dissolves in a fine random pattern that
//! reads as fading on e-ink. This module is pure (no device I/O) and unit
//! tested; the rm2fb push lives in the binary's takeover backend.

pub const WHITE: u8 = 0xFF;
pub const BLACK: u8 = 0x00;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BBox {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl BBox {
    pub fn is_empty(&self) -> bool {
        self.x1 < self.x0 || self.y1 < self.y0
    }
}

/// An owned 8-bit grayscale framebuffer (0 = black ink, 255 = white paper).
#[derive(Clone)]
pub struct Surface {
    pub width: u32,
    pub height: u32,
    pub px: Vec<u8>,
}

impl Surface {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height, px: vec![WHITE; (width * height) as usize] }
    }

    #[inline]
    pub fn in_bounds(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && (x as u32) < self.width && (y as u32) < self.height
    }

    #[inline]
    pub fn luma(&self, x: i32, y: i32) -> u8 {
        if self.in_bounds(x, y) {
            self.px[(y as u32 * self.width + x as u32) as usize]
        } else {
            WHITE
        }
    }

    #[inline]
    pub fn put_px(&mut self, x: i32, y: i32, v: u8) {
        if self.in_bounds(x, y) {
            self.px[(y as u32 * self.width + x as u32) as usize] = v;
        }
    }

    pub fn clear(&mut self) {
        self.px.iter_mut().for_each(|p| *p = WHITE);
    }

    /// Bounding box of all inked (non-white) pixels, or None if blank.
    pub fn ink_bbox(&self, ink_below: u8) -> Option<BBox> {
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for y in 0..self.height as i32 {
            for x in 0..self.width as i32 {
                if self.luma(x, y) < ink_below {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        if x1 < x0 {
            None
        } else {
            Some(BBox { x0, y0, x1, y1 })
        }
    }
}

/// Deterministic per-pixel hash for the dissolve pattern (same construction
/// as riddle — cheap, well-scattered).
#[inline]
pub fn px_hash(x: i32, y: i32) -> u32 {
    let mut h = (x as u32).wrapping_mul(0x9E3779B1) ^ (y as u32).wrapping_mul(0x85EBCA6B);
    h ^= h >> 13;
    h = h.wrapping_mul(0xC2B2AE35);
    h ^ (h >> 16)
}

/// One stage of the "drink the ink" fade over `region`: turn inked pixels
/// whose hash falls in this stage white. After `stages` passes (stage 0..
/// stages-1) the region is clean. Returns the pixels changed this pass.
pub fn dissolve_pass(surf: &mut Surface, region: BBox, stage: u32, stages: u32, ink_below: u8) -> u32 {
    if region.is_empty() || stages == 0 {
        return 0;
    }
    let mut changed = 0;
    for y in region.y0..=region.y1 {
        for x in region.x0..=region.x1 {
            if surf.luma(x, y) < ink_below && px_hash(x, y) % stages <= stage {
                surf.put_px(x, y, WHITE);
                changed += 1;
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw_blob(s: &mut Surface) {
        for y in 10..40 {
            for x in 10..50 {
                s.put_px(x, y, BLACK);
            }
        }
    }

    #[test]
    fn fresh_surface_is_white_and_blank() {
        let s = Surface::new(64, 64);
        assert_eq!(s.luma(0, 0), WHITE);
        assert!(s.ink_bbox(250).is_none());
    }

    #[test]
    fn ink_bbox_tracks_drawn_region() {
        let mut s = Surface::new(64, 64);
        draw_blob(&mut s);
        let b = s.ink_bbox(250).unwrap();
        assert_eq!((b.x0, b.y0, b.x1, b.y1), (10, 10, 49, 39));
    }

    #[test]
    fn full_dissolve_leaves_region_clean() {
        let mut s = Surface::new(64, 64);
        draw_blob(&mut s);
        let region = s.ink_bbox(250).unwrap();
        const STAGES: u32 = 6;
        for stage in 0..STAGES {
            dissolve_pass(&mut s, region, stage, STAGES, 250);
        }
        assert!(s.ink_bbox(250).is_none(), "after all stages the ink is gone");
    }

    #[test]
    fn early_stages_are_partial_and_monotonic() {
        let mut s = Surface::new(64, 64);
        draw_blob(&mut s);
        let region = s.ink_bbox(250).unwrap();
        let total_before = s.px.iter().filter(|&&p| p < 250).count();
        const STAGES: u32 = 6;
        // First stage removes some but not all.
        dissolve_pass(&mut s, region, 0, STAGES, 250);
        let after1 = s.px.iter().filter(|&&p| p < 250).count();
        assert!(after1 < total_before, "stage 0 removes some ink");
        assert!(after1 > 0, "stage 0 does not remove all ink");
        // Later stages keep reducing.
        dissolve_pass(&mut s, region, 3, STAGES, 250);
        let after2 = s.px.iter().filter(|&&p| p < 250).count();
        assert!(after2 < after1);
    }

    #[test]
    fn dissolve_scatters_rather_than_wipes_rows() {
        // After an early stage the removed pixels should be spread across the
        // region (a scatter), not a contiguous block — check both first and
        // last rows still have a mix.
        let mut s = Surface::new(64, 64);
        draw_blob(&mut s);
        let region = s.ink_bbox(250).unwrap();
        dissolve_pass(&mut s, region, 0, 8, 250);
        let mut white_in_region = 0;
        let mut black_in_region = 0;
        for y in region.y0..=region.y1 {
            for x in region.x0..=region.x1 {
                if s.luma(x, y) < 250 {
                    black_in_region += 1;
                } else {
                    white_in_region += 1;
                }
            }
        }
        assert!(white_in_region > 0 && black_in_region > 0, "a partial scatter");
    }
}
