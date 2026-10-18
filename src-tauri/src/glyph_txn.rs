//! Pure logic for a `CMD_GLYPHS` blob transfer: frame splitting, ACK matching,
//! retry and timeout accounting. No `hidapi`, no threads, no sleeping — the
//! caller drives the machine with an `Instant` and acts on each [`Event`], so
//! every branch (including the pacing gap) is unit-testable off-device.
//!
//! The wire contract lives in `esp32-keyboard/ai_work_doc/glyph_font_protocol.md`.
//! A frame is a fixed 64-byte HID report:
//!
//! ```text
//! [0]     report id        (owned by the caller's base-frame builder)
//! [1..3]  magic 'K','M'    (caller)
//! [3]     version 1        (caller)
//! [4]     command = 21     (caller)
//! [5..7]  sequence u16 LE  (caller, one per frame)
//! [7..9]  index u16 LE     (this module)
//! [9..11] total u16 LE     (this module)
//! [11..64] 53 data bytes   (this module)
//! ```

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use crate::glyph_font::{self, BLOB_HEADER_BYTES, BLOB_MAGIC, MAX_BLOB_BYTES};

pub const CMD_GLYPHS: u8 = 21;
/// Response flag OR'd into the command byte by the device, matching the
/// `kResponseFlag` in `HidMockProtocol.cpp`.
pub const RESPONSE_FLAG: u8 = 0x80;
/// Data bytes per frame: `frame[11..64]`.
pub const FRAME_DATA: usize = 53;
/// A single transaction may carry at most this many glyphs; more than this
/// degrades to the ASCII path rather than spilling into a second transaction.
pub const MAX_GLYPHS_PER_TXN: usize = 96;
/// Minimum spacing between consecutive frames. The device drains its 256-byte
/// USB RX queue once per ~10 ms MainTask tick (`USBHIDVendor.cpp:88`,
/// `MainTask.cpp:1162`); at 53 payload bytes per frame the safe sustained rate
/// is ~4 frames / 10 ms, so 4 ms leaves a 1.6× margin.
pub const FRAME_GAP: Duration = Duration::from_millis(4);
/// How long to wait for the final frame's ACK before resending.
pub const ACK_TIMEOUT: Duration = Duration::from_secs(4);
/// Total attempts (initial send plus resends) before giving up.
pub const MAX_ATTEMPTS: u32 = 3;
/// Wall-clock ceiling for the whole transaction regardless of attempts.
pub const TXN_DEADLINE: Duration = Duration::from_secs(15);
/// Hard frame ceiling implied by the largest legal blob.
pub const MAX_FRAMES: usize = MAX_BLOB_BYTES.div_ceil(FRAME_DATA);

// Device status codes from `HidMockProtocol.cpp` (`kOk`/`kBadPacket`/`kBadCommand`).
const STATUS_OK: u8 = 0;
const STATUS_BAD_COMMAND: u8 = 2;

/// Why a transaction stopped being useful. Every variant means "fall back to
/// the ASCII path for this push", never "blank the label".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// Device NAK'd the glyph command as unknown — an old firmware. The caller
    /// should latch this for the whole session and stop probing.
    DeviceUnsupported,
    /// Device kept refusing the blob (validation failed) past the retry budget.
    Refused,
    /// No matching ACK within [`ACK_TIMEOUT`] on every attempt.
    Timeout,
    /// The transaction outlived [`TXN_DEADLINE`].
    Deadline,
    /// Too many unique codepoints for one transaction.
    TooManyGlyphs,
    /// The link died or a write failed mid-transfer; nothing about the device
    /// protocol is implicated, so the caller must not latch this as
    /// [`Reject::DeviceUnsupported`].
    Transport,
}

/// Outcome of feeding an ACK into [`Transaction::on_ack`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ack {
    /// Glyph set is now on the device; safe to bump the DeckView revision.
    Accepted,
    /// Refused this attempt; the machine reset itself for a resend.
    Retried,
    /// Not for this transaction (wrong command, stale sequence, or already over).
    Ignored,
    /// Terminal: stop and fall back, with this reason.
    Rejected(Reject),
}

