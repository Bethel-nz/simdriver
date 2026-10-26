//! Annotated screenshots: the current frame with numbered boxes around the elements
//! (or the grid's cells), encoded as PNG. Labels use a built-in 5×7 pixel font, so no
//! font files or system text APIs are involved.
use crate::marks::{Element, Visibility};
use simdriver::{Frame, Grid};

/// Distinct on light and dark UI alike.
const PALETTE: [[u8; 3]; 8] = [
    [236, 64, 122],
    [255, 145, 0],
    [0, 200, 83],
    [41, 121, 255],
    [170, 0, 255],
    [0, 184, 212],
    [255, 214, 0],
    [255, 23, 68],
];
const COVERED: [u8; 3] = [140, 140, 140];
const GRID: [u8; 3] = [236, 64, 122];

/// RGB canvas over a BGRA frame.
struct Canvas {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

impl Canvas {
    fn from_frame(f: &Frame) -> Canvas {
        let (w, h) = (f.width as usize, f.height as usize);
        let mut px = Vec::with_capacity(w * h * 3);
        for p in f.data.as_chunks::<4>().0.iter().take(w * h) {
            px.extend_from_slice(&[p[2], p[1], p[0]]);
        }
        Canvas { w, h, px }
    }

    fn blend(&mut self, x: i64, y: i64, c: [u8; 3], alpha: u16) {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return;
        }
        let i = (y as usize * self.w + x as usize) * 3;
        for (dst, &src) in self.px[i..i + 3].iter_mut().zip(&c) {
            *dst = ((u16::from(*dst) * (256 - alpha) + u16::from(src) * alpha) >> 8) as u8;
        }
    }

    fn fill(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, c: [u8; 3], alpha: u16) {
        for y in y0.max(0)..y1.min(self.h as i64) {
            for x in x0.max(0)..x1.min(self.w as i64) {
                self.blend(x, y, c, alpha);
            }
        }
    }

    fn outline(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, t: i64, c: [u8; 3]) {
        self.fill(x0, y0, x1, y0 + t, c, 256);
        self.fill(x0, y1 - t, x1, y1, c, 256);
        self.fill(x0, y0, x0 + t, y1, c, 256);
        self.fill(x1 - t, y0, x1, y1, c, 256);
    }

    /// A filled tag with white text, its top-left at (x, y).
    fn tag(&mut self, x: i64, y: i64, text: &str, s: i64, c: [u8; 3]) {
        let (tw, th) = (text.len() as i64 * 6 * s - s, 7 * s);
        let pad = 2 * s;
        let x = x.min(self.w as i64 - tw - 2 * pad).max(0);
        let y = y.min(self.h as i64 - th - 2 * pad).max(0);
        self.fill(x, y, x + tw + 2 * pad, y + th + 2 * pad, c, 256);
        for (i, ch) in text.chars().enumerate() {
            let rows = glyph(ch);
            for (r, bits) in rows.iter().enumerate() {
                for b in 0..5 {
                    if bits & (0x10 >> b) != 0 {
                        let gx = x + pad + i as i64 * 6 * s + b * s;
                        let gy = y + pad + r as i64 * s;
                        self.fill(gx, gy, gx + s, gy + s, [255, 255, 255], 256);
                    }
                }
            }
        }
    }

    fn png(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, self.w as u32, self.h as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().expect("png header");
        writer.write_image_data(&self.px).expect("png data");
        writer.finish().expect("png finish");
        out
    }
}

/// The frame with each element boxed and tagged with its ref; covered ones in grey.
pub fn elements(frame: &Frame, screen: (f64, f64), els: &[Element]) -> Vec<u8> {
    let mut c = Canvas::from_frame(frame);
    let k = frame.width as f64 / screen.0;
    let t = (k * 1.5).round().max(2.0) as i64;
    let s = (k * 1.4).round().max(2.0) as i64;
    for (i, e) in els.iter().enumerate() {
        let color = if e.visibility == Visibility::Covered { COVERED } else { PALETTE[i % PALETTE.len()] };
        let f = e.frame;
        let (x0, y0) = ((f[0] * k) as i64, (f[1] * k) as i64);
        let (x1, y1) = (((f[0] + f[2]) * k) as i64, ((f[1] + f[3]) * k) as i64);
        c.fill(x0, y0, x1, y1, color, 28);
        c.outline(x0, y0, x1, y1, t, color);
        c.tag(x0, y0, &e.reference, s, color);
    }
    c.png()
}

