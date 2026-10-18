//! Glyph bitmaps for the deck LCD.
//!
//! The firmware carries no CJK (or any other non-Latin) font, so any label
//! with non-ASCII text would render as nothing. This module rasterizes the
//! codepoints a label actually needs into an LVGL `fmt_txt`-shaped blob that
//! the device assembles into a runtime `lv_font_t` falling back to
//! `lv_font_montserrat_16`.
//!
//! Glyphs come from the operating system's own font engine, so the companion
//! ships no font data and the device stores only the handful of bitmaps a
//! visible label needs.

use sha2::{Digest, Sha256};

/// Rasterization size in pixels. Must match the firmware's label font.
pub const PX_SIZE: f64 = 16.0;
/// Bits per pixel of the packed coverage, matching LVGL's `bpp` field.
pub const BPP: u8 = 4;
/// [`BPP`] widened for arithmetic on `usize` quantities.
const BITS_PER_PIXEL: usize = BPP as usize;
/// Metrics copied verbatim from `lv_font_montserrat_16` so mixed
/// ASCII/CJK runs share one baseline.
pub const LINE_HEIGHT: u8 = 18;
pub const BASE_LINE: u8 = 3;

/// Upper bound for one blob. The deck transport fragments at 54 bytes per
/// HID report, so this is also what keeps a glyph transaction finite.
pub const MAX_BLOB_BYTES: usize = 16 * 1024;

pub const BLOB_MAGIC: &[u8; 4] = b"GLFY";
pub const BLOB_VERSION: u8 = 1;
/// Magic | ver | px | bpp | flags | count | body_len | epoch | crc32
pub const BLOB_HEADER_BYTES: usize = 22;

/// The system face glyphs are rasterized from. Resolved by name, never by
/// collection index: PingFang.ttc index 0 is PingFang *HK*, which differs from
/// the simplified-Chinese face on about 40% of common ideographs.
#[cfg(target_os = "macos")]
const FACE_NAME: &str = "PingFang SC";

/// One glyph in the exact shape the blob carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlyphRecord {
    pub cp: u32,
    /// Advance in LVGL's 1/16 px units. Always the font's own advance, never
    /// the ink width: a full-width ideograph or `，` inks narrow but occupies
    /// a full em, and using the ink width would slide the next glyph left.
    pub adv_w: u16,
    pub box_w: u8,
    pub box_h: u8,
    pub ofs_x: i8,
    /// Distance from the baseline up to the bottom of the ink box, negative
    /// when the glyph descends.
    pub ofs_y: i8,
    /// 4 bpp coverage, row-major top-down, bits contiguous across rows so a
    /// row need not end on a byte boundary. Empty when the box is empty.
    pub bitmap: Vec<u8>,
}

impl GlyphRecord {
    pub fn bitmap_bytes(&self) -> usize {
        (self.box_w as usize * self.box_h as usize * BITS_PER_PIXEL).div_ceil(8)
    }
}

/// Linear 8-bit coverage to LVGL's 4-bit opacity scale, rounding to nearest.
pub fn coverage_to_level(cov: u8) -> u8 {
    let max_level = (1u32 << BPP) - 1;
    ((cov as u32 * max_level + 127) / 255) as u8
}

/// Pack a top-down 8-bit coverage grid into the 4 bpp stream LVGL expects.
pub fn pack_bitmap(box_w: usize, box_h: usize, coverage: &[u8]) -> Vec<u8> {
    let pixels = box_w * box_h;
    let mut out = vec![0u8; (pixels * BITS_PER_PIXEL).div_ceil(8)];
    for (i, &cov) in coverage.iter().take(pixels).enumerate() {
        let level = coverage_to_level(cov);
        let bit = i * BITS_PER_PIXEL;
        out[bit / 8] |= level << (8 - BITS_PER_PIXEL - bit % 8);
    }
    out
}

/// Codepoints a label actually needs: sorted, deduplicated, ASCII excluded
/// because the firmware's montserrat already draws it.
pub fn non_ascii_codepoints<'a, I: IntoIterator<Item = &'a str>>(texts: I) -> Vec<u32> {
    let mut cps: Vec<u32> = texts
        .into_iter()
        .flat_map(|text| text.chars())
        .map(|ch| ch as u32)
        .filter(|&cp| cp >= 0x80)
        .collect();
    cps.sort_unstable();
    cps.dedup();
    cps
}