/// An action the caller must perform, produced by [`Transaction::next`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Write this report to the device (outside the held device lock if the
    /// caller can, so a concurrent status read is not starved).
    Frame([u8; 64]),
    /// Do nothing for this long, then call `next` again. The caller sleeps on
    /// the ACK channel for at most this duration so a real ACK wakes it early.
    Wait(Duration),
    /// Succeeded; carries the epoch now installed on the device.
    Done { epoch: u32 },
    /// Failed; carries why. Fall back to ASCII for this push.
    Abandon { reason: Reject },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Active,
    Done,
    Abandoned(Reject),
}

/// One in-flight `CMD_GLYPHS` transaction.
#[derive(Debug, Clone)]
pub struct Transaction {
    frames: Vec<[u8; 64]>,
    epoch: u32,
    expected_seq: u16,
    attempt: u32,
    next_frame: usize,
    last_emit: Option<Instant>,
    sent_all: bool,
    awaiting_since: Option<Instant>,
    started: Instant,
    status: Status,
}

impl Transaction {
    /// Split `blob` into frames. `base` returns a frame already carrying the
    /// report id, magic, version, command byte and a fresh sequence number
    /// (`vendor_hid::command_frame(CMD_GLYPHS, seq, &[])`); this module stamps
    /// `index`, `total` and the 53-byte data slice over `[7..64]`.
    ///
    /// Returns `None` for an impossibly short or oversized blob, a wrong magic,
    /// or a frame count that overflows `u16` — the caller treats `None` as
    /// "do not transfer, fall back to ASCII".
    pub fn new<F: FnMut() -> [u8; 64]>(blob: &[u8], epoch: u32, mut base: F) -> Option<Self> {
        if blob.len() < BLOB_HEADER_BYTES || &blob[..4] != BLOB_MAGIC {
            return None;
        }
        if blob.len() > MAX_BLOB_BYTES {
            return None;
        }
        let total = blob.len().div_ceil(FRAME_DATA);
        if total == 0 || total > MAX_FRAMES {
            return None;
        }
        let total_u16 = u16::try_from(total).ok()?;

        let mut frames = Vec::with_capacity(total);
        let mut expected_seq = 0_u16;
        for index in 0..total {
            let mut frame = base();
            let start = index * FRAME_DATA;
            let end = (start + FRAME_DATA).min(blob.len());
            frame[7..9].copy_from_slice(&(index as u16).to_le_bytes());
            frame[9..11].copy_from_slice(&total_u16.to_le_bytes());
            frame[11..11 + (end - start)].copy_from_slice(&blob[start..end]);
            expected_seq = u16::from_le_bytes([frame[5], frame[6]]);
            frames.push(frame);
        }

        let started = Instant::now();
        Some(Self {
            frames,
            epoch,
            expected_seq,
            attempt: 1,
            next_frame: 0,
            last_emit: None,
            sent_all: false,
            awaiting_since: None,
            started,
            status: Status::Active,
        })
    }

    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    pub fn expected_seq(&self) -> u16 {
        self.expected_seq
    }

    /// True once every frame of the current attempt has been emitted and the
    /// machine is waiting for the final ACK.
    pub fn awaiting_ack(&self) -> bool {
        self.sent_all && matches!(self.status, Status::Active)
    }

