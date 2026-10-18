//! Prove the firmware glyph path on its own, before any companion state is
//! blamed: rasterize `text` from the OS font, ship the GLFY blob as
//! `CMD_GLYPHS`, wait for the deck to ACK it, then push a deck view whose first
//! session row *is* `text`. If the deck renders that row, framing, blob
//! validation, font assembly and the label font swap all work, and the
//! companion is the only thing left to suspect.
//!
//! Quit the companion first. The deck has a single vendor HID queue, and two
//! readers on it eat each other's reports.
//!
//! ```text
//! cargo run --manifest-path src-tauri/Cargo.toml --example glyph_send -- \
//!   "审查并修复适配问题，「好」"
//! ```
//!
//! The deck view comes from the same encoder the deck worker uses, so this
//! cannot drift from what ships. The revision is wall-clock seconds, so a second
//! run is never mistaken for a stale view.

use std::sync::atomic::AtomicU16;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hidapi::HidApi;

use agent_deck_companion_lib::glyph_font;
use agent_deck_companion_lib::glyph_txn::{self, Ack, Event, Reject, Transaction};
use agent_deck_companion_lib::vendor_hid::{self, USAGE, USAGE_PAGE, VENDOR_ID};

fn main() {
    let Some(text) = std::env::args().nth(1) else {
        eprintln!("usage: glyph_send <title>");
        std::process::exit(2);
    };

    let (mut payload, labels) = vendor_hid::self_test_payload(&text);
    let codepoints = glyph_font::non_ascii_codepoints(labels.iter().map(String::as_str));
    println!(
        "first session row carries {:?}, needing {} non-ASCII codepoints",
        labels.first().map(String::as_str),
        codepoints.len()
    );
    if codepoints.len() > glyph_txn::MAX_GLYPHS_PER_TXN {
        println!(
            "note: the companion would skip glyphs and send ASCII here (> {} per transaction)",
            glyph_txn::MAX_GLYPHS_PER_TXN
        );
    }

    let mut api = HidApi::new().expect("hidapi init failed");
    #[cfg(target_os = "macos")]
    api.set_open_exclusive(false);
    let _ = api.refresh_devices();
    let device = Mutex::new(Some(open_deck(&api)));
    let seq = AtomicU16::new(0);

    let epoch = glyph_font::glyph_epoch(&codepoints);
    let records = glyph_font::rasterize_codepoints(&codepoints);
    if records.len() < codepoints.len() {
        println!(
            "note: {} of {} codepoints have no ink in the OS face and will draw as boxes",
            codepoints.len() - records.len(),
            codepoints.len()
        );
    }
    let blob = match glyph_font::encode_glyph_blob(&records, epoch) {
        Some(blob) => blob,
        None => {
            eprintln!("nothing to send: {text} rasterized no glyphs");
            std::process::exit(3);
        }
    };

    match send_glyphs(&device, &seq, &blob, epoch) {
        Ok((frames, elapsed)) => println!(
            "deck accepted the blob: {} B over {frames} frames in {elapsed:?}",
            blob.len()
        ),
        Err(reason) => {
            eprintln!("deck rejected the glyph blob: {reason}");
            eprintln!("it will draw {text:?} as placeholder boxes, or not at all");
            std::process::exit(4);
        }
    }

    payload[..4].copy_from_slice(&current_rev().to_le_bytes());
    // This example opens no reader thread, so it cannot wait for the apply ACK
    // the firmware sends after the last frame; it only checks the writes.
    if vendor_hid::write_deck_payload(&device, &seq, &payload).is_none() {
        eprintln!("deck view write failed");
        std::process::exit(5);
    }
    println!("pushed the deck view: the first session row should now read {text}");
}

fn open_deck(api: &HidApi) -> hidapi::HidDevice {
    let Some(path) = api
        .device_list()
        .find(|device| {
            device.vendor_id() == VENDOR_ID
                && device.usage_page() == USAGE_PAGE
                && device.usage() == USAGE
        })
        .map(|device| device.path())
    else {
        panic!("no deck found: is it plugged in, and is the companion stopped?");
    };
    api.open_path(path)
        .expect("open failed: another reader still owns the deck")
}

/// Drive a whole transaction, reading back the ACK this thread wrote for.
fn send_glyphs(
    device: &Mutex<Option<hidapi::HidDevice>>,
    seq: &AtomicU16,
    blob: &[u8],
    epoch: u32,
) -> Result<(usize, Duration), String> {
    let mut txn = match Transaction::new(blob, epoch, || {
        vendor_hid::command_frame(glyph_txn::CMD_GLYPHS, seq, &[])
    }) {
        Some(txn) => txn,
        None => return Err("blob is too large to frame".into()),
    };
    let frames = txn.frame_count();
    println!("{frames} frames at {:?} apart", glyph_txn::FRAME_GAP);
    let started = Instant::now();
    loop {
        match txn.next(Instant::now()) {
            Event::Frame(frame) => {
                if !vendor_hid::write_frame(device, frame) {
                    txn.abandon(Reject::Transport);
                }
            }
            Event::Wait(until) => drain_responses(device, &mut txn, until),
            Event::Done { .. } => return Ok((frames, started.elapsed())),
            Event::Abandon { reason } => return Err(format!("{reason:?}")),
        }
    }
}

/// Feed the transaction every glyph response arriving inside `until`, so a real
/// ACK wakes it before its pacing wait expires. Other reports — a status reply,
/// a selection event — are dropped, as the companion's reader thread drops them.
fn drain_responses(
    device: &Mutex<Option<hidapi::HidDevice>>,
    txn: &mut Transaction,
    until: Duration,
) {
    let deadline = Instant::now() + until;
    let mut report = [0_u8; 64];
    while Instant::now() < deadline {
        let Ok(guard) = device.lock() else { return };
        let Some(handle) = guard.as_ref() else { return };
        let left = deadline.duration_since(Instant::now());
        if !matches!(handle.read_timeout(&mut report, left.as_millis() as i32), Ok(length) if length > 0)
        {
            continue;
        }
        if report[1] != b'K'
            || report[2] != b'M'
            || report[4] != (glyph_txn::CMD_GLYPHS | glyph_txn::RESPONSE_FLAG)
        {
            continue;
        }
        let ack_seq = u16::from_le_bytes([report[5], report[6]]);
        match txn.on_ack(report[4], ack_seq, report[7], Instant::now()) {
            Ack::Retried => println!("device refused the blob; resending"),
            Ack::Rejected(reason) => println!("device rejected the blob: {reason:?}"),
            _ => {}
        }
    }
}

/// Wall-clock seconds, so every run outruns the revision the deck cached.
fn current_rev() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as u32
}
