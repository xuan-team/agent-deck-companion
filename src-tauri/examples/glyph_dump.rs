//! Dev-only eyeball check for the glyph pipeline: rasterize a string, print
//! each glyph as ASCII art with the metrics the blob carries, and compose the
//! whole run into a PPM exactly the way LVGL will place it on screen.
//!
//! ```text
//! cargo run --example glyph_dump -- 审查并修复
//! ```
//!
//! The PPM goes to /tmp/glyph_dump.ppm in reading order; the metrics listing
//! below it is in codepoint order, because that is the order the blob carries.
//! ASCII never appears in the PPM — the device draws those from montserrat —
//! so judge the CJK against the line-box guide rather than the Latin run.

use agent_deck_companion_lib::glyph_font::{
    face_name, glyph_epoch, non_ascii_codepoints, rasterize_codepoints, GlyphRecord, BASE_LINE,
    BPP, LINE_HEIGHT, PX_SIZE,
};
use std::collections::HashMap;

const RAMP: &[u8] = b" .:-=+*#%@";
const GUIDE: u8 = 40;

fn main() {
    let text: String = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let text = if text.trim().is_empty() {
        String::from("审查并修复，「好」")
    } else {
        text
    };

    let codepoints = non_ascii_codepoints([&text[..]]);
    let records = rasterize_codepoints(&codepoints);
    println!("text        : {text}");
    println!("face        : {:?}", face_name());
    println!("px/bpp      : {PX_SIZE} / {BPP}");
    println!(
        "line/base   : {LINE_HEIGHT} / {BASE_LINE}, drawable rows 0..{}",
        LINE_HEIGHT - 1
    );
    println!(
        "glyphs      : {} of {} codepoints\n",
        records.len(),
        codepoints.len()
    );

    for record in &records {
        print_glyph(record);
    }

    // The run is laid out in reading order, the way the device will place it.
    let by_cp: HashMap<u32, &GlyphRecord> =
        records.iter().map(|record| (record.cp, record)).collect();
    let ordered: Vec<&GlyphRecord> = text
        .chars()
        .filter_map(|ch| by_cp.get(&(ch as u32)).copied())
        .collect();

    // lv_font_fmt_txt.c:177 turns 1/16 px units back into whole pixels here.
    let width = ordered
        .iter()
        .map(|record| (record.adv_w as i32 + 8) / 16)
        .sum::<i32>()
        + 4;
    let canvas = compose(&ordered, width.max(1) as usize);
    let path = "/tmp/glyph_dump.ppm";
    write_ppm(
        path,
        &canvas,
        width.max(1) as usize,
        LINE_HEIGHT as usize,
        8,
    );
    println!("rendered {width}x{LINE_HEIGHT} px scaled x8 -> {path}");
    println!("epoch = {:#010x}", glyph_epoch(&codepoints));
}

fn print_glyph(record: &GlyphRecord) {
    let w = record.box_w as usize;
    let h = record.box_h as usize;
    let ch = char::from_u32(record.cp).map_or_else(|| "?".to_string(), |c| c.to_string());
    println!(
        "U+{:04X} {ch}  adv={:>5} ({:.2}px)  box={w:>2}x{h:<2} ofs=({:>3},{:>3})  bitmap={:>3}B",
        record.cp,
        record.adv_w,
        record.adv_w as f64 / 16.0,
        record.ofs_x,
        record.ofs_y,
        record.bitmap.len(),
    );
    if w * h > 32 * 32 {
        println!("  (box too large to print)");
        return;
    }
    let levels = unpack(&record.bitmap, w, h);
    // lv_draw_sw_letter.c:132-133 — the canvas row the ink lands on.
    let top = LINE_HEIGHT as i32 - BASE_LINE as i32 - h as i32 - record.ofs_y as i32;
    println!("  rows {top}..{}", top + h as i32 - 1);
    for (row, level) in levels.chunks(w).enumerate() {
        let glyphs: String = level
            .iter()
            .map(|&l| RAMP[l as usize * (RAMP.len() - 1) / 15] as char)
            .collect();
        println!("  {:>2} |{glyphs}|", top + row as i32);
    }
    println!();
}

/// The device's read path: high nibble is the leftmost pixel, rows are bit
/// contiguous, so a row need not end on a byte boundary.
fn unpack(bitmap: &[u8], w: usize, h: usize) -> Vec<u8> {
    (0..w * h)
        .map(|i| {
            let byte = bitmap.get(i / 2).copied().unwrap_or(0);
            if i % 2 == 0 {
                byte >> 4
            } else {
                byte & 0x0F
            }
        })
        .collect()
}

fn compose(records: &[&GlyphRecord], width: usize) -> Vec<u8> {
    let height = LINE_HEIGHT as usize;
    let mut canvas = vec![0u8; width * height];
    let mut pen_x = 2i32;
    for record in records {
        let w = record.box_w as usize;
        let h = record.box_h as usize;
        let gx = pen_x + record.ofs_x as i32;
        let gy = LINE_HEIGHT as i32 - BASE_LINE as i32 - h as i32 - record.ofs_y as i32;
        for (row, level) in unpack(&record.bitmap, w, h).chunks(w).enumerate() {
            let y = gy + row as i32;
            if y < 0 || y >= height as i32 {
                println!("  !! ink escaped the line box at row {y}");
                continue;
            }
            for (col, &l) in level.iter().enumerate() {
                let x = gx + col as i32;
                if x < 0 || x >= width as i32 {
                    continue;
                }
                let pixel = &mut canvas[y as usize * width + x as usize];
                *pixel = (*pixel).max(l * 17);
            }
        }
        pen_x += (record.adv_w as i32 + 8) / 16;
    }
    // The last row of the line box, drawn where no ink landed.
    for x in 0..width {
        let pixel = &mut canvas[(height - 1) * width + x];
        if *pixel == 0 {
            *pixel = GUIDE;
        }
    }
    canvas
}

fn write_ppm(path: &str, canvas: &[u8], width: usize, height: usize, scale: usize) {
    let header = format!("P6\n{} {}\n255\n", width * scale, height * scale);
    let mut out = Vec::with_capacity(header.len() + width * height * scale * scale * 3);
    out.extend_from_slice(header.as_bytes());
    for row in canvas.chunks(width) {
        for _ in 0..scale {
            for &v in row {
                for _ in 0..scale {
                    // Invert: the deck draws light glyphs on a dark panel.
                    let shade = 255 - v;
                    out.extend_from_slice(&[shade, shade, shade]);
                }
            }
        }
    }
    if let Err(error) = std::fs::write(path, out) {
        println!("could not write {path}: {error}");
    }
}
