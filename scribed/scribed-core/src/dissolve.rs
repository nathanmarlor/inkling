//! Dissolve planner: ink mask -> block tiling -> seeded shuffle -> ordered
//! erase strokes, per DESIGN.md §5.4. The "fade" is real erasure sequenced
//! to look like a dissolve — no framebuffer tricks. Pure logic; the ink
//! writer (device-side) just replays whatever strokes this produces.

use crate::geometry::Stroke;
use rand::seq::SliceRandom;
use rand::SeedableRng;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRegion {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl BlockRegion {
    pub fn center(&self) -> (f32, f32) {
        (self.x as f32 + self.w as f32 / 2.0, self.y as f32 + self.h as f32 / 2.0)
    }
}

/// A binary ink mask: `true` = inked pixel, row-major, `width * height` long.
pub struct InkMask<'a> {
    pub width: u32,
    pub height: u32,
    pub pixels: &'a [bool],
}

impl<'a> InkMask<'a> {
    pub fn new(width: u32, height: u32, pixels: &'a [bool]) -> Self {
        assert_eq!(pixels.len(), (width * height) as usize);
        Self { width, height, pixels }
    }

    fn block_has_ink(&self, bx: u32, by: u32, block_px: u32) -> bool {
        let x0 = bx * block_px;
        let y0 = by * block_px;
        let x1 = (x0 + block_px).min(self.width);
        let y1 = (y0 + block_px).min(self.height);
        for y in y0..y1 {
            let row = (y * self.width) as usize;
            for x in x0..x1 {
                if self.pixels[row + x as usize] {
                    return true;
                }
            }
        }
        false
    }
}

/// Tile the mask into `block_px`-square blocks, keeping only blocks that
/// contain at least one inked pixel. Cost scales with ink, not page area,
/// since callers typically only care about the returned (small) list.
pub fn ink_blocks(mask: &InkMask, block_px: u32) -> Vec<BlockRegion> {
    assert!(block_px > 0);
    let blocks_x = mask.width.div_ceil(block_px);
    let blocks_y = mask.height.div_ceil(block_px);
    let mut out = Vec::new();
    for by in 0..blocks_y {
        for bx in 0..blocks_x {
            if mask.block_has_ink(bx, by, block_px) {
                let x = bx * block_px;
                let y = by * block_px;
                out.push(BlockRegion {
                    x,
                    y,
                    w: block_px.min(mask.width - x),
                    h: block_px.min(mask.height - y),
                });
            }
        }
    }
    out
}

/// Deterministic seeded shuffle so the dissolve order is reproducible for
/// a given seed but looks scattered rather than a top-to-bottom wipe.
pub fn shuffle_blocks(blocks: &mut [BlockRegion], seed: u64) {
    let mut rng = rand_pcg::Pcg64::seed_from_u64(seed);
    blocks.shuffle(&mut rng);
}

/// A short serpentine (zigzag) stroke sweeping the block's area, used to
/// erase it with the rubber tool. `line_spacing` controls how many passes.
pub fn serpentine_stroke(block: BlockRegion, line_spacing: f32) -> Stroke {
    let mut s = Stroke::new();
    if line_spacing <= 0.0 || block.h == 0 {
        return s;
    }
    let mut y = block.y as f32;
    let bottom = (block.y + block.h) as f32;
    let (left, right) = (block.x as f32, (block.x + block.w) as f32);
    let mut left_to_right = true;
    let mut first = true;
    while y <= bottom {
        let (x_start, x_end) = if left_to_right { (left, right) } else { (right, left) };
        if first {
            s.push(x_start, y, 0.6);
            first = false;
        } else {
            s.push(x_start, y, 0.6);
        }
        s.push(x_end, y, 0.6);
        y += line_spacing;
        left_to_right = !left_to_right;
    }
    s
}

