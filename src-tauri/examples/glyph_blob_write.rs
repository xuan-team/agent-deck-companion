//! Dump the GLFY blob for a string to a file, so the firmware's own validator
//! can be run over it on the host.
//!
//! ```text
//! cargo run --manifest-path src-tauri/Cargo.toml --example glyph_blob_write -- \
//!   "审查并修复适配问题，「好」" /tmp/glyphs.bin
//! cc -Wall -Werror esp32-keyboard/main/glyph_blob.c \
//!    esp32-keyboard/main/tools/glyph_host_test.c -o /tmp/gbt
//! /tmp/gbt /tmp/glyphs.bin
//! ```
//!
//! This pairs the Rust encoder against the C parser that will run on the deck:
//! two independent implementations of the same contract must agree byte for
//! byte before either is trusted on hardware.

use agent_deck_companion_lib::glyph_font::{
    encode_glyph_blob, glyph_epoch, non_ascii_codepoints, rasterize_codepoints,
};
use std::path::Path;

fn main() {
    let mut argv = std::env::args().skip(1);
    let Some(text) = argv.next().filter(|value| !value.trim().is_empty()) else {
        eprintln!("usage: glyph_blob_write <text> <out.bin>");
        std::process::exit(2);
    };
    let out = argv.next().unwrap_or_else(|| "/tmp/glyphs.bin".to_string());

    let codepoints = non_ascii_codepoints([&text[..]]);
    let records = rasterize_codepoints(&codepoints);
    let epoch = glyph_epoch(&codepoints);
    let Some(blob) = encode_glyph_blob(&records, epoch) else {
        eprintln!("encoder rejected {} records", records.len());
        std::process::exit(1);
    };

    let path = Path::new(&out);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, &blob).unwrap_or_else(|error| {
        eprintln!("cannot write {out}: {error}");
        std::process::exit(1);
    });

    println!("text     : {text}");
    println!(
        "codepoints: {} non-ascii, {} rasterized",
        codepoints.len(),
        records.len()
    );
    println!("epoch    : {epoch:#010x}");
    println!("blob     : {} bytes -> {out}", blob.len());
}