    /// Advance the machine. The caller invokes this repeatedly, performing the
    /// returned action; `now` is monotonic wall-clock the caller controls.
    pub fn next(&mut self, now: Instant) -> Event {
        if let Some(terminal) = self.terminal_event() {
            return terminal;
        }
        if now.duration_since(self.started) >= TXN_DEADLINE {
            return self.abandon(Reject::Deadline);
        }

        if self.sent_all {
            let since = now
                .checked_duration_since(self.awaiting_since.unwrap_or(now))
                .unwrap_or(Duration::ZERO);
            if since >= ACK_TIMEOUT {
                return self.resend_or_give_up(Reject::Timeout, now);
            }
            // Caller parks on the ACK channel for at most the remaining window;
            // a real ACK wakes it early via `on_ack`.
            return Event::Wait(ACK_TIMEOUT - since);
        }

        if self.next_frame < self.frames.len() {
            if let Some(last_emit) = self.last_emit {
                let gap = FRAME_GAP.saturating_sub(now.duration_since(last_emit));
                if !gap.is_zero() {
                    return Event::Wait(gap);
                }
            }
            let frame = self.frames[self.next_frame];
            self.next_frame += 1;
            self.last_emit = Some(now);
            if self.next_frame == self.frames.len() {
                self.sent_all = true;
                self.awaiting_since = Some(now);
            }
            return Event::Frame(frame);
        }

        // Defensive: nothing left to send but not marked awaiting.
        self.sent_all = true;
        self.awaiting_since = Some(now);
        Event::Wait(ACK_TIMEOUT)
    }

    /// Feed a device response. `cmd` is the raw response command byte (already
    /// carrying [`RESPONSE_FLAG`]), `seq` its sequence, `status` the byte at
    /// `report[7]`. Non-matching responses are [`Ack::Ignored`] so a
    /// concurrent `CMD_STATUS` reply never disturbs the machine.
    pub fn on_ack(&mut self, cmd: u8, seq: u16, status: u8, now: Instant) -> Ack {
        if cmd != CMD_GLYPHS | RESPONSE_FLAG || !matches!(self.status, Status::Active) {
            return Ack::Ignored;
        }
        if now.duration_since(self.started) >= TXN_DEADLINE {
            self.abandon(Reject::Deadline);
            return Ack::Ignored;
        }
        // An unknown command NAK arrives even on the very first frame (old
        // firmware replies before we finish sending), so check it unconditionally.
        if status == STATUS_BAD_COMMAND {
            self.abandon(Reject::DeviceUnsupported);
            return Ack::Rejected(Reject::DeviceUnsupported);
        }
        if !self.sent_all || seq != self.expected_seq {
            return Ack::Ignored;
        }
        if status == STATUS_OK {
            self.status = Status::Done;
            return Ack::Accepted;
        }
        match self.resend_or_give_up(Reject::Refused, now) {
            Event::Wait(_) => Ack::Retried,
            Event::Abandon { reason } => Ack::Rejected(reason),
            _ => Ack::Retried,
        }
    }

    /// Force the transaction over, e.g. the write call failed mid-send.
    pub fn abandon(&mut self, reason: Reject) -> Event {
        self.status = Status::Abandoned(reason);
        Event::Abandon { reason }
    }

    fn resend_or_give_up(&mut self, reason: Reject, _now: Instant) -> Event {
        if self.attempt >= MAX_ATTEMPTS {
            return self.abandon(reason);
        }
        self.attempt += 1;
        self.next_frame = 0;
        self.sent_all = false;
        self.awaiting_since = None;
        self.last_emit = None;
        Event::Wait(Duration::ZERO)
    }

    fn terminal_event(&self) -> Option<Event> {
        match self.status {
            Status::Active => None,
            Status::Done => Some(Event::Done { epoch: self.epoch }),
            Status::Abandoned(reason) => Some(Event::Abandon { reason }),
        }
    }
}

/// What the deck push should do about non-ASCII labels, decided purely from
/// the visible codepoint set and the set believed installed on the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Transfer these codepoints, then bump the revision once the ACK lands.
    /// Carries the whole set the blob installs, which is a superset of what the
    /// current labels need.
    Transfer { epoch: u32, codepoints: Vec<u32> },
    /// The device already renders every codepoint on screen. Send no glyph
    /// frames; the Unicode payload stands.
    Keep,
    /// Do not attempt glyphs this push; emit the ASCII-filtered payload.
    Degrade(Reject),
}

