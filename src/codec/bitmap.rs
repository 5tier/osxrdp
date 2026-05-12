/// RDP Bitmap Update encoder.
///
/// Encodes a full-screen BGRA frame into an RDP UpdateBitmapPDU payload.
/// Phase 1: uncompressed, tiled into 64×64 rectangles (simplest possible).
/// Phase 2: add RLE compression (saves ~40-60% bandwidth for typical desktops).
/// Phase 3: replace with H.264 via VideoToolbox for high-refresh sessions.

use crate::capture::Frame;
use anyhow::Result;

const TILE: u16 = 64;

pub struct BitmapEncoder {
    width: u16,
    height: u16,
}

impl BitmapEncoder {
    pub fn new(width: u16, height: u16) -> Self {
        Self { width, height }
    }

    /// Returns a Vec<u8> ready to be written as the body of an
    /// TS_FP_UPDATE_BITMAP / TS_BITMAP_UPDATE payload.
    pub fn encode(&mut self, frame: &Frame) -> Result<Vec<u8>> {
        let mut tiles: Vec<Tile> = Vec::new();

        let mut y = 0u16;
        while y < self.height {
            let th = TILE.min(self.height - y);
            let mut x = 0u16;
            while x < self.width {
                let tw = TILE.min(self.width - x);
                let data = extract_tile(&frame.data, frame.width, x, y, tw, th);
                tiles.push(Tile { x, y, w: tw, h: th, bpp: 32, data });
                x += TILE;
            }
            y += TILE;
        }

        serialize_bitmap_update(tiles)
    }
}

struct Tile {
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    bpp: u16,
    data: Vec<u8>,
}

fn extract_tile(src: &[u8], src_w: u32, x: u16, y: u16, w: u16, h: u16) -> Vec<u8> {
    let stride = src_w as usize * 4; // BGRA
    let mut out = Vec::with_capacity(w as usize * h as usize * 4);
    for row in 0..h as usize {
        let start = (y as usize + row) * stride + x as usize * 4;
        out.extend_from_slice(&src[start..start + w as usize * 4]);
    }
    out
}

/// Serialises tiles into a minimal TS_BITMAP_UPDATE wire format.
/// Reference: MS-RDPBCGR §2.2.9.1.1.3.1
fn serialize_bitmap_update(tiles: Vec<Tile>) -> Result<Vec<u8>> {
    use byteorder::{LE, WriteBytesExt};
    let mut buf = Vec::new();

    // updateType = UPDATETYPE_BITMAP (0x0001)
    buf.write_u16::<LE>(0x0001)?;
    // numberRectangles
    buf.write_u16::<LE>(tiles.len() as u16)?;

    for tile in tiles {
        // TS_BITMAP_DATA header
        buf.write_u16::<LE>(tile.x)?;
        buf.write_u16::<LE>(tile.y)?;
        buf.write_u16::<LE>(tile.x + tile.w - 1)?; // right
        buf.write_u16::<LE>(tile.y + tile.h - 1)?; // bottom
        buf.write_u16::<LE>(tile.w)?;
        buf.write_u16::<LE>(tile.h)?;
        buf.write_u16::<LE>(tile.bpp)?;
        // flags: NO_COMPRESSION (0x0000)
        buf.write_u16::<LE>(0x0000)?;
        // bitmapLength
        buf.write_u16::<LE>(tile.data.len() as u16)?;
        buf.extend_from_slice(&tile.data);
    }

    Ok(buf)
}
