//! Vendor HID (Report ID 6) link to the ESP32 deck.
//!
//! Ingress: unsolicited `EV_SELECTED_CHANGED` event frames (demuxed from
//! solicited RPC responses by the command byte) plus a 5s `status` poll that
//! doubles as heartbeat and reconciliation. Egress: coalesced `deck-view`
//! pushes carrying agent status dots and K-key labels, preceded by a
//! `CMD_GLYPHS` transfer whenever a label needs codepoints the deck's font does
//! not have yet. Framing follows the companion/firmware state-sync design; the A-key keyboard fast
//! path (event tap) stays enabled during the transition, so duplicate selections
//! must stay harmless (see the idempotent guard in `process_event`).
//!
//! Three threads share this link and the split is load-bearing:
//! - the reader (`run_session`) is the *only* thread that reads the device, so
//!   a glyph ACK can only reach the deck worker through it. Nothing slow may
//!   happen here: selections are queued for the device-event worker instead.
//!   That is why neither it nor `handle_report` takes an `AppHandle`.
//! - the device-event worker runs `process_event`, where an agent activation
//!   costs one to three seconds of AppleScript and subprocesses.
//! - the deck worker owns every write of `CMD_GLYPHS` and `CMD_DECK_VIEW`, and
//!   holds the device lock only for a whole burst of writes — never while
//!   waiting for the ACK the reader must deliver.
//!
//! A few items here are `pub` only so `examples/glyph_send.rs` can drive the
//! same encoder and framing as the deck worker; nothing else calls them.

use std::sync::{
    atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering},
    mpsc::{self, RecvTimeoutError},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

use hidapi::{BusType, HidApi};
use tauri::AppHandle;

use crate::glyph_font;
use crate::glyph_txn::{self, Event, Plan, Reject, Transaction, CMD_GLYPHS};
use crate::state::AppSnapshot;
use crate::{emit_state, process_event, AgentStatus, AppRuntime, DeviceEvent};

pub const VENDOR_ID: u16 = 0x303A;
pub const PRODUCT_ID: u16 = 0x8360;
pub const USAGE_PAGE: u16 = 0xFF00;
pub const USAGE: u16 = 0x01;
const REPORT_ID: u8 = 6;
const FAST_DECK_REPORT_ID: u8 = 7;
const CMD_FAST_DECK_VIEW: u8 = 22;
const CMD_FAST_GLYPHS: u8 = 23;
const FAST_GLYPH_CHUNK: usize = 285;
const FAST_GLYPH_MAX_FRAMES: usize = 57;

const CMD_STATUS: u8 = 5;
const CMD_DECK_VIEW: u8 = 18;
const CMD_HELLO: u8 = 19;
const CMD_REQUEST_DECK_VIEW: u8 = 0x41;
const EV_SELECTED_CHANGED: u8 = 0x40;

// Firmware CompanionNavigation v1: capability bit 2, a renewable two-second lease.
#[derive(Default)]
struct NavigationLink {
    supported: bool,
    token: u32,
    epoch: u32,
    pending: Option<u16>,
    ack: Option<Instant>,
    physical: bool,
}
static NAVIGATION_LINK: Mutex<NavigationLink> = Mutex::new(NavigationLink {
    supported: false,
    token: 0,
    epoch: 0,
    pending: None,
    ack: None,
    physical: false,
});
fn navigation_revision(state: &Arc<AppRuntime>, epoch: u32) -> u64 {
    (state
        .navigation
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .view
        .revision
        << 32)
        | epoch as u64
}
fn navigation_live(link: &NavigationLink) -> bool {
    link.supported
        && link
            .ack
            .is_some_and(|at| at.elapsed() < Duration::from_millis(1400))
}
pub fn companion_hardware_ready() -> bool {
    navigation_live(&NAVIGATION_LINK.lock().unwrap())
}
pub fn navigation_captures(key: &str) -> bool {
    let link = NAVIGATION_LINK.lock().unwrap();
    navigation_live(&link) && (link.physical || matches!(key, "A4" | "A5"))
}
fn reset_navigation() {
    // Link ownership is transient; the selected control context is user-owned.
    // Reconnects must not restore the previous Agent or dismiss its navigator.
    *NAVIGATION_LINK.lock().unwrap() = NavigationLink::default();
}
fn navigation_event_valid(event: &DeviceEvent, state: &Arc<AppRuntime>) -> bool {
    let Some((token, revision)) = event.navigation_stamp else {
        return true;
    };
    let link = NAVIGATION_LINK.lock().unwrap();
    navigation_live(&link)
        && token == link.token
        && (event.key.as_deref().is_some_and(|key| key.starts_with('A'))
            || (crate::session_navigation::active(state)
                && revision == navigation_revision(state, link.epoch)))
}

const POLL_INTERVAL: Duration = Duration::from_secs(5);
const REOPEN_DELAY: Duration = Duration::from_millis(2000);
const FLUSH_COALESCE: Duration = Duration::from_millis(30); // Reduced from 150ms for faster agent switching
const FULL_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
/// How long a failed glyph transfer keeps the next push on the ASCII path, so
/// a device that refuses the blob cannot stall every deck refresh.
const GLYPH_RETRY_BACKOFF: Duration = Duration::from_secs(10);

// This process owns one deck. A waiting writer must get the mutex after the
// reader's current 50 ms read, instead of losing it to the reader's next read.
static DEVICE_WRITE_WAITERS: AtomicUsize = AtomicUsize::new(0);

struct DeviceWriteIntent;