/// Full plan: shuffled blocks, each turned into an erase stroke, in the
/// order they should be injected to produce the scattered dissolve effect.
pub fn plan_dissolve(mask: &InkMask, block_px: u32, eraser_pass_spacing: f32, seed: u64) -> Vec<Stroke> {
    let mut blocks = ink_blocks(mask, block_px);
    shuffle_blocks(&mut blocks, seed);
    blocks.into_iter().map(|b| serpentine_stroke(b, eraser_pass_spacing)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask_from_ascii(rows: &[&str]) -> (u32, u32, Vec<bool>) {
        let height = rows.len() as u32;
        let width = rows[0].len() as u32;
        let mut pixels = Vec::with_capacity((width * height) as usize);
        for row in rows {
            for c in row.chars() {
                pixels.push(c == '#');
            }
        }
        (width, height, pixels)
    }

    #[test]
    fn only_inked_blocks_are_returned() {
        // 4x4 mask, 2px blocks -> 4 blocks total, only top-left has ink.
        let (w, h, px) = mask_from_ascii(&["#...", "....", "....", "...."]);
        let mask = InkMask::new(w, h, &px);
        let blocks = ink_blocks(&mask, 2);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0], BlockRegion { x: 0, y: 0, w: 2, h: 2 });
    }

    #[test]
    fn blank_page_has_no_blocks() {
        let (w, h, px) = mask_from_ascii(&["....", "....", "....", "...."]);
        let mask = InkMask::new(w, h, &px);
        assert!(ink_blocks(&mask, 2).is_empty());
    }

    #[test]
    fn edge_blocks_are_clipped_to_mask_bounds() {
        // 5x5 mask with 2px blocks -> last column/row of blocks is only 1px wide/tall.
        let rows = vec!["....#", "....#", "....#", "....#", "....#"];
        let (w, h, px) = mask_from_ascii(&rows);
        let mask = InkMask::new(w, h, &px);
        let blocks = ink_blocks(&mask, 2);
        // rightmost blocks should have w=1 (5 - 4 = 1)
        assert!(blocks.iter().all(|b| b.x + b.w <= 5 && b.y + b.h <= 5));
        assert!(blocks.iter().any(|b| b.w == 1));
    }

    #[test]
    fn shuffle_is_deterministic_per_seed() {
        let mut a = vec![
            BlockRegion { x: 0, y: 0, w: 1, h: 1 },
            BlockRegion { x: 1, y: 0, w: 1, h: 1 },
            BlockRegion { x: 2, y: 0, w: 1, h: 1 },
            BlockRegion { x: 3, y: 0, w: 1, h: 1 },
            BlockRegion { x: 4, y: 0, w: 1, h: 1 },
        ];
        let mut b = a.clone();
        shuffle_blocks(&mut a, 42);
        shuffle_blocks(&mut b, 42);
        assert_eq!(a, b, "same seed must produce the same order");

        let mut c = a.clone();
        // restore original order first
        let mut original = vec![
            BlockRegion { x: 0, y: 0, w: 1, h: 1 },
            BlockRegion { x: 1, y: 0, w: 1, h: 1 },
            BlockRegion { x: 2, y: 0, w: 1, h: 1 },
            BlockRegion { x: 3, y: 0, w: 1, h: 1 },
            BlockRegion { x: 4, y: 0, w: 1, h: 1 },
        ];
        shuffle_blocks(&mut original, 7);
        c.clone_from(&original);
        assert_ne!(a, c, "different seeds should (almost always) differ");
    }

    #[test]
    fn serpentine_stroke_covers_block_bounding_box() {
        let block = BlockRegion { x: 10, y: 10, w: 20, h: 20 };
        let stroke = serpentine_stroke(block, 5.0);
        assert!(!stroke.is_empty());
        let min_x = stroke.points.iter().map(|p| p.x).fold(f32::MAX, f32::min);
        let max_x = stroke.points.iter().map(|p| p.x).fold(f32::MIN, f32::max);
        let min_y = stroke.points.iter().map(|p| p.y).fold(f32::MAX, f32::min);
        let max_y = stroke.points.iter().map(|p| p.y).fold(f32::MIN, f32::max);
        assert_eq!(min_x, 10.0);
        assert_eq!(max_x, 30.0);
        assert_eq!(min_y, 10.0);
        assert!(max_y <= 30.0);
    }

    #[test]
    fn plan_dissolve_produces_one_stroke_per_inked_block() {
        let (w, h, px) = mask_from_ascii(&["#...", "....", "....", "..#."]);
        let mask = InkMask::new(w, h, &px);
        let strokes = plan_dissolve(&mask, 2, 3.0, 1);
        assert_eq!(strokes.len(), 2);
    }
}