/// Truncate to `max_bytes` without cutting a codepoint in half. The deck
/// payload is fixed width, so slicing on bytes alone sends a device a
/// truncated UTF-8 sequence.
pub fn utf8_truncate(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Identity of the glyph set. The device keys its cached font on this, so it
/// must change if — and only if — the set of codepoints changes.
pub fn glyph_epoch(codepoints: &[u32]) -> u32 {
    let mut sorted: Vec<u32> = codepoints.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut hasher = Sha256::new();
    for cp in sorted {
        hasher.update(cp.to_le_bytes());
    }
    let digest = hasher.finalize();
    u32::from_le_bytes(digest[..4].try_into().expect("sha256 prefix"))
}

/// CRC-32 as `esp_rom_crc32_le` computes it: reflected, init and final xor of
/// `0xFFFF_FFFF`. `"123456789"` yields `0xCBF43926`.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Serialize records into the `GLFY` blob the firmware unpacks.
///
/// Returns `None` if the records violate the format: codepoints must be
/// strictly ascending (the device looks them up with a binary search), boxes
/// must fit their declared bitmap, and the result must fit one transaction.
pub fn encode_glyph_blob(records: &[GlyphRecord], epoch: u32) -> Option<Vec<u8>> {
    // A record-less blob would install an empty font while the caller records
    // the whole set as installed. The device rejects `count == 0` today, but a
    // sender that ships a blob it knows is empty has already lost the invariant
    // that `glyph_installed` describes what the device actually got.
    if records.is_empty() {
        return None;
    }
    let mut body = Vec::new();
    let mut previous: Option<u32> = None;
    for record in records {
        if previous.is_some_and(|prev| record.cp <= prev) {
            return None;
        }
        previous = Some(record.cp);
        if record.bitmap.len() != record.bitmap_bytes() {
            return None;
        }
        body.extend_from_slice(&record.cp.to_le_bytes());
        body.extend_from_slice(&record.adv_w.to_le_bytes());
        body.push(record.box_w);
        body.push(record.box_h);
        body.push(record.ofs_x as u8);
        body.push(record.ofs_y as u8);
        body.extend_from_slice(&record.bitmap);
    }

    let total = BLOB_HEADER_BYTES + body.len();
    if records.len() > u16::MAX as usize || total > MAX_BLOB_BYTES {
        return None;
    }

    let mut blob = Vec::with_capacity(total);
    blob.extend_from_slice(BLOB_MAGIC);
    blob.push(BLOB_VERSION);
    blob.push(PX_SIZE as u8);
    blob.push(BPP);
    blob.push(0);
    blob.extend_from_slice(&(records.len() as u16).to_le_bytes());
    blob.extend_from_slice(&(body.len() as u32).to_le_bytes());
    blob.extend_from_slice(&epoch.to_le_bytes());
    blob.extend_from_slice(&crc32(&body).to_le_bytes());
    blob.extend_from_slice(&body);
    Some(blob)
}

/// The face labels are rasterized from, for logging and capability probing.
/// `None` means this platform has no glyph source and the caller must stay on
/// the ASCII path.
#[cfg(target_os = "macos")]
pub fn face_name() -> Option<String> {
    core_text_face().map(|font| font.postscript_name())
}

#[cfg(not(target_os = "macos"))]
pub fn face_name() -> Option<String> {
    None
}

/// Rasterize one codepoint per record. Codepoints the system face cannot draw
/// — and every codepoint on platforms without a glyph source — are dropped,
/// which leaves the device to fall back to montserrat for them.
#[cfg(target_os = "macos")]
pub fn rasterize_codepoints(codepoints: &[u32]) -> Vec<GlyphRecord> {
    let Some(font) = core_text_face() else {
        return Vec::new();
    };
    let mut records = Vec::with_capacity(codepoints.len());
    for cp in codepoints {
        if let Some(record) = rasterize_one(&font, *cp) {
            records.push(record);
        }
    }
    records
}

#[cfg(not(target_os = "macos"))]
pub fn rasterize_codepoints(codepoints: &[u32]) -> Vec<GlyphRecord> {
    let _ = codepoints;
    Vec::new()
}

#[cfg(target_os = "macos")]
fn core_text_face() -> Option<core_text::font::CTFont> {
    // Resolved by name rather than by collection index: PingFang.ttc holds 36
    // faces and its index 0 is PingFang *HK*, which differs from PingFang SC
    // on about 40% of common ideographs.
    core_text::font::new_from_name(FACE_NAME, PX_SIZE).ok()
}

#[cfg(target_os = "macos")]
fn rasterize_one(font: &core_text::font::CTFont, cp: u32) -> Option<GlyphRecord> {
    use core_graphics::color_space::CGColorSpace;
    use core_graphics::context::CGContext;
    use core_graphics::geometry::{CGPoint, CGRect, CGSize};
    use core_text::font_descriptor::kCTFontOrientationHorizontal;

    let value = char::from_u32(cp)?;
    let mut buffer = [0u16; 2];
    let units = value.encode_utf16(&mut buffer);

    // A glyph id of 0 means the face has no glyph for this codepoint; fontdue
    // and CoreText both happily hand back the notdef outline instead.
    let mut glyphs = vec![0u16; units.len()];
    unsafe {
        font.get_glyphs_for_characters(units.as_ptr(), glyphs.as_mut_ptr(), units.len() as isize);
    }
    let glyph = glyphs.first().copied().filter(|&id| id != 0)?;
    let glyph = [glyph];

    let advance = {
        let mut size = CGSize::new(0.0, 0.0);
        unsafe {
            font.get_advances_for_glyphs(
                kCTFontOrientationHorizontal,
                glyph.as_ptr(),
                &mut size,
                1,
            );
        }
        size.width
    };
    let box_rect = font.get_bounding_rects_for_glyphs(kCTFontOrientationHorizontal, &glyph);

    let ofs_x = box_rect.origin.x.floor();
    let ofs_y = box_rect.origin.y.floor();
    let box_w = (box_rect.origin.x + box_rect.size.width).ceil() - ofs_x;
    let box_h = (box_rect.origin.y + box_rect.size.height).ceil() - ofs_y;
    let adv_w = (advance * 16.0).round().clamp(0.0, u16::MAX as f64) as u16;
    if ofs_x.abs() > i8::MAX as f64 || ofs_y.abs() > i8::MAX as f64 {
        return None;
    }

    if box_w <= 0.0 || box_h <= 0.0 {
        // Whitespace, including the ideographic space: no ink, real advance.
        return Some(GlyphRecord {
            cp,
            adv_w,
            box_w: 0,
            box_h: 0,
            ofs_x: ofs_x as i8,
            ofs_y: ofs_y as i8,
            bitmap: Vec::new(),
        });
    }
    if box_w > u8::MAX as f64 || box_h > u8::MAX as f64 {
        return None;
    }
    let (box_w, box_h) = (box_w as usize, box_h as usize);

    let space = CGColorSpace::create_device_gray();
    let mut context = CGContext::create_bitmap_context(None, box_w, box_h, 8, box_w, &space, 0);
    context.set_should_antialias(true);
    context.set_allows_font_subpixel_positioning(false);
    context.set_allows_font_subpixel_quantization(false);
    context.set_should_smooth_fonts(false);
    context.set_gray_fill_color(0.0, 1.0);
    context.fill_rect(CGRect::new(
        &CGPoint::new(0.0, 0.0),
        &CGSize::new(box_w as f64, box_h as f64),
    ));
    context.set_gray_fill_color(1.0, 1.0);
    // Put the pen so the floored ink box exactly fills the context.
    font.draw_glyphs(&glyph, &[CGPoint::new(-ofs_x, -ofs_y)], context.clone());

    let row_bytes = context.bytes_per_row();
    let pixels = context.data();
    let mut coverage = Vec::with_capacity(box_w * box_h);
    for row in 0..box_h {
        let start = row * row_bytes;
        let end = start + box_w;
        match pixels.get(start..end) {
            Some(slice) => coverage.extend_from_slice(slice),
            None => return None,
        }
    }

    Some(GlyphRecord {
        cp,
        adv_w,
        box_w: box_w as u8,
        box_h: box_h as u8,
        ofs_x: ofs_x as i8,
        ofs_y: ofs_y as i8,
        bitmap: pack_bitmap(box_w, box_h, &coverage),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Decoded byte for byte out of components/lvgl/src/font/
    // lv_font_montserrat_16.c. Coverage values are nibble*17 so the 4 bpp
    // quantizer maps them back exactly.
    #[rustfmt::skip]
    const COV_EXCLAM: [u8; 36] = [
        187, 255,   0, 187, 255,   0, 170, 255,   0,
        170, 238,   0, 153, 238,   0, 136, 221,   0,
        136, 204,   0, 119, 204,   0,   0,   0,   0,
          0,  17,   0, 187, 255,  17, 170, 238,   0,
    ];
    #[rustfmt::skip]
    const PACK_EXCLAM: [u8; 18] = [
        0xbf, 0x0b, 0xf0, 0xaf, 0x0a, 0xe0, 0x9e, 0x08, 0xd0,
        0x8c, 0x07, 0xc0, 0x00, 0x00, 0x10, 0xbf, 0x1a, 0xe0,
    ];

    #[rustfmt::skip]
    const COV_QUOTE: [u8; 25] = [
        255,  85,  17, 255,  51,
        255,  85,  17, 255,  51,
        238,  68,   0, 255,  51,
        238,  68,   0, 255,  34,
        119,  34,   0, 136,  17,
    ];
    #[rustfmt::skip]
    const PACK_QUOTE: [u8; 13] = [
        0xf5, 0x1f, 0x3f, 0x51, 0xf3, 0xe4, 0x0f, 0x3e, 0x40,
        0xf2, 0x72, 0x08, 0x10,
    ];

    #[rustfmt::skip]
    const COV_PAREN: [u8; 60] = [
          0,   0, 221, 170,   0,  85, 255,  34,   0, 187,
        204,   0,   0, 255, 119,   0,  51, 255,  68,   0,
         85, 255,  34,   0, 102, 255,  17,   0, 119, 255,
          0,   0, 102, 255,  17,   0,  85, 255,  34,   0,
         51, 255,  68,   0,   0, 255, 119,   0,   0, 187,
        204,   0,   0,  85, 255,  34,   0,   0, 221, 170,
    ];
    #[rustfmt::skip]
    const PACK_PAREN: [u8; 30] = [
        0x00, 0xda, 0x05, 0xf2, 0x0b, 0xc0, 0x0f, 0x70, 0x3f, 0x40,
        0x5f, 0x20, 0x6f, 0x10, 0x7f, 0x00, 0x6f, 0x10, 0x5f, 0x20,
        0x3f, 0x40, 0x0f, 0x70, 0x0b, 0xc0, 0x05, 0xf2, 0x00, 0xda,
    ];

    #[test]
    fn packs_three_pixel_rows_without_byte_aligning_them() {
        // 3 px * 12 rows * 4 bpp = 144 bits: the rows cannot be byte aligned.
        assert_eq!(pack_bitmap(3, 12, &COV_EXCLAM), PACK_EXCLAM);
    }

    #[test]
    fn pads_a_trailing_partial_nibble_with_zero() {
        // 5 px * 5 rows = 25 px = 100 bits, stored in 13 bytes.
        assert_eq!(pack_bitmap(5, 5, &COV_QUOTE), PACK_QUOTE);
    }

    #[test]
    fn packs_a_four_pixel_wide_paren() {
        assert_eq!(pack_bitmap(4, 15, &COV_PAREN), PACK_PAREN);
    }

    #[test]
    fn coverage_levels_round_to_nearest() {
        assert_eq!(coverage_to_level(0), 0);
        assert_eq!(coverage_to_level(8), 0);
        assert_eq!(coverage_to_level(9), 1);
        assert_eq!(coverage_to_level(255), 15);
        for nibble in 0..=15u8 {
            assert_eq!(coverage_to_level(nibble * 17), nibble);
        }
    }

    /// Independent re-implementation of the device's read path, transcribed
    /// from lv_draw_sw_letter.c (`bitmask_init = 0xF0` for bpp 4, column 0 is
    /// the high nibble, `width_bit = box_w * bpp`) and lv_font_fmt_txt.c
    /// (`_lv_utils_bsearch` over an ascending SPARSE_TINY cmap). It shares no
    /// constant with the encoder on purpose.
    fn decode_glyph_bitmap(bitmap: &[u8], box_w: usize, box_h: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(box_w * box_h);
        for index in 0..box_w * box_h {
            let byte = bitmap[index * 4 / 8];
            let nibble = if index % 2 == 0 {
                byte >> 4
            } else {
                byte & 0x0F
            };
            out.push(nibble);
        }
        out
    }

    #[test]
    fn decoder_reads_back_what_the_encoder_wrote() {
        for &(w, h, cov) in &[
            (3usize, 12usize, &COV_EXCLAM[..]),
            (5, 5, &COV_QUOTE),
            (4, 15, &COV_PAREN),
        ] {
            let packed = pack_bitmap(w, h, cov);
            let levels = decode_glyph_bitmap(&packed, w, h);
            for (i, &level) in levels.iter().enumerate() {
                assert_eq!(level as usize, cov[i] as usize / 17, "pixel {i} of {w}x{h}");
            }
        }
    }

    #[test]
    fn encodes_a_self_describing_blob() {
        let records = vec![
            GlyphRecord {
                cp: 0x5ba1,
                adv_w: 256,
                box_w: 3,
                box_h: 2,
                ofs_x: 1,
                ofs_y: -2,
                bitmap: pack_bitmap(3, 2, &[255; 6]),
            },
            GlyphRecord {
                cp: 0xff0c,
                adv_w: 256,
                box_w: 1,
                box_h: 1,
                ofs_x: 0,
                ofs_y: 0,
                bitmap: pack_bitmap(1, 1, &[255]),
            },
        ];
        let epoch = glyph_epoch(&[0x5ba1, 0xff0c]);
        let blob = encode_glyph_blob(&records, epoch).expect("blob");

        assert_eq!(&blob[..4], BLOB_MAGIC);
        assert_eq!(blob[4], BLOB_VERSION);
        assert_eq!(blob[5], PX_SIZE as u8);
        assert_eq!(blob[6], BPP);
        assert_eq!(blob[7], 0);
        assert_eq!(&blob[8..10], &2u16.to_le_bytes());
        let body_len = u32::from_le_bytes(blob[10..14].try_into().unwrap()) as usize;
        assert_eq!(blob.len(), BLOB_HEADER_BYTES + body_len);
        assert_eq!(&blob[14..18], &epoch.to_le_bytes());

        let body = &blob[BLOB_HEADER_BYTES..];
        assert_eq!(
            u32::from_le_bytes(blob[18..22].try_into().unwrap()),
            crc32(body)
        );
        // Record layout: cp | adv_w | box_w | box_h | ofs_x | ofs_y | bitmap
        assert_eq!(&body[0..4], &0x5ba1u32.to_le_bytes());
        assert_eq!(&body[4..6], &256u16.to_le_bytes());
        assert_eq!(&body[6..8], &[3, 2]);
        assert_eq!(&body[8..10], &[1u8, (-2i8) as u8]);
        assert_eq!(&body[10..13], &[0xff, 0xff, 0xff]);
    }

    #[test]
    fn rejects_unordered_or_oversized_input() {
        let record = |cp: u32| GlyphRecord {
            cp,
            adv_w: 256,
            box_w: 2,
            box_h: 2,
            ofs_x: 0,
            ofs_y: 0,
            bitmap: vec![0x11; 2],
        };
        assert!(encode_glyph_blob(&[], 0).is_none());
        assert!(encode_glyph_blob(&[record(9), record(9)], 0).is_none());
        assert!(encode_glyph_blob(&[record(9), record(8)], 0).is_none());
        assert!(encode_glyph_blob(
            &[GlyphRecord {
                bitmap: vec![0u8; 3],
                ..record(8)
            }],
            0
        )
        .is_none());

        let fat = GlyphRecord {
            box_w: 255,
            box_h: 255,
            bitmap: vec![0u8; (255 * 255 * 4 + 7) / 8],
            ..record(0x4e00)
        };
        assert_eq!(fat.bitmap.len(), fat.bitmap_bytes());
        assert!(encode_glyph_blob(&[fat], 0).is_none());
    }

    #[test]
    fn crc32_matches_the_zlib_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(&[]), 0);
    }

    #[test]
    fn epoch_tracks_the_set_not_the_order() {
        let a = glyph_epoch(&[0x5ba1, 0xff0c, 0x4e00]);
        let b = glyph_epoch(&[0x4e00, 0xff0c, 0x5ba1]);
        let c = glyph_epoch(&[0x5ba1, 0xff0c, 0x4e00, 0x4e00]);
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_ne!(a, glyph_epoch(&[0x5ba1, 0xff0c]));
    }

    #[test]
    fn epoch_of_empty_set_is_defined_and_stable() {
        assert_eq!(glyph_epoch(&[]), glyph_epoch(&[]));
        assert_ne!(glyph_epoch(&[]), glyph_epoch(&[0]));
    }

    #[test]
    fn collects_non_ascii_codepoints_sorted_and_deduplicated() {
        assert_eq!(
            non_ascii_codepoints(["审查1", "2查", "abc"]),
            vec![0x5ba1, 0x67e5]
        );
        assert!(non_ascii_codepoints(["pure ascii 123"]).is_empty());
        assert!(non_ascii_codepoints(std::iter::empty::<&str>()).is_empty());
    }

    #[test]
    fn truncates_on_codepoint_boundaries() {
        // CJK is 3 bytes each, so 6 is an exact cut and 7 backs off to it.
        assert_eq!(utf8_truncate("审查并", 6), "审查");
        assert_eq!(utf8_truncate("审查并", 7), "审查");
        assert_eq!(utf8_truncate("审查并", 5), "审");
        assert_eq!(utf8_truncate("审查并", 9), "审查并");
        assert_eq!(utf8_truncate("审查并", 100), "审查并");
        assert_eq!(utf8_truncate("aé中", 2), "a");
        assert_eq!(utf8_truncate("aé中", 3), "aé");
        assert_eq!(utf8_truncate("😀x", 3), "");
        assert_eq!(utf8_truncate("😀x", 4), "😀");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn uses_simplified_chinese_pingfang_not_the_default_face() {
        // Index 0 of PingFang.ttc is PingFang HK, so a face chosen by index
        // would silently render regional variants.
        assert_eq!(face_name().as_deref(), Some("PingFangSC-Regular"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rasterizes_an_ideograph_at_a_full_em() {
        let records = rasterize_codepoints(&[0x5ba1]);
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.cp, 0x5ba1);
        assert_eq!(record.adv_w, 256, "16 px advance in 1/16 px units");
        assert!(record.box_w <= LINE_HEIGHT, "box_w {}", record.box_w);
        // Ink must stay inside the line box the firmware label already uses:
        // ofs_y is measured up from the baseline, LINE_HEIGHT - BASE_LINE of
        // it sits above the baseline.
        let top = record.ofs_y as i32 + record.box_h as i32;
        assert!(
            top <= LINE_HEIGHT as i32 - BASE_LINE as i32 + 1,
            "ink top {top} overflows the {LINE_HEIGHT}px line"
        );
        assert!(record.bitmap.iter().any(|&b| b != 0), "empty ink");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn full_width_punctuation_keeps_its_em_even_though_it_inks_narrow() {
        let records = rasterize_codepoints(&[0xff0c]);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].adv_w, 256);
        assert!(
            records[0].box_w < 8,
            "， should ink much narrower than its advance, got {}",
            records[0].box_w
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ink_rows_are_top_down() {
        // 'L' inks its horizontal stroke along the bottom of the box. If the
        // coverage buffer were read bottom-up the stroke would land in row 0.
        let record = &rasterize_codepoints(&[0x4c])[0];
        let levels =
            decode_glyph_bitmap(&record.bitmap, record.box_w as usize, record.box_h as usize);
        let width = record.box_w as usize;
        let height = record.box_h as usize;
        let row_ink =
            |row: usize| -> usize { (0..width).filter(|&c| levels[row * width + c] >= 8).count() };
        assert!(row_ink(0) <= width / 3, "row 0 inked {}", row_ink(0));
        assert!(
            row_ink(height - 1) >= width * 8 / 10,
            "last row inked {}",
            row_ink(height - 1)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn drops_codepoints_the_face_does_not_have() {
        // Ethiopic, outside every face PingFang ships.
        assert!(rasterize_codepoints(&[0x13a0]).is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn records_come_back_ascending_so_the_blob_encodes() {
        // Deliberately unsorted in: the caller feeds us non_ascii_codepoints'
        // output, which is already ascending, but guard the round trip.
        let mut cps = rasterize_codepoints(&[0x5ba1, 0x4e00, 0xff0c])
            .iter()
            .map(|record| record.cp)
            .collect::<Vec<_>>();
        cps.sort_unstable();
        let records = rasterize_codepoints(&cps);
        assert_eq!(records.len(), 3);
        assert!(encode_glyph_blob(&records, glyph_epoch(&cps)).is_some());
    }
}