impl DeviceWriteIntent {
    fn new() -> Self {
        DEVICE_WRITE_WAITERS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for DeviceWriteIntent {
    fn drop(&mut self) {
        DEVICE_WRITE_WAITERS.fetch_sub(1, Ordering::SeqCst);
    }
}

struct BusMonitor {
    stop: Arc<AtomicBool>,
}

impl BusMonitor {
    fn start(current: BusType) -> (Self, mpsc::Receiver<()>) {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (tx, rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let Ok(mut api) = HidApi::new() else { return };
            while !thread_stop.load(Ordering::SeqCst) {
                if api.refresh_devices().is_ok()
                    && matches!(
                        (preferred_deck_bus(&api), current),
                        (Some(BusType::Usb), BusType::Bluetooth)
                            | (Some(BusType::Bluetooth), BusType::Usb)
                    )
                {
                    let _ = tx.try_send(());
                    return;
                }
                thread::sleep(Duration::from_millis(500));
            }
        });
        (Self { stop }, rx)
    }
}

impl Drop for BusMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Non-blocking hint that the deck view should be re-pushed.
pub fn request_deck_push(state: &Arc<AppRuntime>) {
    if !state.hardware_enabled.load(Ordering::Relaxed) {
        return;
    }
    if let Some(tx) = state.deck_push.lock().expect("deck push poisoned").as_ref() {
        let _ = tx.send(Instant::now());
    }
}

fn force_deck_push(state: &Arc<AppRuntime>, reason: &str) {
    eprintln!("[vendor_hid] force deck push: {reason}");
    *state
        .deck_last_payload
        .lock()
        .expect("deck payload poisoned") = None;
    request_deck_push(state);
}

fn retry_deck_push(state: &Arc<AppRuntime>) {
    // A retry can already be in flight when the preceding view finishes. Give
    // a just-written view time to satisfy the request before invalidating the
    // dedupe key; the device will ask again if the view truly went missing.
    let recently_sent = state
        .deck_last_payload
        .lock()
        .expect("deck payload poisoned")
        .as_ref()
        .is_some_and(|(_, sent_at)| sent_at.elapsed() < Duration::from_secs(1));
    if recently_sent {
        eprintln!("[vendor_hid] suppressing deck retry after recent push");
        request_deck_push(state);
    } else {
        force_deck_push(state, "device retry without confirmed fast view");
    }
}

/// Forget everything the companion believes about the device's glyph font.
/// A boot id the companion has not seen before is the only signal that the
/// device dropped the font it installed, and there are two such boots: a real
/// reboot, and this process talking to the deck for the first time (it knows
/// nothing about the font then, so resending is both safe and necessary). A
/// mere USB reconnect reports the *same* boot id and must not clear anything,
/// or every replug would resend a ~95-frame blob for nothing.
fn forget_glyphs(state: &Arc<AppRuntime>) {
    *state.glyph_installed.lock().expect("glyph set poisoned") = None;
    *state
        .glyph_backoff_until
        .lock()
        .expect("glyph backoff poisoned") = None;
    state.glyph_unsupported.store(false, Ordering::SeqCst);
}

/// Queue an activation for the device-event worker instead of running it here.
///
/// `run_session` is the only thread that reads the deck, and a glyph ACK reaches
/// the deck worker through it. Agent activation costs one to three seconds of
/// AppleScript and subprocesses, so running it on the reader thread strands the
/// ACK for exactly that long and holds up the screen with it.
fn post_device_event(state: &Arc<AppRuntime>, event: DeviceEvent) {
    let sender = state
        .device_event_tx
        .lock()
        .expect("device event poisoned")
        .clone();
    let Some(sender) = sender else {
        eprintln!("[vendor_hid] no device event worker; dropping {event:?}");
        return;
    };
    // The runtime holds a sender for its whole life, so a failure here means the
    // worker panicked; its own panic message is the diagnostic.
    let _ = sender.send(event);
}

pub fn start(app: AppHandle, state: Arc<AppRuntime>) {
    let (tx, rx) = mpsc::channel::<Instant>();
    *state.deck_push.lock().expect("deck push poisoned") = Some(tx);
    // The reader thread is the only one that sees the device's responses, so
    // glyph ACKs reach the deck worker through this channel instead.
    let (ack_tx, ack_rx) = mpsc::channel::<(u8, u16, u8)>();
    *state.glyph_ack.lock().expect("glyph ack poisoned") = Some(ack_tx);

    let device: Arc<Mutex<Option<hidapi::HidDevice>>> = Arc::new(Mutex::new(None));
    let seq = Arc::new(AtomicU16::new(0));
    let (audio_ack_tx, audio_ack_rx) = mpsc::channel();
    *state.notification_audio.ack.lock().unwrap() = Some(audio_ack_tx);
    {
        let state = state.clone();
        let device = device.clone();
        let seq = seq.clone();
        thread::spawn(move || audio_worker(state, device, seq, audio_ack_rx));
    }

    {
        // One consumer at a time: two activations racing each other make
        // `activation.rs`'s observed-pane recheck misreport a successful focus
        // as a failed one and focus the wrong window.
        let (event_tx, event_rx) = mpsc::channel::<DeviceEvent>();
        *state.device_event_tx.lock().expect("device event poisoned") = Some(event_tx);
        let app = app.clone();
        let state = state.clone();
        thread::spawn(move || loop {
            let Ok(event) = event_rx.recv() else {
                return;
            };
            if !navigation_event_valid(&event, &state) {
                continue;
            }
            let navigation = event.navigation_stamp.is_some();
            let agent_key = event.key.as_deref().is_some_and(|key| key.starts_with('A'));
            process_event(event, &app, &state);
            if navigation && agent_key {
                let mut link = NAVIGATION_LINK.lock().unwrap();
                link.epoch = link.epoch.wrapping_add(1);
                link.physical = crate::session_navigation::active(&state);
            }
        });
    }

    {
        let device = device.clone();
        let seq = seq.clone();
        let app = app.clone();
        let state = state.clone();
        thread::spawn(move || loop {
            match rx.recv_timeout(FULL_REFRESH_INTERVAL) {
                Ok(queued_at) => {
                    thread::sleep(FLUSH_COALESCE);
                    while rx.try_recv().is_ok() {}
                    let delay = queued_at.elapsed();
                    if delay > Duration::from_millis(100) {
                        eprintln!("[vendor_hid] deck push queue delay {delay:?}");
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            push_deck_view(&app, &state, &device, &seq, &ack_rx);
        });
    }

    thread::spawn(move || {
        let Ok(mut api) = HidApi::new() else {
            eprintln!("[vendor_hid] hidapi init failed");
            return;
        };
        #[cfg(target_os = "macos")]
        api.set_open_exclusive(false);
        loop {
            if let Err(error) = api.refresh_devices() {
                eprintln!("[vendor_hid] device refresh failed: {error}");
            }
            let Some((handle, bus)) = open_deck(&api) else {
                set_link(&app, &state, false);
                thread::sleep(REOPEN_DELAY);
                continue;
            };
            eprintln!("[vendor_hid] deck connected");
            *device.lock().expect("device poisoned") = Some(handle);
            set_link(&app, &state, true);
            write_locked(&device, command_frame(CMD_HELLO, &seq, &[]));
            run_session(&state, &device, &seq, bus);
            *device.lock().expect("device poisoned") = None;
            eprintln!("[vendor_hid] deck disconnected");
            set_link(&app, &state, false);
            thread::sleep(REOPEN_DELAY);
        }
    });
}

/// The deck link's read loop. Deliberately takes no `AppHandle`: everything
/// here must stay cheap so the next report — a glyph ACK included — is read the
/// moment it arrives. Deck selections are handed to the device-event worker and
/// every push to the deck worker.
fn run_session(
    state: &Arc<AppRuntime>,
    device: &Arc<Mutex<Option<hidapi::HidDevice>>>,
    seq: &AtomicU16,
    bus: BusType,
) {
    let (_bus_monitor, bus_changed) = BusMonitor::start(bus);
    let mut boot_id: Option<u32> = None;
    let mut last_seq: Option<u16> = None;
    // Backdate so the first status exchange happens immediately: after a replug
    // that reply is the only thing which notices the deck rebooted, and waiting
    // a whole poll window would push recovery past the 5 s budget.
    let mut last_poll = Instant::now() - POLL_INTERVAL;
    let mut last_lease = Instant::now() - Duration::from_secs(1);
    let mut last_navigation_slot = None;
    loop {
        if bus_changed.try_recv().is_ok() {
            eprintln!("[vendor_hid] preferred deck bus changed; reopening");
            return;
        }
        // Read from device under lock, then release before processing: the deck
        // worker writes its frames through the same lock, and `handle_report`
        // must be able to hand work to the other threads without waiting on it.
        let report = {
            while DEVICE_WRITE_WAITERS.load(Ordering::SeqCst) != 0 {
                thread::sleep(Duration::from_millis(1));
            }
            let guard = device.lock().expect("device poisoned");
            let Some(handle) = guard.as_ref() else {
                return;
            };
            let mut report = [0_u8; 64];
            match handle.read_timeout(&mut report, 50) {
                Ok(0) => None,
                Ok(_) => Some(report),
                Err(error) => {
                    eprintln!("[vendor_hid] read failed: {error}");
                    return;
                }
            }
        };
        // Process the report without the device lock: `handle_report` queues
        // work for the other threads and must return to reading immediately.
        if let Some(report) = report {
            handle_report(&mut boot_id, &mut last_seq, &report, state);
        }
        if last_lease.elapsed() >= Duration::from_millis(400) {
            last_lease = Instant::now();
            let enabled = {
                let config = state.config.lock().unwrap();
                config.assigned_agent("A4") == Some("companion")
                    || config.assigned_agent("A5") == Some("companion")
            };
            let mut link = NAVIGATION_LINK.lock().unwrap();
            // An expired ACK only gates input through navigation_live(). Keep
            // the selected context and renew the lease, even after a slow write.
            if enabled && link.supported {
                let revision = navigation_revision(state, link.epoch);
                let mut body = vec![1, u8::from(crate::session_navigation::active(state))];
                body.extend_from_slice(&link.token.to_le_bytes());
                body.extend_from_slice(&revision.to_le_bytes());
                let frame = command_frame(30, seq, &body);
                link.pending = Some(u16::from_le_bytes([frame[5], frame[6]]));
                drop(link);
                write_locked(device, frame);
                let active = state.snapshot.read().unwrap().active_agent.clone();
                let slot = {
                    let config = state.config.lock().unwrap();
                    crate::profile::AGENT_SLOTS
                        .iter()
                        .position(|key| config.assigned_agent(key) == active.as_deref())
                };
                if active.is_some() && slot != last_navigation_slot {
                    if let Some(slot) = slot {
                        let mut frame = command_frame(17, seq, &[]);
                        frame[7] = slot as u8; // Existing CMD_SET_SELECT_AGENT legacy argument offset.
                        write_locked(device, frame);
                    }
                    last_navigation_slot = slot;
                }
            }
        }
        if last_poll.elapsed() >= POLL_INTERVAL {
            last_poll = Instant::now();
            write_locked(&device, command_frame(CMD_STATUS, seq, &[]));
        }
    }
}

fn write_locked(device: &Mutex<Option<hidapi::HidDevice>>, frame: [u8; 64]) {
    let guard = device.lock().expect("device poisoned");
    if let Some(handle) = guard.as_ref() {
        match handle.write(&frame) {
            Ok(64) => {}
            Ok(written) => eprintln!("[vendor_hid] short command write: {written}/64 bytes"),
            Err(error) => eprintln!("[vendor_hid] command write failed: {error}"),
        }
    }
}

fn preferred_deck_bus(api: &HidApi) -> Option<BusType> {
    let mut bluetooth = false;
    for info in api.device_list() {
        if info.vendor_id() == VENDOR_ID
            && info.product_id() == PRODUCT_ID
            && info.usage_page() == USAGE_PAGE
            && info.usage() == USAGE
        {
            match info.bus_type() {
                BusType::Usb => return Some(BusType::Usb),
                BusType::Bluetooth => bluetooth = true,
                _ => {}
            }
        }
    }
    bluetooth.then_some(BusType::Bluetooth)
}

fn open_deck(api: &HidApi) -> Option<(hidapi::HidDevice, BusType)> {
    // The deck drives one vendor channel at a time and puts the whole Report 6
    // conversation on the cable as soon as a host mounts it, so USB is the
    // preferred bus and BLE only carries the link while the cable is dark.
    let mut usb = None;
    let mut bluetooth = None;
    for info in api.device_list() {
        if info.vendor_id() != VENDOR_ID
            || info.product_id() != PRODUCT_ID
            || info.usage_page() != USAGE_PAGE
            || info.usage() != USAGE
        {
            continue;
        }
        match info.bus_type() {
            BusType::Usb if usb.is_none() => usb = Some(info),
            BusType::Bluetooth if bluetooth.is_none() => bluetooth = Some(info),
            _ => {}
        }
    }
    let info = usb.or(bluetooth)?;
    match api.open_path(info.path()) {
        Ok(handle) => {
            eprintln!("[vendor_hid] opened deck bus={:?}", info.bus_type());
            Some((handle, info.bus_type()))
        }
        Err(error) => {
            eprintln!(
                "[vendor_hid] found deck at {:?}, but open failed: {error}",
                info.path()
            );
            None
        }
    }
}

/// Splits solicited RPC responses (`cmd | 0x80`) from unsolicited event
/// frames, applies the `(boot_id, seq)` watermark, and queues deck selections
/// for the device-event worker.
fn handle_report(
    boot_id: &mut Option<u32>,
    last_seq: &mut Option<u16>,
    report: &[u8; 64],
    state: &Arc<AppRuntime>,
) {
    if report[1] != b'K' || report[2] != b'M' || report[3] != 1 {
        return;
    }
    let command = report[4];
    if command & 0x80 != 0 {
        if command & 0x7f == 30 {
            let mut link = NAVIGATION_LINK.lock().unwrap();
            if link.pending == Some(u16::from_le_bytes([report[5], report[6]])) && report[7] == 0 {
                link.ack = Some(Instant::now());
            }
            return;
        }

        if (crate::notification_audio::CMD_CAPS..=29).contains(&(command & 0x7f)) {
            if let Some(tx) = state.notification_audio.ack.lock().unwrap().as_ref() {
                let _ = tx.send((
                    command,
                    u16::from_le_bytes([report[5], report[6]]),
                    report[7],
                ));
            }
            return;
        }

        if (command & 0x7F == CMD_HELLO || command & 0x7F == CMD_STATUS) && report[7] == 0 {
            {
                let mut link = NAVIGATION_LINK.lock().unwrap();
                link.supported = report[31] & 2 != 0;
                if link.token == 0 {
                    link.token = u32::from_le_bytes(
                        uuid::Uuid::new_v4().as_bytes()[..4].try_into().unwrap(),
                    )
                    .max(1);
                }
            }
            let supported = report[31] & 1 != 0 && !state.fast_deck_failed.load(Ordering::SeqCst);
            let previous = state.fast_deck_supported.swap(supported, Ordering::SeqCst);
            if command & 0x7F == CMD_HELLO || previous != supported {
                eprintln!("[vendor_hid] BLE fast DeckView supported={supported}");
            }
        }
        if command & 0x7F == CMD_FAST_DECK_VIEW || command & 0x7F == CMD_FAST_GLYPHS {
            let ack_seq = u16::from_le_bytes([report[5], report[6]]);
            if let Some(tx) = state.glyph_ack.lock().expect("glyph ack poisoned").as_ref() {
                let _ = tx.send((command, ack_seq, report[7]));
            }
        }
        if command & 0x7F == CMD_GLYPHS {
            // Relay to the deck worker: it is the only writer of CMD_GLYPHS and
            // it cannot read the device itself without deadlocking this thread.
            let ack_seq = u16::from_le_bytes([report[5], report[6]]);
            if let Some(tx) = state.glyph_ack.lock().expect("glyph ack poisoned").as_ref() {
                let _ = tx.send((command, ack_seq, report[7]));
            }
        }
        if command & 0x7F == CMD_DECK_VIEW {
            // The firmware stays silent for every report-6 frame but the last,
            // and answers that one only after applying the view, so this is the
            // same apply confirmation the fast channel gives the deck worker.
            let ack_seq = u16::from_le_bytes([report[5], report[6]]);
            if let Some(tx) = state.glyph_ack.lock().expect("glyph ack poisoned").as_ref() {
                let _ = tx.send((command, ack_seq, report[7]));
            }
        }
        if command & 0x7F == CMD_STATUS && report[7] == 0 {
            let status_boot = u32::from_le_bytes([report[27], report[28], report[29], report[30]]);
            let changed_this_session = boot_id
                .replace(status_boot)
                .is_some_and(|previous| previous != status_boot);
            // The runtime copy survives the link dropping, so a reboot behind a
            // cable gap is still a reboot the moment the device reappears.
            let previous_boot = {
                let mut last_boot = state.deck_boot_id.lock().expect("boot id poisoned");
                let previous = *last_boot;
                *last_boot = Some(status_boot);
                previous
            };
            if changed_this_session {
                *last_seq = None;
                reset_navigation();
            }
            if previous_boot != Some(status_boot) {
                eprintln!(
                    "[vendor_hid] deck boot id {previous_boot:?} -> {status_boot:#x}; resyncing glyphs"
                );
                forget_glyphs(state);
                force_deck_push(state, "device boot id changed");
            }
            follow_slot(report[19], "reconcile", state);
        }
        return;
    }
    if command != EV_SELECTED_CHANGED && command != CMD_REQUEST_DECK_VIEW && command != 0x42 {
        let status = report[7];
        if status != 0 {
            eprintln!("[vendor_hid] command {command:#04x} rejected: status {status}");
        }
        return;
    }
    let seq = u16::from_le_bytes([report[5], report[6]]);
    let boot = u32::from_le_bytes([report[7], report[8], report[9], report[10]]);
    if *boot_id != Some(boot) {
        if let Some(existing) = *boot_id {
            reset_navigation();
            eprintln!("[vendor_hid] deck rebooted (boot_id {existing} -> {boot}); resyncing");
        }
        *boot_id = Some(boot);
        *last_seq = None;
    }
    if !accept_event_sequence(last_seq, seq) {
        return;
    }
    if command == 0x42 {
        if report[11] != 1 {
            return;
        }
        let stamp = (
            u32::from_le_bytes(report[14..18].try_into().unwrap()),
            u64::from_le_bytes(report[18..26].try_into().unwrap()),
        );
        let (kind, key, delta) = match (report[12], report[13]) {
            (1, value @ 0..=14) => (
                "key_down",
                Some(if value < 5 {
                    format!("A{}", value + 1)
                } else {
                    format!("K{}", value - 4)
                }),
                None,
            ),
            (2, 1) => ("encoder", None, Some(1)),
            (2, 255) => ("encoder", None, Some(-1)),
            (3, 0) => ("key_down", Some("ENCODER_PRESS".into()), None),
            _ => return,
        };
        post_device_event(
            state,
            DeviceEvent {
                navigation_stamp: Some(stamp),
                kind: kind.into(),
                key,
                delta,
            },
        );
        return;
    }
    if command == CMD_REQUEST_DECK_VIEW {
        // The device re-asks on its own timer while a view is late, so this is
        // not a keypress and must not re-activate: a stale slot is the only
        // thing worth the seconds an activation costs.
        follow_slot(report[11], "request", state);
        // Hand the push to the deck worker: pushing from here would run the
        // glyph transaction on the only thread that can read its ACK.
        // Both channels answer the last frame only after the firmware applies
        // the view, so a request racing with that ACK must not invalidate
        // deduplication and send the same view again; changed snapshots still
        // pass the payload comparison in push_deck_view. Report 6 is unconfirmed
        // until that ACK arrives, so a push that never landed cannot settle the
        // dedupe key and this retry is what puts the view back.
        if state.fast_deck_supported.load(Ordering::SeqCst) {
            request_deck_push(state);
        } else {
            retry_deck_push(state);
        }
        return;
    }
    if report[12] == 1 {
        return;
    } // Host selection must not undo exact session focus.
    let slot = report[11];
    if slot <= 4 {
        *state
            .deck_last_selection_at
            .lock()
            .expect("selection timestamp poisoned") = Some((seq, Instant::now()));
        // Always queued, even for the slot already showing: the deck's A-keys
        // double as "bring that window back", which `process_event` handles as a
        // refocus. This frame has already been through the `(boot_id, seq)`
        // watermark, so it cannot arrive twice.
        post_selected_slot(state, slot);
    }
}

/// Both unsolicited frame kinds consume the firmware's event sequence.
/// Counting only selections mistakes every RequestDeckView for a lost event.
fn accept_event_sequence(last_seq: &mut Option<u16>, seq: u16) -> bool {
    if let Some(previous) = *last_seq {
        let delta = seq.wrapping_sub(previous);
        if delta == 0 {
            return false;
        }
        if delta > 0x7FFF {
            eprintln!("[vendor_hid] stale event {seq} behind watermark {previous}, dropped");
            return false;
        }
        if delta > 1 {
            eprintln!("[vendor_hid] event gap: {previous} -> {seq}; poll will reconcile");
        }
    }
    *last_seq = Some(seq);
    true
}

/// Follow the device's selection without waiting for it. A mismatch between the
/// deck's slot and the companion's `active_agent` means a selection event went
/// missing (or the deck rebooted), and the device is the one that owns the
/// choice. `why` only labels the log.
fn follow_slot(slot: u8, why: &str, state: &Arc<AppRuntime>) {
    if slot > 4 {
        return;
    }
    let key = format!("A{}", slot + 1);
    let Some(agent_id) = state
        .config
        .lock()
        .expect("config poisoned")
        .assigned_agent(&key)
        .map(str::to_owned)
    else {
        return;
    };
    if agent_id == "companion" || crate::session_navigation::active(state) {
        return;
    }
    let already_active = state
        .snapshot
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .active_agent
        .as_deref()
        == Some(agent_id.as_str());
    if already_active {
        return;
    }
    eprintln!("[vendor_hid] {why}: device slot {} -> {agent_id}", slot + 1);
    post_selected_slot(state, slot);
}

fn post_selected_slot(state: &Arc<AppRuntime>, slot: u8) {
    let started = Instant::now();
    let key = format!("A{}", slot + 1);
    let agent_id = state
        .config
        .lock()
        .expect("config poisoned")
        .assigned_agent(&key)
        .map(str::to_owned);
    // Legacy selection reports cannot establish input ownership. Companion
    // enters only through the advertised, acknowledged v1 navigation lease.
    if agent_id.as_deref() == Some("companion") || crate::session_navigation::active(state) {
        eprintln!("[vendor_hid] Companion physical navigation requires firmware input isolation");
        return;
    }
    let changed = agent_id.as_ref().is_some_and(|agent_id| {
        state
            .snapshot
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .active_agent
            .as_deref()
            != Some(agent_id.as_str())
    });
    if changed
        && agent_id.as_deref() != Some("companion")
        && !crate::session_navigation::active(state)
    {
        crate::activation::prime_agent_selection(agent_id.as_deref().unwrap(), state);
        // The screen can now be refreshed from the primed snapshot without
        // waiting for focus, tray updates, or the event worker to finish.
        request_deck_push(state);
    }
    let event = DeviceEvent {
        navigation_stamp: None,
        kind: if changed {
            "agent_key_primed"
        } else {
            "agent_key"
        }
        .into(),
        key: Some(key),
        delta: None,
    };
    post_device_event(state, event);
    let elapsed = started.elapsed();
    if elapsed > Duration::from_millis(50) {
        eprintln!("[vendor_hid] selected slot {slot} state update took {elapsed:?}");
    }
}

/// Wall-clock split of one deck push, so a slow screen says which stage stalled.
/// `transfer` is frames written, `ack_wait` is the device thinking about them,
/// and `deck` is the 289-byte view itself.
#[derive(Default)]
struct Phases {
    rasterize: Duration,
    transfer: Duration,
    ack_wait: Duration,
    deck: Duration,
}

impl Phases {
    fn summary(&self) -> String {
        let Self {
            rasterize,
            transfer,
            ack_wait,
            deck,
        } = *self;
        format!("rasterize={rasterize:?} transfer={transfer:?} ack_wait={ack_wait:?} deck={deck:?}")
    }
}

/// Publishes the deck view, transferring any glyphs its labels need first.
///
/// Glyph frames must be ACKed *before* the `deck-view` revision is bumped: the
/// device applies a view only when its `rev` is newer, so bumping first would
/// paint labels the current font cannot draw as placeholder boxes until some
/// later push. Every failure path here converges on the ASCII-filtered
/// payload, never on a blank label.
fn push_deck_view(
    _app: &AppHandle,
    state: &Arc<AppRuntime>,
    device: &Mutex<Option<hidapi::HidDevice>>,
    seq: &AtomicU16,
    ack_rx: &mpsc::Receiver<(u8, u16, u8)>,
) {
    let started = Instant::now();
    let mut phases = Phases::default();
    let snapshot = state.read();
    let selection_at = *state
        .deck_last_selection_at
        .lock()
        .expect("selection timestamp poisoned");
    // The HID reader holds the device mutex for each 50 ms read. Checking
    // connectivity by taking that mutex here has no write-intent priority and
    // can starve a queued A-key refresh before timing even starts. The state
    // snapshot is kept in sync by set_link; writes still handle disconnects.
    if !snapshot.device.connected {
        return;
    }
    let unicode = glyphs_allowed(state);
    let (mut payload, labels) = {
        let config = state.config.lock().expect("config poisoned");
        build_deck_payload(&snapshot, &config, unicode)
    };

    let mut sent = deck_dedupe_key(&payload);
    {
        let previous = state
            .deck_last_payload
            .lock()
            .expect("deck payload poisoned");
        if let Some((previous, sent_at)) = previous.as_ref() {
            if previous == &sent && sent_at.elapsed() < FULL_REFRESH_INTERVAL {
                return;
            }
            if sent_at.elapsed() < Duration::from_secs(5) {
                let changed: Vec<_> = previous
                    .iter()
                    .zip(&sent)
                    .enumerate()
                    .filter_map(|(index, (before, after))| (before != after).then_some(index))
                    .take(32)
                    .collect();
                eprintln!("[vendor_hid] deck payload changed bytes={changed:?}");
            }
        }
    }

    if unicode && deliver_glyphs(state, &labels, device, seq, ack_rx, &mut phases).is_err() {
        let config = state.config.lock().expect("config poisoned");
        payload = build_deck_payload(&snapshot, &config, false).0;
        sent = deck_dedupe_key(&payload);
    }

    let rev = state.deck_rev.fetch_add(1, Ordering::SeqCst) + 1;
    payload[..4].copy_from_slice(&rev.to_le_bytes());
    let deck_started = Instant::now();
    let mut fast = false;
    let written = if state.fast_deck_supported.load(Ordering::SeqCst) {
        match write_fast_deck_payload(device, seq, &payload)
            .and_then(|sent_seq| wait_for_fast_deck_ack(ack_rx, sent_seq))
        {
            Ok(()) => {
                fast = true;
                true
            }
            Err(error) => {
                eprintln!("[vendor_hid] BLE fast DeckView failed: {error}; using Report 6");
                state.fast_deck_failed.store(true, Ordering::SeqCst);
                reset_navigation();
                state.fast_deck_supported.store(false, Ordering::SeqCst);
                write_confirmed_deck_payload(device, seq, &payload, ack_rx)
            }
        }
    } else {
        write_confirmed_deck_payload(device, seq, &payload, ack_rx)
    };
    phases.deck = deck_started.elapsed();
    eprintln!(
        "[vendor_hid] deck push rev={rev} fast={fast} unicode={unicode} total={:?} {}",
        started.elapsed(),
        phases.summary()
    );
    if let Some((selection_seq, observed_at)) = selection_at {
        if observed_at.elapsed() < Duration::from_secs(5) {
            eprintln!(
                "[vendor_hid] deck rev={rev} after selected event seq={selection_seq} observed_for={:?}",
                observed_at.elapsed()
            );
        }
    }
    if written {
        *state
            .deck_last_payload
            .lock()
            .expect("deck payload poisoned") = Some((sent, Instant::now()));
    }
}

/// The usage age is derived from the local clock. It changes every second
/// even when the underlying snapshot does not. The 30-second full refresh
/// still sends an accurate age, but age alone should not queue another BLE
/// write and delay a physical Agent switch.
fn deck_dedupe_key(payload: &[u8]) -> Vec<u8> {
    let mut key = payload.to_vec();
    debug_assert_eq!(key.len(), 289);
    if key.len() >= 9 {
        // Last fields: age_seconds u16, weekly_percent u8,
        // weekly_reset_time [u8; 6].
        let age_offset = key.len() - 9;
        key[age_offset..age_offset + 2].fill(0);
    }
    key
}

/// True while a Unicode payload is worth attempting at all.
fn glyphs_allowed(state: &AppRuntime) -> bool {
    if state.glyph_unsupported.load(Ordering::SeqCst) {
        return false;
    }
    match *state
        .glyph_backoff_until
        .lock()
        .expect("glyph backoff poisoned")
    {
        None => true,
        Some(until) => Instant::now() >= until,
    }
}

/// Decide what the device's font must gain for this payload to render, and get
/// it there. `Ok(())` means the Unicode labels already are, or now are, on the
/// device; `Err` says to resend with the ASCII filter.
fn deliver_glyphs(
    state: &Arc<AppRuntime>,
    labels: &[String],
    device: &Mutex<Option<hidapi::HidDevice>>,
    seq: &AtomicU16,
    ack_rx: &mpsc::Receiver<(u8, u16, u8)>,
    phases: &mut Phases,
) -> Result<(), Reject> {
    let codepoints = glyph_font::non_ascii_codepoints(labels.iter().map(String::as_str));
    // Cloned rather than held: a transfer takes seconds, and a reboot noticed
    // during it must be able to clear what the device no longer has.
    let installed = state
        .glyph_installed
        .lock()
        .expect("glyph set poisoned")
        .clone();
    match glyph_txn::plan(&codepoints, installed.as_ref()) {
        // Nothing the device cannot already draw: send no glyph frames.
        Plan::Keep => Ok(()),
        Plan::Degrade(reason) => Err(refuse_glyphs(state, reason)),
        Plan::Transfer { epoch, codepoints } => {
            match run_glyph_transfer(state, &codepoints, epoch, device, seq, ack_rx, phases) {
                Ok(()) => Ok(()),
                Err(reason) => Err(refuse_glyphs(state, reason)),
            }
        }
    }
}

/// Record why glyphs are unavailable and hand the reason back for logging.
fn refuse_glyphs(state: &Arc<AppRuntime>, reason: Reject) -> Reject {
    match reason {
        // The firmware predates the command: stop probing it for good.
        Reject::DeviceUnsupported => state.glyph_unsupported.store(true, Ordering::SeqCst),
        // A dead or vanished link says nothing about the protocol, so the next
        // push after reopening should try glyphs again immediately.
        Reject::Transport => {}
        _ => {
            *state
                .glyph_backoff_until
                .lock()
                .expect("glyph backoff poisoned") = Some(Instant::now() + GLYPH_RETRY_BACKOFF);
        }
    }
    eprintln!("[vendor_hid] glyphs unavailable ({reason:?}); falling back to ASCII labels");
    reason
}

/// Drive a whole blob transfer to completion.
///
/// Each attempt writes all of its frames in one device-lock acquisition, because
/// the reader thread holds that lock for a whole `read_timeout` slice and a
/// per-frame acquisition would pay that slice once per frame. The lock is then
/// dropped *before* the ACK is waited for: only the reader can deliver that ACK,
/// and a writer asleep in here must never be the reason it cannot get through.
fn run_glyph_transfer(
    state: &Arc<AppRuntime>,
    codepoints: &[u32],
    epoch: u32,
    device: &Mutex<Option<hidapi::HidDevice>>,
    seq: &AtomicU16,
    ack_rx: &mpsc::Receiver<(u8, u16, u8)>,
    phases: &mut Phases,
) -> Result<(), Reject> {
    let rasterized = Instant::now();
    let records = glyph_font::rasterize_codepoints(codepoints);
    phases.rasterize += rasterized.elapsed();
    // A codepoint CoreText could not draw is dropped in silence, and a blob that
    // carries fewer glyphs than we are about to record as installed would leave
    // placeholder boxes on screen forever: the next push plans `Keep` and never
    // resends. Anything short of the full set is a failed transaction, and a
    // failed transaction falls back to ASCII.
    if records.len() != codepoints.len() {
        eprintln!(
            "[vendor_hid] rasterized {}/{} glyphs; falling back to ASCII",
            records.len(),
            codepoints.len()
        );
        return Err(Reject::Refused);
    }
    let Some(blob) = glyph_font::encode_glyph_blob(&records, epoch) else {
        return Err(Reject::Refused);
    };
    while ack_rx.try_recv().is_ok() {}
    if state.fast_deck_supported.load(Ordering::SeqCst) {
        if let Some(reports) = fast_glyph_reports(seq, &blob) {
            let started = Instant::now();
            eprintln!(
                "[vendor_hid] BLE fast glyph transfer: {} glyphs, {} B, {} frames",
                codepoints.len(),
                blob.len(),
                reports.len()
            );
            let transfer_started = Instant::now();
            let result = {
                let _intent = DeviceWriteIntent::new();
                let guard = device.lock().expect("device poisoned");
                match guard.as_ref() {
                    Some(handle) => reports.iter().try_for_each(|report| {
                        handle
                            .send_feature_report(report)
                            .map_err(|error| error.to_string())
                    }),
                    None => Err("device disconnected".into()),
                }
            };
            phases.transfer += transfer_started.elapsed();
            let result = result.and_then(|()| {
                let ack_started = Instant::now();
                let ack = wait_for_ack(
                    ack_rx,
                    CMD_FAST_GLYPHS,
                    u16::from_le_bytes([reports.last().unwrap()[5], reports.last().unwrap()[6]]),
                    Duration::from_secs(2),
                );
                phases.ack_wait += ack_started.elapsed();
                ack
            });
            match result {
                Ok(()) => {
                    *state.glyph_installed.lock().expect("glyph set poisoned") =
                        Some(codepoints.iter().copied().collect());
                    *state
                        .glyph_backoff_until
                        .lock()
                        .expect("glyph backoff poisoned") = None;
                    eprintln!(
                        "[vendor_hid] BLE fast glyph transfer done in {:?}",
                        started.elapsed()
                    );
                    return Ok(());
                }
                Err(error) => {
                    eprintln!("[vendor_hid] BLE fast glyph transfer failed ({error}); retrying with legacy frames");
                    state.fast_deck_supported.store(false, Ordering::SeqCst);
                    state.fast_deck_failed.store(true, Ordering::SeqCst);
                }
            }
        }
    }
    let Some(mut txn) = Transaction::new(&blob, epoch, || command_frame(CMD_GLYPHS, seq, &[]))
    else {
        return Err(Reject::Refused);
    };
    // Anything still queued belongs to an earlier attempt.
    while ack_rx.try_recv().is_ok() {}
    let started = Instant::now();
    eprintln!(
        "[vendor_hid] glyph transfer: {} glyphs, {} B, {} frames, epoch {epoch:#010x}",
        codepoints.len(),
        blob.len(),
        txn.frame_count()
    );
    loop {
        // One attempt's frames, written with the device held.
        let burst = Instant::now();
        let outcome = {
            let _intent = DeviceWriteIntent::new();
            let guard = device.lock().expect("device poisoned");
            match guard.as_ref() {
                None => Event::Abandon {
                    reason: Reject::Transport,
                },
                Some(handle) => loop {
                    match txn.next(Instant::now()) {
                        Event::Frame(frame) => match handle.write(&frame) {
                            Ok(64) => {}
                            Ok(written) => {
                                eprintln!("[vendor_hid] short glyph write: {written}/64 bytes");
                                txn.abandon(Reject::Transport);
                            }
                            Err(error) => {
                                eprintln!("[vendor_hid] glyph write failed: {error}");
                                txn.abandon(Reject::Transport);
                            }
                        },
                        // The 4 ms pacing is the only sleep allowed in here, and
                        // it belongs in the lock on purpose: releasing between
                        // frames would let each one queue behind the reader's
                        // `read_timeout` slice and turn 84 ms into seconds.
                        Event::Wait(gap) if !txn.awaiting_ack() => thread::sleep(gap),
                        other => break other,
                    }
                },
            }
        };
        phases.transfer += burst.elapsed();
        match outcome {
            // The reader is the only source of this event and it is not waiting
            // on us any more, so the wait here is the device's own build time.
            Event::Wait(until) => {
                let waited = Instant::now();
                match ack_rx.recv_timeout(until) {
                    Ok((cmd, ack_seq, status)) => {
                        txn.on_ack(cmd, ack_seq, status, Instant::now());
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        txn.abandon(Reject::Transport);
                    }
                }
                phases.ack_wait += waited.elapsed();
            }
            Event::Done { .. } => {
                // The blob replaced the device's whole font, so exactly the set
                // it carried is now installed — including whatever the labels
                // before this one needed.
                *state.glyph_installed.lock().expect("glyph set poisoned") =
                    Some(codepoints.iter().copied().collect());
                *state
                    .glyph_backoff_until
                    .lock()
                    .expect("glyph backoff poisoned") = None;
                eprintln!(
                    "[vendor_hid] glyph transfer done in {:?} ({} attempt(s))",
                    started.elapsed(),
                    txn.attempt()
                );
                return Ok(());
            }
            Event::Abandon { reason } => {
                eprintln!(
                    "[vendor_hid] glyph transfer failed after {:?}: {reason:?}",
                    started.elapsed()
                );
                return Err(reason);
            }
            // The loop above takes every `Frame` before asking for another.
            Event::Frame(_) => unreachable!("frames are written where they are emitted"),
        }
    }
}

/// Write one report, taking the device lock only for the write itself so the
/// reader thread is never starved by a long transfer.
pub fn write_frame(device: &Mutex<Option<hidapi::HidDevice>>, frame: [u8; 64]) -> bool {
    let _intent = DeviceWriteIntent::new();
    let guard = device.lock().expect("device poisoned");
    let Some(handle) = guard.as_ref() else {
        return false;
    };
    match handle.write(&frame) {
        Ok(64) => true,
        Ok(written) => {
            eprintln!("[vendor_hid] short write: {written}/64 bytes");
            false
        }
        Err(error) => {
            eprintln!("[vendor_hid] write failed: {error}");
            false
        }
    }
}

/// Fragment and send a finished deck-view payload. The device applies it only
/// when `rev` is newer than its cached view.
///
/// One lock acquisition for all six chunks. The device's USB receive queue is
/// only 256 bytes, so even this small burst needs spacing between 64-byte
/// reports; otherwise later writes can stall behind its queue.
fn write_fast_deck_payload(
    device: &Mutex<Option<hidapi::HidDevice>>,
    seq: &AtomicU16,
    payload: &[u8],
) -> Result<u16, String> {
    let (report, sent_seq) = fast_deck_report(seq, payload)
        .ok_or_else(|| format!("expected 289 DeckView bytes, got {}", payload.len()))?;
    let _intent = DeviceWriteIntent::new();
    let guard = device.lock().expect("device poisoned");
    let handle = guard.as_ref().ok_or("device disconnected")?;
    handle
        .send_feature_report(&report)
        .map_err(|error| error.to_string())?;
    Ok(sent_seq)
}

fn fast_deck_report(seq: &AtomicU16, payload: &[u8]) -> Option<([u8; 296], u16)> {
    if payload.len() != 289 {
        return None;
    }
    let mut report = [0u8; 296];
    report[0] = FAST_DECK_REPORT_ID;
    report[1..3].copy_from_slice(b"KM");
    report[3] = 1;
    report[4] = CMD_FAST_DECK_VIEW;
    let sent_seq = seq.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
    report[5..7].copy_from_slice(&sent_seq.to_le_bytes());
    report[7..].copy_from_slice(payload);
    Some((report, sent_seq))
}

fn fast_glyph_reports(seq: &AtomicU16, blob: &[u8]) -> Option<Vec<[u8; 296]>> {
    if blob.is_empty() || blob.len() > FAST_GLYPH_CHUNK * FAST_GLYPH_MAX_FRAMES {
        return None;
    }
    let total = blob.len().div_ceil(FAST_GLYPH_CHUNK) as u16;
    let mut reports = Vec::with_capacity(total as usize);
    for (index, chunk) in blob.chunks(FAST_GLYPH_CHUNK).enumerate() {
        let mut report = [0u8; 296];
        report[0] = FAST_DECK_REPORT_ID;
        report[1..3].copy_from_slice(b"KM");
        report[3] = 1;
        report[4] = CMD_FAST_GLYPHS;
        let sent_seq = seq.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        report[5..7].copy_from_slice(&sent_seq.to_le_bytes());
        report[7..9].copy_from_slice(&(index as u16).to_le_bytes());
        report[9..11].copy_from_slice(&total.to_le_bytes());
        report[11..11 + chunk.len()].copy_from_slice(chunk);
        reports.push(report);
    }
    Some(reports)
}

fn wait_for_fast_deck_ack(
    ack_rx: &mpsc::Receiver<(u8, u16, u8)>,
    sent_seq: u16,
) -> Result<(), String> {
    wait_for_ack(
        ack_rx,
        CMD_FAST_DECK_VIEW,
        sent_seq,
        Duration::from_millis(800),
    )
}

fn wait_for_ack(
    ack_rx: &mpsc::Receiver<(u8, u16, u8)>,
    expected_command: u8,
    sent_seq: u16,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("ACK timed out".into());
        }
        match ack_rx.recv_timeout(remaining) {
            Ok((command, ack_seq, status))
                if command & 0x7F == expected_command && ack_seq == sent_seq =>
            {
                eprintln!(
                    "[vendor_hid] command {expected_command} ACK seq={sent_seq} status={status}"
                );
                return if status == 0 {
                    Ok(())
                } else {
                    Err(format!("status {status}"))
                };
            }
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => return Err("ACK timed out".into()),
            Err(RecvTimeoutError::Disconnected) => return Err("ACK channel closed".into()),
        }
    }
}

/// Writes the fragmented deck view and returns the sequence of its last frame,
/// which is the one the firmware's apply confirmation carries.
pub fn write_deck_payload(
    device: &Mutex<Option<hidapi::HidDevice>>,
    seq: &AtomicU16,
    payload: &[u8],
) -> Option<u16> {
    let _intent = DeviceWriteIntent::new();
    let lock_started = Instant::now();
    let guard = device.lock().expect("device poisoned");
    let lock_wait = lock_started.elapsed();
    if lock_wait > Duration::from_millis(75) {
        eprintln!("[vendor_hid] deck device lock waited {lock_wait:?}");
    }
    let Some(handle) = guard.as_ref() else {
        return None;
    };
    const CHUNK: usize = 54; // frame[10..64]
    const FRAME_GAP: Duration = Duration::from_millis(4);
    let total = payload.len().div_ceil(CHUNK);
    let mut last_seq = 0;
    for index in 0..total {
        let mut frame = command_frame(CMD_DECK_VIEW, seq, &[]);
        frame[7] = index as u8;
        frame[8] = total as u8;
        frame[9] = (total >> 8) as u8;
        let start = index * CHUNK;
        let end = (start + CHUNK).min(payload.len());
        frame[10..10 + (end - start)].copy_from_slice(&payload[start..end]);
        last_seq = u16::from_le_bytes([frame[5], frame[6]]);
        let write_started = Instant::now();
        let result = handle.write(&frame);
        let write_time = write_started.elapsed();
        if write_time > Duration::from_millis(75) {
            eprintln!("[vendor_hid] deck frame {index}/{total} write took {write_time:?}");
        }
        match result {
            Ok(64) => {}
            Ok(written) => {
                eprintln!("[vendor_hid] short deck write: {written}/64 bytes");
                return None;
            }
            Err(error) => {
                eprintln!("[vendor_hid] deck write failed: {error}");
                return None;
            }
        }
        if index + 1 < total {
            thread::sleep(FRAME_GAP);
        }
    }
    Some(last_seq)
}

/// Pushes a deck view over the fragmented report-6 path and waits for the
/// firmware to say it applied the last frame. Without that wait the dedupe key
/// below would freeze on a payload the deck never showed, and the only way back
/// would be the device re-asking and the companion guessing.
fn write_confirmed_deck_payload(
    device: &Mutex<Option<hidapi::HidDevice>>,
    seq: &AtomicU16,
    payload: &[u8],
    ack_rx: &mpsc::Receiver<(u8, u16, u8)>,
) -> bool {
    let Some(sent_seq) = write_deck_payload(device, seq, payload) else {
        return false;
    };
    match wait_for_ack(ack_rx, CMD_DECK_VIEW, sent_seq, Duration::from_millis(800)) {
        Ok(()) => true,
        Err(error) => {
            eprintln!("[vendor_hid] Report 6 DeckView seq={sent_seq} unconfirmed ({error})");
            false
        }
    }
}

/// Renders `label` as the first session row of a real deck-view payload,
/// because `examples/glyph_send.rs` has no running app to read a snapshot from.
/// The bytes come from the same encoder the deck worker uses, so the standalone
/// firmware test cannot drift from what ships.
pub fn self_test_payload(label: &str) -> (Vec<u8>, Vec<String>) {
    let runtime = AppRuntime::new();
    let mut snapshot = runtime.read();
    snapshot.sessions = vec![crate::state::AgentSession {
        id: "glyph-self-test".into(),
        label: label.into(),
        status: AgentStatus::Idle,
        unread: false,
        focused: false,
    }];
    let config = runtime.config.lock().expect("config poisoned");
    build_deck_payload(&snapshot, &config, true)
}

/// Serialize the deck view. `unicode` selects how text is *rendered*, not the
/// layout: the field widths and offsets are identical either way, so a device
/// that has never heard of glyphs sees exactly the bytes it saw before.
/// Returns the payload (with a zero revision) and the label strings that were
/// actually placed on it, which is the set of codepoints the font must carry.
fn build_deck_payload(
    snapshot: &AppSnapshot,
    config: &crate::profile::AppConfig,
    unicode: bool,
) -> (Vec<u8>, Vec<String>) {
    let mut payload = Vec::with_capacity(233);
    payload.extend_from_slice(&[0; 4]); // revision is filled only when this snapshot is sent
    let mut labels = Vec::new();

    // Agent status codes (5 bytes)
    for slot in crate::profile::AGENT_SLOTS {
        let status = config
            .assigned_agent(slot)
            .and_then(|agent| {
                snapshot
                    .agents
                    .iter()
                    .find(|agent_state| agent_state.id == agent)
            })
            .map(|agent_state| status_code(agent_state.status))
            .unwrap_or(0);
        payload.push(status);
    }

    // Agent flags: bit0=active, bit1=unread, bit2=acknowledged error.
    for slot in crate::profile::AGENT_SLOTS {
        let flags = config
            .assigned_agent(slot)
            .and_then(|agent| {
                snapshot
                    .agents
                    .iter()
                    .find(|agent_state| agent_state.id == agent)
            })
            .map(|agent_state| {
                let mut f = 0u8;
                if agent_state.active {
                    f |= 0x01;
                }
                if agent_state.unread {
                    f |= 0x02;
                }
                if crate::state::agent_error_acknowledged(snapshot, &agent_state.id) {
                    f |= 0x04;
                }
                f
            })
            .unwrap_or(0);
        payload.push(flags);
    }

    // Session data for K1-K3 (54 bytes: 3 × 18 bytes each)
    let session_count = snapshot.sessions.len().min(3);
    payload.push(session_count as u8);

    // Get active agent's display name for label prefix stripping
    let active_agent_display = snapshot
        .active_agent
        .as_deref()
        .and_then(|agent_id| snapshot.agents.iter().find(|a| a.id == agent_id))
        .map(|a| a.display_name.as_str())
        .unwrap_or("");

    for i in 0..3 {
        if i < snapshot.sessions.len() {
            let session = &snapshot.sessions[i];
            payload.push(status_code(session.status));
            // bit0=unread, bit1=focused. Old firmware only tests bit0, so a
            // focused row simply stays unlit there.
            let mut flags = 0u8;
            if session.unread {
                flags |= 0x01;
            }
            if session.focused {
                flags |= 0x02;
            }
            if snapshot.active_panels.iter().any(|panel| {
                panel.id == session.id && crate::state::panel_error_acknowledged(snapshot, panel)
            }) {
                flags |= 0x04;
            }
            payload.push(flags);
            // Strip agent name prefix from label (e.g. "Qwen fixing bug" -> "fixing bug")
            let label_text = if !active_agent_display.is_empty()
                && session.label.starts_with(active_agent_display)
                && session.label.as_bytes().get(active_agent_display.len()) == Some(&b' ')
            {
                session.label[active_agent_display.len()..].trim_start()
            } else {
                &session.label
            };
            // A label only carries Unicode once the glyph transfer for it has
            // been ACKed; until then this is the same ASCII filter as before.
            labels.push(put_text::<32>(&mut payload, label_text, unicode));
        } else {
            // Empty session slot
            payload.push(0); // status = Offline
            payload.push(0); // flags
            payload.extend_from_slice(&[0u8; 32]); // empty label
        }
    }

    // Function key labels for K4-K10 (7 keys × 7 bytes = 49 bytes). These are
    // 12 px on the device, which the 16 px dynamic font cannot serve, so they
    // stay on the ASCII path and must never cut a codepoint in half.
    for index in 3..10 {
        let text = snapshot
            .function_keys
            .get(index)
            .map(|key| key.label.as_str())
            .unwrap_or("");
        put_text::<7>(&mut payload, text, false);
    }

    // Function key supported flags for K4-K10 (7 bytes)
    for index in 3..10 {
        let supported = snapshot
            .function_keys
            .get(index)
            .map(|key| u8::from(key.supported))
            .unwrap_or(0);
        payload.push(supported);
    }

    // Knob position and reasoning level
    payload.push(snapshot.device.knob_position);
    payload.push(0); // reasoning level (reserved)

    // Attention counts
    payload.push(snapshot.attention_count as u8);
    payload.push(snapshot.attention_urgent as u8);

    // Agent display names per slot (5 × 8 bytes)
    for slot in crate::profile::AGENT_SLOTS {
        let text = config
            .assigned_agent(slot)
            .map(agent_display_name)
            .unwrap_or_default();
        labels.push(put_text::<8>(&mut payload, &text, unicode));
    }

    // Active task label (16 bytes) — from active panel or agent task. It is a
    // 14 px field on the device, so like the function keys it stays ASCII.
    let task_text = snapshot
        .active_agent
        .as_deref()
        .and_then(|active_agent_id| {
            snapshot
                .active_panels
                .iter()
                .find(|panel| panel.agent_id == active_agent_id)
                .map(|panel| panel.label.as_str())
                .or_else(|| {
                    snapshot
                        .agents
                        .iter()
                        .find(|agent| agent.id == active_agent_id)
                        .and_then(|agent| agent.task.as_deref())
                })
        })
        .unwrap_or("");
    put_text::<16>(&mut payload, task_text, false);

    // ChatGPT usage extension (56 bytes). Older firmware ignores the tail;
    // the existing revision gate keeps the whole view atomic.
    let usage = &snapshot.chatgpt_usage;
    payload.push(u8::from(
        snapshot.active_agent.as_deref() == Some("chatgpt"),
    ));
    put_text::<16>(&mut payload, &usage.model, false);
    payload.extend_from_slice(&usage.input_tokens.to_le_bytes());
    payload.extend_from_slice(&usage.output_tokens.to_le_bytes());
    payload.extend_from_slice(&usage.remaining_tokens.unwrap_or(u64::MAX).to_le_bytes());
    payload.push(
        usage
            .remaining_5h_percent
            .or(usage.remaining_percent)
            .unwrap_or(u8::MAX),
    );
    let mut reset_5h_time = [0u8; 5];
    if let Some(value) = usage
        .reset_5h_time
        .as_deref()
        .or(usage.reset_time.as_deref())
        .filter(|value| value.len() == 5)
    {
        reset_5h_time.copy_from_slice(value.as_bytes());
    }
    payload.extend_from_slice(&reset_5h_time);
    let age_seconds = if usage.snapshot_at_ms == 0 {
        u16::MAX
    } else {
        ((crate::state::now_ms().saturating_sub(usage.snapshot_at_ms) / 1000).min(u16::MAX as u128))
            as u16
    };
    payload.extend_from_slice(&age_seconds.to_le_bytes());
    // Append Weekly fields after the original 49-byte layout so older
    // firmware still reads the 5H reset time and age correctly.
    payload.push(usage.remaining_weekly_percent.unwrap_or(u8::MAX));
    let mut reset_weekly_time = [0u8; 6];
    if let Some(value) = usage
        .reset_weekly_time
        .as_deref()
        .filter(|value| value.len() == 6)
    {
        reset_weekly_time.copy_from_slice(value.as_bytes());
    }
    payload.extend_from_slice(&reset_weekly_time);

    (payload, labels)
}

/// Write `text` into the payload as a zero-padded `N`-byte field and return the
/// string actually placed, which is what the device font has to be able to
/// draw. `unicode` keeps every codepoint that fits (cutting at a character
/// boundary, never mid-codepoint); otherwise non-ASCII bytes are dropped, which
/// is what the deck could render before dynamic glyphs existed. Control
/// characters are dropped either way.
fn put_text<const N: usize>(payload: &mut Vec<u8>, text: &str, unicode: bool) -> String {
    let mut fitted = String::with_capacity(N.min(text.len()));
    if unicode {
        for ch in text.chars() {
            if ch.is_ascii_control() {
                continue;
            }
            if fitted.len() + ch.len_utf8() > N {
                break;
            }
            fitted.push(ch);
        }
    } else {
        for byte in text.bytes() {
            if fitted.len() == N {
                break;
            }
            if byte.is_ascii() && !byte.is_ascii_control() {
                fitted.push(byte as char);
            }
        }
    }
    payload.extend_from_slice(fitted.as_bytes());
    payload.resize(payload.len() + N - fitted.len(), 0);
    fitted
}

/// Converts agent_id to display name by capitalizing the first letter.
/// "qwen" -> "Qwen", "chatgpt" -> "ChatGPT", "claude" -> "Claude", etc.
fn agent_display_name(agent_id: &str) -> String {
    match agent_id {
        "chatgpt" => "ChatGPT".into(),
        "qwen" => "Qwen".into(),
        "herdr" => "Herdr".into(),
        "codex" => "Codex".into(),
        "claude" => "Claude".into(),
        "dsh" => "DSH".into(),
        _ => {
            let mut chars = agent_id.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => {
                    let mut name = first.to_uppercase().to_string();
                    name.extend(chars);
                    name
                }
            }
        }
    }
}

fn status_code(status: AgentStatus) -> u8 {
    match status {
        AgentStatus::Offline => 0,
        AgentStatus::Idle => 1,
        AgentStatus::Working => 2,
        AgentStatus::Done => 3,
        AgentStatus::NeedsInput => 4,
        AgentStatus::Error => 5,
        AgentStatus::Unknown => 6,
    }
}

/// Standard request frame. `body` lands at frame[11..] (the key-name slot);
/// commands with dedicated short fields (e.g. deck-view fragments at
/// frame[7..10]) write those before sending.
pub fn command_frame(command: u8, seq: &AtomicU16, body: &[u8]) -> [u8; 64] {
    let mut frame = [0_u8; 64];
    frame[0] = REPORT_ID;
    frame[1] = b'K';
    frame[2] = b'M';
    frame[3] = 1;
    frame[4] = command;
    let value = seq.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
    frame[5] = value as u8;
    frame[6] = (value >> 8) as u8;
    let len = body.len().min(53);
    frame[11..11 + len].copy_from_slice(&body[..len]);
    frame
}

fn set_link(app: &AppHandle, state: &Arc<AppRuntime>, connected: bool) {
    state.fast_deck_supported.store(false, Ordering::SeqCst);
    state.fast_deck_failed.store(false, Ordering::SeqCst);
    if connected {
        *state
            .deck_last_payload
            .lock()
            .expect("deck payload poisoned") = None;
    }
    {
        let mut snapshot = state.snapshot.write().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot write lock poisoned, recovering");
            poisoned.into_inner()
        });
        snapshot.device.mode = if connected {
            "hid_vendor".into()
        } else {
            "mock".into()
        };
        snapshot.device.connected = connected;
        snapshot.device.port = connected.then(|| "Vendor HID".to_string());
        snapshot.updated_at_ms = crate::now_ms();
    }
    emit_state(app, state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AgentSession;

    #[test]
    fn navigation_reports_preserve_a5_k10_encoder_order_and_watermark() {
        let state = Arc::new(AppRuntime::new());
        let (tx, rx) = mpsc::channel();
        *state.device_event_tx.lock().unwrap() = Some(tx);
        let mut boot = Some(42);
        let mut sequence = None;
        for (seq, kind, value) in [(1u16, 1, 4), (2, 1, 14), (3, 2, 255), (4, 3, 0)] {
            let mut report = [0; 64];
            report[1..5].copy_from_slice(&[b'K', b'M', 1, 0x42]);
            report[5..7].copy_from_slice(&seq.to_le_bytes());
            report[7..11].copy_from_slice(&42u32.to_le_bytes());
            report[11..14].copy_from_slice(&[1, kind, value]);
            report[14..18].copy_from_slice(&17u32.to_le_bytes());
            report[18..26].copy_from_slice(&99u64.to_le_bytes());
            handle_report(&mut boot, &mut sequence, &report, &state);
            handle_report(&mut boot, &mut sequence, &report, &state);
        }
        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].key.as_deref(), Some("A5"));
        assert_eq!(events[1].key.as_deref(), Some("K10"));
        assert_eq!(events[2].delta, Some(-1));
        assert_eq!(events[3].key.as_deref(), Some("ENCODER_PRESS"));
        assert!(events.iter().all(|e| e.navigation_stamp == Some((17, 99))));
    }

    #[test]
    fn navigation_link_reset_preserves_selected_companion_and_queues_no_restore() {
        let state = Arc::new(AppRuntime::new());
        state.snapshot.write().unwrap().active_agent = Some("companion".into());
        {
            let mut nav = state.navigation.lock().unwrap();
            nav.view.active = true;
            nav.view.visible = true;
        }
        let (tx, rx) = mpsc::channel();
        *state.device_event_tx.lock().unwrap() = Some(tx);
        {
            let mut link = NAVIGATION_LINK.lock().unwrap();
            link.physical = true;
            link.supported = true;
            link.ack = Some(Instant::now() - Duration::from_secs(2));
            assert!(!navigation_live(&link));
        }
        reset_navigation();
        assert!(rx.try_recv().is_err());
        assert_eq!(state.read().active_agent.as_deref(), Some("companion"));
        let nav = state.navigation.lock().unwrap();
        assert!(nav.view.active && nav.view.visible);
    }

    #[test]
    fn navigation_lease_matches_firmware_v1_layout() {
        let seq = AtomicU16::new(7);
        let mut body = vec![1, 1];
        body.extend_from_slice(&0x12345678u32.to_le_bytes());
        body.extend_from_slice(&0x1122334455667788u64.to_le_bytes());
        let frame = command_frame(30, &seq, &body);
        assert_eq!(frame[4], 30);
        assert_eq!(&frame[11..13], &[1, 1]);
        assert_eq!(&frame[13..17], &0x12345678u32.to_le_bytes());
        assert_eq!(&frame[17..25], &0x1122334455667788u64.to_le_bytes());
    }
    #[test]
    fn navigation_requires_capability_and_recent_ack() {
        let mut link = NavigationLink::default();
        link.ack = Some(Instant::now());
        assert!(!navigation_live(&link));
        link.supported = true;
        assert!(navigation_live(&link));
        link.ack = Some(Instant::now() - Duration::from_secs(2));
        assert!(!navigation_live(&link));
    }

    #[test]
    fn deck_dedupe_ignores_only_clock_age() {
        let payload = (0..289).map(|index| index as u8).collect::<Vec<_>>();
        let mut age_changed = payload.clone();
        age_changed[280] ^= 0x55;
        age_changed[281] ^= 0xaa;
        assert_eq!(deck_dedupe_key(&payload), deck_dedupe_key(&age_changed));

        for index in [0, 15, 279, 282, 288] {
            let mut changed = payload.clone();
            changed[index] ^= 1;
            assert_ne!(deck_dedupe_key(&payload), deck_dedupe_key(&changed));
        }
    }

    #[test]
    fn selected_and_request_frames_share_one_sequence_watermark() {
        let mut last = None;
        assert!(accept_event_sequence(&mut last, 18));
        assert!(accept_event_sequence(&mut last, 19)); // RequestDeckView
        assert!(accept_event_sequence(&mut last, 20)); // next selection
        assert!(!accept_event_sequence(&mut last, 20));
        assert!(!accept_event_sequence(&mut last, 19));
        last = Some(u16::MAX);
        assert!(accept_event_sequence(&mut last, 0));
    }

    #[test]
    fn ble_fast_deck_report_contains_the_unchanged_view() {
        let seq = AtomicU16::new(41);
        let payload = (0..289).map(|n| n as u8).collect::<Vec<_>>();
        let (report, sent_seq) = fast_deck_report(&seq, &payload).unwrap();
        assert_eq!(sent_seq, 42);
        assert_eq!(report[0], FAST_DECK_REPORT_ID);
        assert_eq!(&report[1..5], &[b'K', b'M', 1, CMD_FAST_DECK_VIEW]);
        assert_eq!(&report[5..7], &42u16.to_le_bytes());
        assert_eq!(&report[7..], payload);
        assert!(fast_deck_report(&seq, &payload[..288]).is_none());
    }

    #[test]
    fn ble_fast_glyph_reports_cover_blob_with_padded_tail() {
        let seq = AtomicU16::new(7);
        let blob = (0..571).map(|n| n as u8).collect::<Vec<_>>();
        let reports = fast_glyph_reports(&seq, &blob).unwrap();
        assert_eq!(reports.len(), 3);
        for (index, report) in reports.iter().enumerate() {
            assert_eq!(report[0], FAST_DECK_REPORT_ID);
            assert_eq!(&report[1..5], &[b'K', b'M', 1, CMD_FAST_GLYPHS]);
            assert_eq!(u16::from_le_bytes([report[5], report[6]]), 8 + index as u16);
            assert_eq!(u16::from_le_bytes([report[7], report[8]]), index as u16);
            assert_eq!(u16::from_le_bytes([report[9], report[10]]), 3);
            let start = index * FAST_GLYPH_CHUNK;
            let end = blob.len().min(start + FAST_GLYPH_CHUNK);
            assert_eq!(&report[11..11 + end - start], &blob[start..end]);
            assert!(report[11 + end - start..].iter().all(|byte| *byte == 0));
        }
        assert!(
            fast_glyph_reports(&seq, &vec![0; FAST_GLYPH_CHUNK * FAST_GLYPH_MAX_FRAMES + 1])
                .is_none()
        );
    }

    #[test]
    fn hardware_selection_is_visible_before_the_event_worker_runs() {
        let state = Arc::new(AppRuntime::new());
        let selected = state
            .config
            .lock()
            .unwrap()
            .assigned_agent("A1")
            .unwrap()
            .to_owned();
        state.snapshot.write().unwrap().active_agent = None;
        post_selected_slot(&state, 0);
        assert_eq!(
            state.read().active_agent.as_deref(),
            Some(selected.as_str())
        );
    }

    /// Session `i` starts at 15 (after rev/status/flags/count) and is
    /// status + flags + a 32-byte label.
    const fn session_label_at(i: usize) -> usize {
        15 + 34 * i + 2
    }

    /// The filtering the companion shipped with before dynamic glyphs, written
    /// independently of `put_text` so the ASCII red line compares against the
    /// old behaviour rather than against a copy of the new one.
    fn legacy_field<const N: usize>(text: &str) -> [u8; N] {
        let mut out = [0_u8; N];
        let mut pos = 0;
        for byte in text.bytes() {
            if pos == N {
                break;
            }
            if byte.is_ascii() && !byte.is_ascii_control() {
                out[pos] = byte;
                pos += 1;
            }
        }
        out
    }

    fn fixture(labels: &[&str]) -> (AppSnapshot, crate::profile::AppConfig) {
        let runtime = AppRuntime::new();
        let mut snapshot = runtime.read();
        snapshot.sessions = labels
            .iter()
            .map(|label| AgentSession {
                id: (*label).to_owned(),
                label: (*label).to_owned(),
                status: AgentStatus::Working,
                unread: false,
                focused: false,
            })
            .collect();
        let config = runtime.config.lock().expect("config poisoned").clone();
        (snapshot, config)
    }

    #[test]
    fn ascii_labels_produce_the_same_payload_with_glyphs_offered() {
        let (snapshot, config) =
            fixture(&["fix the parser", "Review Q3 numbers", "Qwen reading logs"]);
        let (ascii, _) = build_deck_payload(&snapshot, &config, false);
        let (unicode, labels) = build_deck_payload(&snapshot, &config, true);
        assert_eq!(ascii.len(), 289, "deck-view layout is frozen");
        assert_eq!(ascii, unicode, "ASCII input must not disturb the wire");
        assert!(
            labels.iter().all(|label| label.is_ascii()),
            "labels: {labels:?}"
        );
    }

    #[test]
    fn acknowledged_errors_keep_wire_status_and_set_a_separate_flag() {
        let (mut snapshot, config) = fixture(&["error-session"]);
        let owner = config.assigned_agent("A1").unwrap().to_owned();
        snapshot.active_panels = vec![crate::state::ActivePanel {
            id: "error-session".into(),
            agent_id: owner.clone(),
            session_id: "native-error".into(),
            slot: 1,
            label: "Error".into(),
            status: AgentStatus::Error,
            cwd: None,
            jobs_working: 0,
            jobs_waiting: 0,
            jobs_error: 1,
            updated_at_ms: 100,
            status_since_ms: 100,
            last_activity_ms: 0,
            first_seen_ms: 1,
            transport: crate::state::PanelTransport::Unknown,
            unread: false,
            unread_since_ms: 0,
        }];
        snapshot.sessions[0].status = AgentStatus::Error;
        snapshot
            .agents
            .iter_mut()
            .find(|a| a.id == owner)
            .unwrap()
            .status = AgentStatus::Error;
        let key = crate::session_activity::session_key(&snapshot.active_panels[0]);
        snapshot.acknowledged_errors.insert(key, 100);
        let (payload, _) = build_deck_payload(&snapshot, &config, false);
        assert_eq!(payload.len(), 289);
        assert_eq!(payload[4], status_code(AgentStatus::Error));
        assert_eq!(payload[9] & 4, 4);
        assert_eq!(
            payload[session_label_at(0) - 2],
            status_code(AgentStatus::Error)
        );
        assert_eq!(payload[session_label_at(0) - 1] & 4, 4);
        snapshot.active_panels[0].status_since_ms = 101;
        let (payload, _) = build_deck_payload(&snapshot, &config, false);
        assert_eq!(payload[9] & 4, 0);
        assert_eq!(payload[session_label_at(0) - 1] & 4, 0);
    }

    #[test]
    fn a_focused_session_sets_flag_bit_2() {
        let (mut snapshot, config) = fixture(&["focused", "unread", "plain"]);
        snapshot.sessions[0].focused = true;
        snapshot.sessions[1].unread = true;
        let (payload, _) = build_deck_payload(&snapshot, &config, false);
        let flags_at = |row: usize| payload[session_label_at(row) - 1];
        assert_eq!(flags_at(0) & 0x02, 0x02);
        assert_eq!(flags_at(1) & 0x02, 0);
        assert_eq!(flags_at(1) & 0x01, 0x01);
        assert_eq!(flags_at(2), 0);
    }

    #[test]
    fn unicode_labels_are_truncated_at_a_character_boundary() {
        let title = "审查并修复适配问题,「好」";
        // Nine CJK characters (27 B) + ',' + 「 = 31 B; 好 would need 34.
        let fits = "审查并修复适配问题,「";
        assert_eq!(fits.len(), 31);
        let (snapshot, config) = fixture(&[title, "plain", ""]);
        let (unicode, labels) = build_deck_payload(&snapshot, &config, true);
        assert_eq!(unicode.len(), 289);
        let mut field = [0_u8; 32];
        field[..fits.len()].copy_from_slice(fits.as_bytes());
        let at = session_label_at(0);
        assert_eq!(&unicode[at..at + 32], &field);
        assert_eq!(&labels[0], fits);
        assert_eq!(&labels[1], "plain");
    }

    #[test]
    fn the_ascii_fallback_matches_the_previous_encoder_byte_for_byte() {
        let (snapshot, config) = fixture(&["审查并修复适配问题,「好」", "ok", "third"]);
        let (ascii, labels) = build_deck_payload(&snapshot, &config, false);
        assert_eq!(ascii.len(), 289);
        for (index, title) in ["审查并修复适配问题,「好」", "ok", "third"]
            .iter()
            .enumerate()
        {
            let at = session_label_at(index);
            assert_eq!(&ascii[at..at + 32], legacy_field::<32>(title));
        }
        // Nothing non-ASCII reached the device, so nothing has to be drawn.
        assert!(labels.iter().all(|label| label.is_ascii()));
        // The rest of the fixed fields kept their old filtering too.
        let model_at = 234;
        assert_eq!(
            &ascii[model_at..model_at + 16],
            legacy_field::<16>(&snapshot.chatgpt_usage.model)
        );
    }
}