/// The frame with the grid drawn and every cell named.
pub fn grid(frame: &Frame, grid: &Grid) -> Vec<u8> {
    let mut c = Canvas::from_frame(frame);
    let (w, h) = (c.w as f64, c.h as f64);
    let t = ((w / 400.0).round() as i64).max(1);
    let s = ((w / 300.0).round() as i64).max(2);
    for col in 0..=grid.cols {
        let x = (f64::from(col) * w / f64::from(grid.cols)) as i64;
        c.fill(x - t / 2, 0, x - t / 2 + t, h as i64, GRID, 220);
    }
    for row in 0..=grid.rows {
        let y = (f64::from(row) * h / f64::from(grid.rows)) as i64;
        c.fill(0, y - t / 2, w as i64, y - t / 2 + t, GRID, 220);
    }
    for row in 0..grid.rows {
        for col in 0..grid.cols {
            let x = (f64::from(col) * w / f64::from(grid.cols)) as i64 + 2 * t;
            let y = (f64::from(row) * h / f64::from(grid.rows)) as i64 + 2 * t;
            c.tag(x, y, &grid.name(row, col), s, GRID);
        }
    }
    c.png()
}

/// Crop an element frame from a BGRA simulator frame and return a PNG.
pub fn crop(frame: &Frame, screen: (f64, f64), rect: [f64; 4]) -> Option<Vec<u8>> {
    let k = frame.width as f64 / screen.0;
    let x0 = (rect[0] * k).floor().max(0.0) as usize;
    let y0 = (rect[1] * k).floor().max(0.0) as usize;
    let x1 = ((rect[0] + rect[2]) * k).ceil().min(frame.width as f64) as usize;
    let y1 = ((rect[1] + rect[3]) * k).ceil().min(frame.height as f64) as usize;
    if x1 <= x0 || y1 <= y0 { return None; }
    let (w, h) = (x1 - x0, y1 - y0);
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in y0..y1 {
        let row = &frame.data[(y * frame.width as usize + x0) * 4..(y * frame.width as usize + x1) * 4];
        for p in row.as_chunks::<4>().0 {
            rgb.extend_from_slice(&[p[2], p[1], p[0]]);
        }
    }
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let mut writer = enc.write_header().ok()?;
    writer.write_image_data(&rgb).ok()?;
    writer.finish().ok()?;
    Some(out)
}

/// 5×7 glyphs (row bits, MSB left) for refs and cell names: 0-9, A-Z, e.
fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1C, 0x12, 0x11, 0x11, 0x11, 0x12, 0x1C],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x11, 0x0A, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        'e' => [0x00, 0x00, 0x0E, 0x11, 0x1F, 0x10, 0x0E],
        _ => [0x1F, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1F],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::marks::{elements as marks, tests::settings};

    fn frame(w: u32, h: u32) -> Frame {
        Frame { width: w, height: h, source_width: w, source_height: h, seq: 2, data: vec![0x20; (w * h * 4) as usize] }
    }

    fn decode(png_bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut r = png::Decoder::new(std::io::Cursor::new(png_bytes)).read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size().unwrap()];
        let info = r.next_frame(&mut buf).unwrap();
        (info.width, info.height, buf)
    }

    #[test]
    fn element_boxes_land_on_the_frames() {
        let (els, _) = marks(&settings(), (400.0, 800.0));
        let f = frame(800, 1600); // 2× the point size
        let (w, h, px) = decode(&elements(&f, (400.0, 800.0), &els));
        assert_eq!((w, h), (800, 1600));
        let at = |x: u32, y: u32| {
            let i = ((y * w + x) * 3) as usize;
            [px[i], px[i + 1], px[i + 2]]
        };
        // e1 "General" spans x 16..384, y 200..250 pt: its left edge (x 32 px, mid-height)
        // is drawn in e1's colour; the background far from any box is untouched.
        assert_eq!(at(33, 450), PALETTE[0]);
        assert_eq!(at(790, 20), [0x20, 0x20, 0x20]);
    }

    #[test]
    fn covered_elements_are_grey() {
        let (els, _) = marks(&settings(), (400.0, 800.0));
        let passcode = els.iter().find(|e| e.name() == "Passcode").unwrap();
        assert_eq!(passcode.visibility, Visibility::Covered);
        let (w, _, px) = decode(&elements(&frame(400, 800), (400.0, 800.0), std::slice::from_ref(passcode)));
        let i = ((775 * w + 17) * 3) as usize; // its left edge
        assert_eq!([px[i], px[i + 1], px[i + 2]], COVERED);
    }

    #[test]
    fn grid_overlay_names_every_cell() {
        let g = Grid::new((400.0, 800.0), 6);
        let (w, h, px) = decode(&grid(&frame(400, 800), &g));
        assert_eq!((w, h), (400, 800));
        // The tag of A1 sits at the top-left corner.
        let i = ((4 * w + 4) * 3) as usize;
        assert_eq!([px[i], px[i + 1], px[i + 2]], GRID);
    }

    #[test]
    fn every_ref_and_cell_character_has_a_glyph() {
        let unknown = glyph('?');
        for ch in ('0'..='9').chain('A'..='Z').chain(['e']) {
            assert_ne!(glyph(ch), unknown, "{ch}");
        }
    }
}