/// Decide whether a glyph transfer is needed. `installed` is the codepoint set
/// the companion believes is on the device right now — `None` means "unknown",
/// which is what a reboot or a first link leaves it at.
///
/// The device font is *replaced* whole by every blob, so a transfer carries the
/// union of what is installed and what the new labels need. That is what makes
/// A→B→A cost no frames the second time around: the set only ever grows, until
/// it no longer fits one transaction.
pub fn plan(needed: &[u32], installed: Option<&BTreeSet<u32>>) -> Plan {
    // An empty need means the Unicode and ASCII payloads are byte-identical, so
    // there is nothing for a font to draw.
    if needed.is_empty() {
        return Plan::Keep;
    }
    let Some(installed) = installed else {
        return transfer(needed);
    };
    if needed.iter().all(|cp| installed.contains(cp)) {
        return Plan::Keep;
    }
    let mut union = installed.clone();
    union.extend(needed.iter().copied());
    // A union that outgrew the transaction budget throws the cache away rather
    // than dropping a label: the blob then carries just what is on screen now.
    if union.len() <= MAX_GLYPHS_PER_TXN {
        return transfer(&union.into_iter().collect::<Vec<_>>());
    }
    transfer(needed)
}

fn transfer(codepoints: &[u32]) -> Plan {
    // The blob requires strictly ascending codepoints and the epoch must not
    // depend on the order labels happened to be collected in.
    let mut codepoints: Vec<u32> = codepoints.iter().copied().collect();
    codepoints.sort_unstable();
    codepoints.dedup();
    if codepoints.len() > MAX_GLYPHS_PER_TXN {
        return Plan::Degrade(Reject::TooManyGlyphs);
    }
    Plan::Transfer {
        epoch: glyph_font::glyph_epoch(&codepoints),
        codepoints,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A base-frame builder that mimics `command_frame`: fixed header, a
    /// per-call-incremented sequence at `[5..7]`.
    fn base_builder(seq: Cell<u16>) -> impl Fn() -> [u8; 64] {
        move || {
            let mut frame = [0_u8; 64];
            frame[0] = 6;
            frame[1] = b'K';
            frame[2] = b'M';
            frame[3] = 1;
            frame[4] = CMD_GLYPHS;
            let value = seq.get() + 1;
            seq.set(value);
            frame[5] = value as u8;
            frame[6] = (value >> 8) as u8;
            frame
        }
    }

    fn blob_of_len(len: usize) -> Vec<u8> {
        let mut blob = Vec::with_capacity(len);
        blob.extend_from_slice(BLOB_MAGIC);
        for i in 4..len {
            blob.push((i % 251) as u8);
        }
        blob
    }

    struct Clock(Instant);
    impl Clock {
        fn new() -> Self {
            Clock(Instant::now())
        }
        fn at(&self, ms: u64) -> Instant {
            self.0 + Duration::from_millis(ms)
        }
    }

    /// Emit frames until the machine is waiting for the ACK, advancing the
    /// clock over any pacing wait.
    fn send_all(txn: &mut Transaction, now: &mut Instant) -> Vec<[u8; 64]> {
        let mut out = Vec::new();
        loop {
            match txn.next(*now) {
                Event::Frame(frame) => {
                    out.push(frame);
                    if txn.awaiting_ack() {
                        break;
                    }
                }
                Event::Wait(gap) => *now += gap,
                other => panic!("unexpected terminal during send: {other:?}"),
            }
        }
        out
    }

    #[test]
    fn splits_blob_into_expected_frame_count() {
        for (len, expected) in [(BLOB_HEADER_BYTES, 1), (53, 1), (54, 2), (106, 2), (107, 3)] {
            let seq = Cell::new(0);
            let txn = Transaction::new(&blob_of_len(len), 7, base_builder(seq))
                .unwrap_or_else(|| panic!("frame count for len {len}"));
            assert_eq!(txn.frame_count(), expected, "len {len}");
        }
    }

    #[test]
    fn stamps_index_total_and_data_at_exact_offsets() {
        let blob = blob_of_len(120); // 3 frames: 53 + 53 + 14
        let seq = Cell::new(0);
        let txn = Transaction::new(&blob, 42, base_builder(seq)).unwrap();
        let clock = Clock::new();
        let mut now = clock.at(0);
        let frames = send_all(&mut txn.clone(), &mut now);
        assert_eq!(frames.len(), 3);

        for (index, frame) in frames.iter().enumerate() {
            assert_eq!(
                u16::from_le_bytes([frame[7], frame[8]]),
                index as u16,
                "index at frame {index}"
            );
            assert_eq!(u16::from_le_bytes([frame[9], frame[10]]), 3, "total");
            let start = index * FRAME_DATA;
            let end = (start + FRAME_DATA).min(blob.len());
            assert_eq!(&frame[11..11 + (end - start)], &blob[start..end]);
        }
        // The final frame's tail beyond its data stays zero.
        assert!(frames[2][11 + 14..64].iter().all(|&b| b == 0));
    }

    #[test]
    fn sequences_frames_and_tracks_expected_ack() {
        let seq = Cell::new(10);
        let txn = Transaction::new(&blob_of_len(120), 1, base_builder(seq)).unwrap();
        // command_frame assigns seq+1 per call: 11, 12, 13.
        assert_eq!(txn.expected_seq(), 13);
        assert_eq!(txn.frame_count(), 3);
    }

    #[test]
    fn accepts_matching_ok_after_last_frame() {
        let seq = Cell::new(0);
        let mut txn = Transaction::new(&blob_of_len(120), 99, base_builder(seq)).unwrap();
        let clock = Clock::new();
        let mut now = clock.at(0);
        send_all(&mut txn, &mut now);
        assert!(txn.awaiting_ack());
        let ack = txn.on_ack(
            CMD_GLYPHS | RESPONSE_FLAG,
            txn.expected_seq(),
            STATUS_OK,
            now,
        );
        assert_eq!(ack, Ack::Accepted);
        assert_eq!(txn.next(now), Event::Done { epoch: 99 });
    }

    #[test]
    fn ignores_wrong_command_and_stale_sequence() {
        let seq = Cell::new(0);
        let mut txn = Transaction::new(&blob_of_len(120), 1, base_builder(seq)).unwrap();
        let clock = Clock::new();
        let mut now = clock.at(0);
        send_all(&mut txn, &mut now);

        // A CMD_STATUS response (5|0x80) must never disturb the machine.
        assert_eq!(
            txn.on_ack(5 | RESPONSE_FLAG, txn.expected_seq(), STATUS_OK, now),
            Ack::Ignored
        );
        // Correct command, stale sequence (a previous attempt's last frame).
        let stale = txn.expected_seq().wrapping_sub(1);
        assert_eq!(
            txn.on_ack(CMD_GLYPHS | RESPONSE_FLAG, stale, STATUS_OK, now),
            Ack::Ignored
        );
        // Still waiting, not Done.
        assert!(txn.awaiting_ack());
    }

    #[test]
    fn bad_command_nak_means_unsupported_even_mid_send() {
        let seq = Cell::new(0);
        let mut txn = Transaction::new(&blob_of_len(120), 1, base_builder(seq)).unwrap();
        let clock = Clock::new();
        let now = clock.at(0);
        // Only the first frame sent — old firmware already NAK'd it.
        assert!(matches!(txn.next(now), Event::Frame(_)));
        assert!(!txn.awaiting_ack());
        let ack = txn.on_ack(
            CMD_GLYPHS | RESPONSE_FLAG,
            txn.expected_seq(), // won't match but BAD_COMMAND is checked first
            STATUS_BAD_COMMAND,
            now,
        );
        assert_eq!(ack, Ack::Rejected(Reject::DeviceUnsupported));
        assert_eq!(
            txn.next(now),
            Event::Abandon {
                reason: Reject::DeviceUnsupported
            }
        );
    }

    #[test]
    fn resends_up_to_max_attempts_then_times_out() {
        let seq = Cell::new(0);
        let mut txn = Transaction::new(&blob_of_len(60), 1, base_builder(seq)).unwrap();
        let clock = Clock::new();
        let mut now = clock.at(0);

        for attempt in 1..=MAX_ATTEMPTS {
            send_all(&mut txn, &mut now);
            assert!(txn.awaiting_ack(), "attempt {attempt} should be awaiting");
            // Let the ACK window elapse with no reply.
            now += ACK_TIMEOUT + Duration::from_millis(1);
            match txn.next(now) {
                Event::Wait(_) => {
                    // A resend was scheduled; still active until the last attempt.
                    assert_eq!(txn.attempt(), attempt + 1, "attempt counter");
                    assert!(!txn.awaiting_ack(), "reset for resend");
                }
                Event::Abandon { reason } => {
                    assert_eq!(attempt, MAX_ATTEMPTS, "abandoned too early");
                    assert_eq!(reason, Reject::Timeout);
                    return;
                }
                other => panic!("expected resend/abandon, got {other:?}"),
            }
        }
        // After the final resend runs out:
        send_all(&mut txn, &mut now);
        now += ACK_TIMEOUT + Duration::from_millis(1);
        assert_eq!(
            txn.next(now),
            Event::Abandon {
                reason: Reject::Timeout
            }
        );
    }

    #[test]
    fn refusal_retries_then_degrades() {
        let seq = Cell::new(0);
        let mut txn = Transaction::new(&blob_of_len(60), 1, base_builder(seq)).unwrap();
        let clock = Clock::new();
        let mut now = clock.at(0);

        for _attempt in 1..MAX_ATTEMPTS {
            send_all(&mut txn, &mut now);
            let ack = txn.on_ack(
                CMD_GLYPHS | RESPONSE_FLAG,
                txn.expected_seq(),
                1, /* bad packet */
                now,
            );
            assert_eq!(ack, Ack::Retried);
        }
        send_all(&mut txn, &mut now);
        let ack = txn.on_ack(CMD_GLYPHS | RESPONSE_FLAG, txn.expected_seq(), 1, now);
        assert_eq!(ack, Ack::Rejected(Reject::Refused));
    }

    #[test]
    fn paces_consecutive_frames_by_the_gap() {
        let seq = Cell::new(0);
        let mut txn = Transaction::new(&blob_of_len(120), 1, base_builder(seq)).unwrap();
        let clock = Clock::new();
        let mut now = clock.at(0);

        assert!(
            matches!(txn.next(now), Event::Frame(_)),
            "first frame immediate"
        );
        // Immediately after, the next frame must wait the full gap.
        match txn.next(now) {
            Event::Wait(gap) => assert_eq!(gap, FRAME_GAP),
            other => panic!("expected pacing wait, got {other:?}"),
        }
        now += FRAME_GAP;
        assert!(
            matches!(txn.next(now), Event::Frame(_)),
            "second frame after gap"
        );
    }

    #[test]
    fn deadline_abandons_regardless_of_attempts() {
        let seq = Cell::new(0);
        let mut txn = Transaction::new(&blob_of_len(120), 1, base_builder(seq)).unwrap();
        let clock = Clock::new();
        let late = clock.at(0) + TXN_DEADLINE;
        assert_eq!(
            txn.next(late),
            Event::Abandon {
                reason: Reject::Deadline
            }
        );
    }

    #[test]
    fn explicit_abandon_is_terminal() {
        let seq = Cell::new(0);
        let mut txn = Transaction::new(&blob_of_len(120), 1, base_builder(seq)).unwrap();
        let clock = Clock::new();
        let now = clock.at(0);
        txn.abandon(Reject::Refused);
        assert_eq!(
            txn.next(now),
            Event::Abandon {
                reason: Reject::Refused
            }
        );
        assert_eq!(
            txn.on_ack(
                CMD_GLYPHS | RESPONSE_FLAG,
                txn.expected_seq(),
                STATUS_OK,
                now
            ),
            Ack::Ignored
        );
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Transaction::new(&[], 1, base_builder(Cell::new(0))).is_none());
        assert!(Transaction::new(b"nope", 1, base_builder(Cell::new(0))).is_none());
        let mut oversized = blob_of_len(BLOB_HEADER_BYTES);
        oversized.resize(MAX_BLOB_BYTES + 1, 0);
        assert!(Transaction::new(&oversized, 1, base_builder(Cell::new(0))).is_none());
    }

    fn set(codepoints: &[u32]) -> BTreeSet<u32> {
        codepoints.iter().copied().collect()
    }

    #[test]
    fn plan_degrades_when_too_many_glyphs() {
        let codepoints: Vec<u32> = (0x4e00..0x4e00 + (MAX_GLYPHS_PER_TXN as u32) + 1).collect();
        assert_eq!(
            plan(&codepoints, None),
            Plan::Degrade(Reject::TooManyGlyphs)
        );
    }

    #[test]
    fn plan_keeps_when_nothing_new_is_needed() {
        assert!(matches!(plan(&[], None), Plan::Keep));
        let codepoints = vec![0x4e00_u32, 0x4e01];
        // Nothing known on the device, and nothing the font must draw: still Keep.
        assert!(matches!(plan(&[], Some(&set(&[]))), Plan::Keep));
        // The device carries this exact set.
        assert!(matches!(
            plan(&codepoints, Some(&set(&codepoints))),
            Plan::Keep
        ));
    }

    #[test]
    fn plan_keeps_when_the_device_carries_a_superset() {
        let installed = vec![0x4e00_u32, 0x4e01, 0x4e02];
        let needed = vec![0x4e01, 0x4e02];
        assert!(matches!(plan(&needed, Some(&set(&installed))), Plan::Keep));
    }

    #[test]
    fn plan_resends_only_what_is_needed_when_nothing_is_installed() {
        let codepoints = vec![0x4e00_u32, 0x4e01];
        match plan(&codepoints, None) {
            Plan::Transfer {
                epoch: seen,
                codepoints: cps,
            } => {
                assert_eq!(seen, glyph_font::glyph_epoch(&codepoints));
                assert_eq!(cps, codepoints);
            }
            other => panic!("expected Transfer, got {other:?}"),
        }
    }

    #[test]
    fn plan_transfer_ships_the_union_so_going_back_is_free() {
        let agent_a = vec![0x4e00_u32, 0x4e01];
        let agent_b = vec![0x4e02_u32, 0x4e01];
        // A installs its own set; switching to B ships A ∪ B, not just B.
        let Plan::Transfer {
            codepoints: on_a, ..
        } = plan(&agent_a, Some(&BTreeSet::new()))
        else {
            panic!("expected Transfer for agent A");
        };
        assert_eq!(on_a, agent_a);
        let Plan::Transfer {
            epoch: on_b,
            codepoints: on_b_set,
        } = plan(&agent_b, Some(&set(&on_a)))
        else {
            panic!("expected Transfer for agent B");
        };
        assert_eq!(on_b_set, vec![0x4e00, 0x4e01, 0x4e02]);
        assert_eq!(on_b, glyph_font::glyph_epoch(&on_b_set));
        // Back to A: the union already covers it.
        assert!(matches!(plan(&agent_a, Some(&set(&on_b_set))), Plan::Keep));
    }

    #[test]
    fn plan_drops_the_cache_rather_than_a_label_when_the_union_overflows() {
        let installed: Vec<u32> = (0x4e00..0x4e00 + 60).collect();
        let needed: Vec<u32> = (0x5df3..0x5df3 + 40).collect();
        match plan(&needed, Some(&set(&installed))) {
            Plan::Transfer { codepoints, .. } => assert_eq!(codepoints, needed),
            other => panic!("expected Transfer of just the needed set, got {other:?}"),
        }
        // Even the needed set alone cannot exceed the budget.
        let too_many: Vec<u32> = (0x5df3..0x5df3 + (MAX_GLYPHS_PER_TXN as u32) + 1).collect();
        assert_eq!(
            plan(&too_many, Some(&set(&installed))),
            Plan::Degrade(Reject::TooManyGlyphs)
        );
    }

    #[test]
    fn plan_sorts_and_deduplicates_the_transfer_set() {
        // `encode_glyph_blob` requires strictly ascending codepoints.
        match plan(&[0x4e02, 0x4e00, 0x4e02], None) {
            Plan::Transfer { codepoints, epoch } => {
                assert_eq!(codepoints, vec![0x4e00, 0x4e02]);
                assert_eq!(epoch, glyph_font::glyph_epoch(&codepoints));
            }
            other => panic!("expected Transfer, got {other:?}"),
        }
    }
}