/// Dedicated audio writer. The reader demultiplexes its ACKs separately from
/// glyph/deck traffic. Each small write releases the device lock immediately.
fn audio_worker(
    state: Arc<AppRuntime>,
    device: Arc<Mutex<Option<hidapi::HidDevice>>>,
    seq: Arc<AtomicU16>,
    ack: mpsc::Receiver<(u8, u16, u8)>,
) {
    use crate::notification_audio::*;
    let audio = &state.notification_audio;
    let mut previous_boot = None;
    let mut generation = 0;
    let mut cached = [None; 3];
    let mut connected = false;
    loop {
        thread::sleep(Duration::from_millis(100));
        let live = device.lock().unwrap().is_some();
        audio.connected.store(live, Ordering::SeqCst);
        let boot = *state.deck_boot_id.lock().unwrap();
        if !live {
            connected = false;
            generation = 0;
            cached = [None; 3];
            // Drop any preview requested while disconnected so it cannot fire
            // unexpectedly the moment the keyboard comes back.
            *audio.play.lock().unwrap() = None;
            *audio.result.lock().unwrap() = (
                "disconnected".into(),
                "连接键盘后自动同步".into(),
                0,
                cached,
            );
            continue;
        }
        if !connected || boot != previous_boot {
            connected = true;
            previous_boot = boot;
            generation = 0;
            cached = [None; 3];
        }
        let desired = audio.generation.load(Ordering::SeqCst);
        if generation != desired {
            generation = desired;
            *audio.result.lock().unwrap() =
                ("syncing".into(), "检测设备音频协议…".into(), 0, cached);
            if let Err(error) = audio_command(&device, &seq, &ack, CMD_CAPS, &[1]) {
                *audio.result.lock().unwrap() = (
                    "unsupported".into(),
                    format!("设备未确认音频协议 v1，继续使用 SPIFFS 提示音：{error}"),
                    0,
                    cached,
                );
                continue;
            }
            let clips = audio.clips.lock().unwrap().clone();
            let mut failed = false;
            for (slot, clip) in clips.iter().enumerate() {
                if audio.generation.load(Ordering::SeqCst) != desired
                    || *state.deck_boot_id.lock().unwrap() != boot
                {
                    break;
                }
                if cached[slot] == Some(clip.crc) {
                    continue;
                }
                let outcome = upload_audio(&device, &seq, &ack, slot as u8, clip, |progress| {
                    let mut result = audio.result.lock().unwrap();
                    result.0 = "syncing".into();
                    result.1 = format!("正在缓存 {}", clip.name);
                    result.2 = progress;
                    result.3 = cached;
                    audio.generation.load(Ordering::SeqCst) == desired
                        && *state.deck_boot_id.lock().unwrap() == boot
                });
                match outcome {
                    Ok(()) => {
                        cached[slot] = Some(clip.crc);
                    }
                    Err(error) => {
                        *audio.result.lock().unwrap() = (
                            "error".into(),
                            format!("音频缓存失败，保留原设备提示音：{error}"),
                            0,
                            cached,
                        );
                        failed = true;
                        break;
                    }
                }
            }
            if !failed
                && audio.generation.load(Ordering::SeqCst) == desired
                && *state.deck_boot_id.lock().unwrap() == boot
            {
                *audio.result.lock().unwrap() =
                    ("ready".into(), "通知音已缓存到键盘 RAM".into(), 100, cached);
            }
        }
        if let Some(slot) = audio.play.lock().unwrap().take() {
            if let Err(error) = audio_command(&device, &seq, &ack, CMD_PLAY, &[slot]) {
                audio.result.lock().unwrap().1 = format!("设备试听失败：{error}");
            }
        }
    }
}

fn write_audio_frame(
    device: &Mutex<Option<hidapi::HidDevice>>,
    frame: &[u8; 64],
) -> Result<(), String> {
    let _intent = DeviceWriteIntent::new();
    let guard = device.lock().unwrap();
    let handle = guard.as_ref().ok_or("device disconnected")?;
    match handle.write(frame) {
        Ok(64) => Ok(()),
        Ok(n) => Err(format!("short audio write: {n}/64")),
        Err(e) => Err(e.to_string()),
    }
}

fn audio_ack(ack: &mpsc::Receiver<(u8, u16, u8)>, cmd: u8, seq: u16) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match ack.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((c, s, status)) if c & 0x7f == cmd && s == seq => {
                return if status == 0 {
                    Ok(())
                } else {
                    Err(format!("device status {status}"))
                }
            }
            Ok(_) => {}
            Err(_) => return Err("audio ACK timed out".into()),
        }
    }
}

fn audio_command(
    device: &Mutex<Option<hidapi::HidDevice>>,
    seq: &AtomicU16,
    ack: &mpsc::Receiver<(u8, u16, u8)>,
    cmd: u8,
    body: &[u8],
) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..3 {
        let frame = command_frame(cmd, seq, body);
        write_audio_frame(device, &frame)?;
        match audio_ack(ack, cmd, u16::from_le_bytes([frame[5], frame[6]])) {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn upload_audio(
    device: &Mutex<Option<hidapi::HidDevice>>,
    seq: &AtomicU16,
    ack: &mpsc::Receiver<(u8, u16, u8)>,
    slot: u8,
    clip: &crate::notification_audio::Clip,
    mut progress: impl FnMut(u8) -> bool,
) -> Result<(), String> {
    use crate::notification_audio::*;
    let mut begin = vec![slot];
    begin.extend((clip.pcm.len() as u32).to_le_bytes());
    begin.extend(clip.crc.to_le_bytes());
    audio_command(device, seq, ack, CMD_BEGIN, &begin)?;
    for (batch, data) in clip.pcm.chunks(46 * 8).enumerate() {
        if !progress((batch * 46 * 8 * 100 / clip.pcm.len()) as u8) {
            return Err("audio settings or device boot changed".into());
        }
        let mut completed = false;
        let mut error = String::new();
        for _ in 0..3 {
            let mut last_seq = 0;
            let total = data.len().div_ceil(46);
            {
                // Keep the reader out only during this 32 ms batch. It must run
                // again before waiting for the ACK; otherwise read_timeout(50)
                // would acquire the mutex between every audio packet.
                let _batch_intent = DeviceWriteIntent::new();
                for (index, chunk) in data.chunks(46).enumerate() {
                    let offset = batch * 46 * 8 + index * 46;
                    let body = data_body(slot, offset, chunk, index + 1 == total)?;
                    let frame = command_frame(CMD_DATA, seq, &body);
                    last_seq = u16::from_le_bytes([frame[5], frame[6]]);
                    write_audio_frame(device, &frame)?;
                    thread::sleep(Duration::from_millis(4));
                }
            }
            match audio_ack(ack, CMD_DATA, last_seq) {
                Ok(()) => {
                    completed = true;
                    break;
                }
                Err(e) => error = e,
            }
        }
        if !completed {
            return Err(error);
        }
    }
    let mut commit = vec![slot];
    commit.extend(clip.crc.to_le_bytes());
    audio_command(device, seq, ack, CMD_COMMIT, &commit)
}
