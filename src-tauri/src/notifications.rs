//! P7a: one place decides which attention events are worth a notification.
//!
//! The floating panel and the hidden main window both render the same
//! snapshot, so the "is this a new event?" comparison must happen in Rust
//! before projection: if each WebView diffed snapshots on its own, a poll and
//! a `state_changed` broadcast would replay the same event, and a restart would
//! replay history. The coordinator keeps the live attention set keyed by stable
//! session identity plus the source version, emits a card only when that tuple
//! first appears, and revokes a card when its tuple stops being live.

use std::collections::HashMap;

use serde::Serialize;

use crate::state::{AttentionClass, DesktopSession};

/// Cards retained for the panel. Beyond this the queue reports an overflow
/// count instead of growing the window; the attention list stays the complete
/// entry point, so no session becomes unreachable.
pub const MAX_SESSION_NOTIFICATIONS: usize = 12;

/// One temporary notification. `key` is the stable session identity, never the
/// positional `row_key`, whose `unresolved:<index>` fallback shifts as rows come
/// and go and would re-fire an already reported event.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SessionNotification {
    pub event_id: String,
    pub key: String,
    pub row_key: String,
    pub kind: AttentionClass,
    pub agent_short: String,
    pub source: String,
    pub label: String,
    pub project_name: Option<String>,
    /// Source version behind this event, re-checked by the UI before opening.
    pub event_version: u128,
    pub created_at_ms: u128,
    pub openable: bool,
}

/// Actionable attention counts for the collapsed bar and the list header.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AttentionCounts {
    pub needs_input: usize,
    pub error: usize,
    pub done: usize,
}

#[derive(Debug, Default)]
pub struct NotificationCoordinator {
    /// A restart must not replay old cards, so the first snapshot only records
    /// what is already waiting.
    baselined: bool,
    herdr_connected: bool,
    /// Live attention events: identity -> (kind, version). Entries vanish with
    /// their rows, which is what makes a later occurrence of the same state a
    /// new event instead of a permanently suppressed one. A `None` version is
    /// still tracked, so a row discovered mid-wait stays quiet until the source
    /// records a real edge.
    live: HashMap<String, (AttentionClass, Option<u128>)>,
    cards: Vec<SessionNotification>,
    overflow_events: HashMap<String, (String, AttentionClass, u128)>,
    disconnected_at_ms: Option<u128>,
    system_events: Vec<SessionNotification>,
}

fn kind_slug(kind: AttentionClass) -> &'static str {
    match kind {
        AttentionClass::NeedsInput => "needs_input",
        AttentionClass::Error => "error",
        AttentionClass::Done => "done",
    }
}

/// Needs input first, then error, then done — the same order the queue uses.
fn priority(kind: AttentionClass) -> u8 {
    match kind {
        AttentionClass::NeedsInput => 0,
        AttentionClass::Error => 1,
        AttentionClass::Done => 2,
    }
}

/// Record separator: the identity comes from socket paths and pane ids, which
/// may contain `:` and `/`, so no printable delimiter is collision-free.
const EVENT_SEPARATOR: char = '\u{1e}';

fn event_id(key: &str, kind: AttentionClass, version: u128) -> String {
    format!(
        "{key}{sep}{slug}{sep}{version}",
        sep = EVENT_SEPARATOR,
        slug = kind_slug(kind)
    )
}

/// Version of the state behind a row, or `None` when the source never recorded a
/// real edge. Done reuses the acknowledgement version, so a new completion round
/// is a new event; NeedsInput and Error use the status edge.
///
/// Observation time is deliberately not a fallback (§9.2 rule 4): a row that was
/// already waiting when discovery first saw it has no observed transition to
/// report, and inventing one from `observed_since_ms` would replay history the
/// moment the app starts.
fn event_version(row: &DesktopSession, kind: AttentionClass) -> Option<u128> {
    let raw = match kind {
        AttentionClass::Done => row.unread_since_ms,
        _ => row.event_at_ms,
    };
    (raw != 0).then_some(raw)
}

fn live_entry(row: &DesktopSession) -> Option<(String, (AttentionClass, Option<u128>))> {
    if row.stale || row.status == crate::state::AgentStatus::Offline {
        return None;
    }
    let kind = row.attention?;
    // Rows the panel cannot locate are counted but never notified: the card's
    // only action is "open the session".
    let key = row.identity.clone()?;
    Some((key, (kind, event_version(row, kind))))
}

pub fn matches_event(
    rows: &[DesktopSession],
    key: &str,
    kind: AttentionClass,
    version: u128,
) -> bool {
    rows.iter()
        .any(|row| live_entry(row) == Some((key.to_owned(), (kind, Some(version)))))
}

impl NotificationCoordinator {
    /// Runtime starts with no sessions; baseline before accepting the first hook
    /// so a real startup event is not mistaken for historical discovery.
    pub fn ready() -> Self {
        Self {
            baselined: true,
            ..Self::default()
        }
    }
    /// Fold the projected rows into the event log and return the cards to
    /// display. Repeated calls over an unchanged state are idempotent, so the
    /// panel poll and every `state_changed` broadcast agree.
    pub fn observe(
        &mut self,
        rows: &[DesktopSession],
        herdr_connected: bool,
        now_ms: u128,
    ) -> Vec<SessionNotification> {
        self.observe_with_channel(rows, herdr_connected, now_ms, false)
    }

    /// Choose the channel only at event creation, never on a visibility toggle.
    pub fn observe_with_channel(
        &mut self,
        rows: &[DesktopSession],
        herdr_connected: bool,
        now_ms: u128,
        system_channel: bool,
    ) -> Vec<SessionNotification> {
        let reconnected = herdr_connected && !self.herdr_connected;
        if !herdr_connected && self.herdr_connected {
            self.disconnected_at_ms = Some(now_ms);
        }
        self.herdr_connected = herdr_connected;
        let live: HashMap<String, (AttentionClass, Option<u128>)> =
            rows.iter().filter_map(live_entry).collect();
        if !self.baselined {
            self.live = live;
            self.baselined = true;
            self.cards.clear();
            self.overflow_events.clear();
            return Vec::new();
        }
        // Fresh events are the live tuples that changed since the last fold;
        // comparing first and only then replacing `self.live` is what keeps a
        // same-status heartbeat from inventing a round. The `Some(version)`
        // pattern is rule 1's other half: a row with no real edge stays quiet
        // even though it is tracked in `live`.
        let mut fresh = Vec::new();
        for row in rows {
            let Some((key, (kind, Some(version)))) = live_entry(row) else {
                continue;
            };
            if self.live.get(&key) == Some(&(kind, Some(version))) {
                continue;
            }
            // Only suppress old Herdr events on reconnect. A desktop event or
            // real hook received during the outage must not be discarded.
            if reconnected
                && row.source == "Herdr"
                && self.disconnected_at_ms.is_some_and(|at| version <= at)
            {
                continue;
            }
            fresh.push(SessionNotification {
                event_id: event_id(&key, kind, version),
                openable: !row.stale,
                key,
                row_key: row.row_key.clone(),
                kind,
                agent_short: row.agent_short.clone(),
                source: row.source.clone(),
                label: row.label.clone(),
                project_name: row.project_name.clone(),
                event_version: version,
                created_at_ms: now_ms,
            });
        }
        self.live = live;
        self.cards.retain(|card| {
            self.live.get(&card.key).is_some_and(|(kind, version)| {
                *kind == card.kind && *version == Some(card.event_version)
            })
        });
        self.overflow_events
            .retain(|_, (key, kind, version)| self.live.get(key) == Some(&(*kind, Some(*version))));
        fresh.sort_by(|a, b| {
            priority(a.kind)
                .cmp(&priority(b.kind))
                .then(a.key.cmp(&b.key))
        });
        if system_channel {
            self.system_events.extend(fresh);
        } else {
            self.cards.extend(fresh);
        }
        self.cards.sort_by(|a, b| {
            priority(a.kind)
                .cmp(&priority(b.kind))
                .then(a.created_at_ms.cmp(&b.created_at_ms))
                .then_with(|| a.key.cmp(&b.key))
        });
        while self.cards.len() > MAX_SESSION_NOTIFICATIONS {
            if let Some(card) = self.cards.pop() {
                self.overflow_events
                    .insert(card.event_id, (card.key, card.kind, card.event_version));
            }
        }
        self.cards.clone()
    }

    /// Drop one card from the display. This is a visual acknowledgement only:
    /// the event stays in the live set, so the same round is not re-reported,
    /// and it leaves `unread`, a wait or an error untouched.
    pub fn dismiss(&mut self, event_id: &str) -> bool {
        let before = self.cards.len();
        self.cards.retain(|card| card.event_id != event_id);
        self.cards.len() != before
    }

    pub fn take_system_events(&mut self) -> Vec<SessionNotification> {
        std::mem::take(&mut self.system_events)
    }

    pub fn overflow(&self) -> usize {
        self.overflow_events.len()
    }
}

/// Count the same set the cards use: attention rows the panel can locate. The
/// collapsed bar and the 需要关注 tab must agree, and offline, stale and
/// unlocatable rows are not something a notification can act on (§9.1).
pub fn attention_counts(rows: &[DesktopSession]) -> AttentionCounts {
    let mut counts = AttentionCounts::default();
    for row in rows {
        let Some(kind) = row.attention else {
            continue;
        };
        if live_entry(row).is_none() {
            continue;
        }
        match kind {
            AttentionClass::NeedsInput => counts.needs_input += 1,
            AttentionClass::Error => counts.error += 1,
            AttentionClass::Done => counts.done += 1,
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AgentStatus, DesktopSession};

    fn row(identity: &str, kind: AttentionClass, version: u128) -> DesktopSession {
        DesktopSession {
            error_acknowledged: false,
            activity: Default::default(),
            row_key: identity.into(),
            identity: Some(identity.into()),
            agent_id: "qoder".into(),
            display_name: "Qoder".into(),
            agent_short: "Qoder".into(),
            source: "Herdr".into(),
            label: "修复跳转竞争".into(),
            status: match kind {
                AttentionClass::NeedsInput => AgentStatus::NeedsInput,
                AttentionClass::Error => AgentStatus::Error,
                AttentionClass::Done => AgentStatus::Done,
            },
            unread: kind == AttentionClass::Done,
            attention: Some(kind),
            event_at_ms: if kind == AttentionClass::Done {
                0
            } else {
                version
            },
            observed_since_ms: 500,
            event_is_activity: false,
            unread_since_ms: if kind == AttentionClass::Done {
                version
            } else {
                0
            },
            project_id: Some("/work/app".into()),
            project_name: Some("app".into()),
            project_hint: None,
            host: "Herdr".into(),
            stale: false,
            updated_at_ms: version,
            recent_activity_ms: version,
        }
    }

    fn unlocated(kind: AttentionClass, version: u128) -> DesktopSession {
        let mut row = row("missing", kind, version);
        row.identity = None;
        row.row_key = format!("unresolved:0:{version}");
        row
    }

    fn keys(cards: &[SessionNotification]) -> Vec<String> {
        cards.iter().map(|card| card.event_id.clone()).collect()
    }

    #[test]
    fn the_first_snapshot_reports_nothing_so_history_is_never_replayed() {
        let mut coordinator = NotificationCoordinator::default();
        let rows = [
            row("herdr:a:p1", AttentionClass::NeedsInput, 9_000),
            row("herdr:a:p2", AttentionClass::Done, 8_000),
        ];
        assert!(coordinator
            .observe(&rows, true, 10_000)
            .iter()
            .next()
            .is_none());
        // A row that appeared while the app was starting is already known, so
        // the next identical snapshot must not report it either.
        assert!(coordinator.observe(&rows, true, 10_001).is_empty());
    }

    #[test]
    fn twenty_snapshots_of_one_event_notify_only_once() {
        let mut coordinator = NotificationCoordinator::default();
        coordinator.observe(&[], true, 1_000);
        let rows = [row("herdr:a:p1", AttentionClass::Error, 7_000)];
        let mut ids = std::collections::BTreeSet::new();
        for tick in 0..20 {
            ids.extend(
                coordinator
                    .observe(&rows, true, 2_000 + tick)
                    .into_iter()
                    .map(|card| card.event_id),
            );
        }
        assert_eq!(ids.len(), 1, "a repeated status heartbeat is one event");
    }

    #[test]
    fn a_new_round_after_recovery_notifies_again() {
        let mut coordinator = NotificationCoordinator::default();
        coordinator.observe(&[], true, 1_000);
        let waiting = [row("herdr:a:p1", AttentionClass::NeedsInput, 2_000)];
        assert_eq!(coordinator.observe(&waiting, true, 2_000).len(), 1);
        // Working clears attention, then a new wait carries a newer edge.
        let mut busy = row("herdr:a:p1", AttentionClass::NeedsInput, 2_000);
        busy.attention = None;
        busy.status = AgentStatus::Working;
        assert!(coordinator.observe(&[busy], true, 3_000).is_empty());
        let again = [row("herdr:a:p1", AttentionClass::NeedsInput, 4_000)];
        let cards = coordinator.observe(&again, true, 4_000);
        assert_eq!(
            keys(&cards),
            vec![event_id("herdr:a:p1", AttentionClass::NeedsInput, 4_000)]
        );
    }

    #[test]
    fn a_row_discovered_mid_wait_is_not_reported_as_an_event() {
        let mut coordinator = NotificationCoordinator::default();
        coordinator.observe(&[], true, 1_000);
        // Discovery gives no status edge, only an observation baseline. Treating
        // that baseline as an event would replay a wait that predates the app.
        let rows = [row("herdr:a:p1", AttentionClass::Error, 0)];
        assert!(
            coordinator.observe(&rows, true, 5_000).is_empty(),
            "an unobserved edge is not a transition"
        );
        assert!(coordinator.observe(&rows, true, 9_000).is_empty());
    }

    #[test]
    fn the_first_real_edge_after_a_quiet_discovery_notifies() {
        let mut coordinator = NotificationCoordinator::default();
        coordinator.observe(&[], true, 1_000);
        let quiet = [row("herdr:a:p1", AttentionClass::Error, 0)];
        coordinator.observe(&quiet, true, 5_000);
        // The pane then transitions for real, so this round is worth a card.
        let edged = [row("herdr:a:p1", AttentionClass::Error, 6_000)];
        let cards = coordinator.observe(&edged, true, 6_000);
        assert_eq!(
            keys(&cards),
            vec![event_id("herdr:a:p1", AttentionClass::Error, 6_000)]
        );
        assert_eq!(
            coordinator.observe(&edged, true, 7_000),
            cards,
            "the edge is reported once, then stays as the live version"
        );
    }

    #[test]
    fn cards_are_revoked_when_the_target_recovers_or_disappears() {
        let mut coordinator = NotificationCoordinator::default();
        coordinator.observe(&[], true, 1_000);
        let rows = [row("herdr:a:p1", AttentionClass::Done, 6_000)];
        coordinator.observe(&rows, true, 6_000);
        assert_eq!(coordinator.cards.len(), 1);
        coordinator.observe(&[], true, 7_000);
        assert!(coordinator.cards.is_empty(), "the pane is gone");

        let rows = [row("herdr:a:p2", AttentionClass::Error, 8_000)];
        coordinator.observe(&rows, true, 8_000);
        let mut recovered = rows[0].clone();
        recovered.attention = None;
        coordinator.observe(&[recovered], true, 9_000);
        assert!(coordinator.cards.is_empty(), "the error cleared");
    }

    #[test]
    fn reconnecting_rebuilds_the_baseline_without_replaying_the_outage() {
        let mut coordinator = NotificationCoordinator::default();
        coordinator.observe(&[], true, 1_000);
        let rows = [row("herdr:a:p1", AttentionClass::NeedsInput, 2_000)];
        // The host drops: its rows stop being actionable, then come back.
        coordinator.observe(&[], false, 3_000);
        let cards = coordinator.observe(&rows, true, 4_000);
        assert!(
            cards.is_empty(),
            "a wait that predates the outage is not a new event"
        );
        coordinator.observe(&[], true, 5_000);
        assert_eq!(coordinator.observe(&rows, true, 6_000).len(), 1);
    }

    #[test]
    fn the_queue_stays_bounded_and_reports_the_overflow() {
        let mut coordinator = NotificationCoordinator::default();
        coordinator.observe(&[], true, 1_000);
        let rows: Vec<DesktopSession> = (0..20)
            .map(|index| {
                row(
                    &format!("herdr:a:p{index}"),
                    AttentionClass::Done,
                    1_000 + index,
                )
            })
            .collect();
        let cards = coordinator.observe(&rows, true, 2_000);
        assert_eq!(cards.len(), MAX_SESSION_NOTIFICATIONS);
        assert_eq!(coordinator.overflow(), 20 - MAX_SESSION_NOTIFICATIONS);
        let counts = attention_counts(&rows);
        assert_eq!(counts.done, 20, "counts stay complete");
    }

    #[test]
    fn needs_input_orders_before_error_before_done() {
        let mut coordinator = NotificationCoordinator::default();
        coordinator.observe(&[], true, 1_000);
        let rows = [
            row("herdr:a:p3", AttentionClass::Done, 3_000),
            row("herdr:a:p2", AttentionClass::Error, 2_000),
            row("herdr:a:p1", AttentionClass::NeedsInput, 1_000),
        ];
        let cards = coordinator.observe(&rows, true, 4_000);
        let kinds: Vec<AttentionClass> = cards.iter().map(|card| card.kind).collect();
        assert_eq!(
            kinds,
            vec![
                AttentionClass::NeedsInput,
                AttentionClass::Error,
                AttentionClass::Done
            ]
        );
    }

    #[test]
    fn dismissing_a_card_only_removes_it_from_the_display() {
        let mut coordinator = NotificationCoordinator::default();
        coordinator.observe(&[], true, 1_000);
        let rows = [row("herdr:a:p1", AttentionClass::Done, 6_000)];
        let cards = coordinator.observe(&rows, true, 6_000);
        assert!(coordinator.dismiss(&cards[0].event_id));
        // The round is still unacknowledged: it stays counted, and re-reading
        // the same state must not bring the card back.
        assert_eq!(attention_counts(&rows).done, 1);
        assert!(coordinator.observe(&rows, true, 7_000).is_empty());
        assert!(!coordinator.dismiss("unknown"));
    }

    #[test]
    fn counts_and_cards_share_the_locatable_set_the_list_uses() {
        let mut coordinator = NotificationCoordinator::default();
        coordinator.observe(&[], true, 1_000);
        let rows = [unlocated(AttentionClass::NeedsInput, 4_000)];
        assert!(coordinator.observe(&rows, true, 5_000).is_empty());
        assert_eq!(
            attention_counts(&rows).needs_input,
            0,
            "a row the panel cannot open is neither a card nor a count"
        );
    }
    #[test]
    fn runtime_first_live_event_is_not_swallowed_by_baselining() {
        let mut coordinator = NotificationCoordinator::ready();
        assert_eq!(
            coordinator
                .observe(&[row("herdr:p", AttentionClass::Error, 10)], true, 10)
                .len(),
            1
        );
    }

    #[test]
    fn herdr_reconnect_keeps_desktop_cards_and_reports_new_desktop_events() {
        let mut coordinator = NotificationCoordinator::ready();
        coordinator.observe(&[], true, 10);
        let mut desktop = row("thread:a", AttentionClass::Done, 20);
        desktop.source = "Desktop".into();
        coordinator.observe(&[desktop.clone()], true, 20);
        coordinator.observe(&[desktop.clone()], false, 30);
        let mut next = row("thread:b", AttentionClass::Error, 40);
        next.source = "Desktop".into();
        assert_eq!(coordinator.observe(&[desktop, next], true, 40).len(), 2);
    }

    #[test]
    fn reconnect_does_not_suppress_a_live_hook_from_during_the_outage() {
        let mut coordinator = NotificationCoordinator::ready();
        coordinator.observe(&[], true, 10);
        coordinator.observe(&[], false, 20);
        assert_eq!(
            coordinator
                .observe(&[row("herdr:p", AttentionClass::Error, 30)], true, 40)
                .len(),
            1
        );
    }

    #[test]
    fn resolved_overflow_does_not_leave_phantom_more_count() {
        let mut coordinator = NotificationCoordinator::ready();
        let rows: Vec<_> = (0..20)
            .map(|n| row(&format!("herdr:{n}"), AttentionClass::Done, 10 + n))
            .collect();
        coordinator.observe(&rows, true, 100);
        assert_eq!(coordinator.overflow(), 8);
        coordinator.observe(&[], true, 200);
        assert_eq!(coordinator.overflow(), 0);
    }
    #[test]
    fn hidden_events_choose_system_once_and_never_replay_as_cards() {
        let mut coordinator = NotificationCoordinator::ready();
        let rows = [row("herdr:p", AttentionClass::NeedsInput, 10)];
        assert!(coordinator
            .observe_with_channel(&rows, true, 10, true)
            .is_empty());
        assert_eq!(coordinator.take_system_events().len(), 1);
        for _ in 0..20 {
            assert!(coordinator
                .observe_with_channel(&rows, true, 11, true)
                .is_empty());
            assert!(coordinator.take_system_events().is_empty());
        }
        assert!(coordinator
            .observe_with_channel(&rows, true, 12, false)
            .is_empty());
        assert!(coordinator.take_system_events().is_empty());
        assert_eq!(attention_counts(&rows).needs_input, 1);
    }

    #[test]
    fn hiding_does_not_resend_an_existing_panel_card() {
        let mut coordinator = NotificationCoordinator::ready();
        let rows = [row("herdr:p", AttentionClass::Done, 10)];
        let first = coordinator.observe_with_channel(&rows, true, 10, false);
        assert_eq!(first.len(), 1);
        assert_eq!(
            coordinator.observe_with_channel(&rows, true, 11, true),
            first
        );
        assert!(coordinator.take_system_events().is_empty());
    }

    #[test]
    fn enabling_notifications_does_not_replay_unavailable_permission_events() {
        let mut coordinator = NotificationCoordinator::ready();
        let rows = [row("herdr:p", AttentionClass::Error, 10)];
        coordinator.observe_with_channel(&rows, true, 10, false);
        coordinator.observe_with_channel(&rows, true, 11, true);
        assert!(coordinator.take_system_events().is_empty());
        let next = [row("herdr:p", AttentionClass::Error, 20)];
        coordinator.observe_with_channel(&next, true, 20, true);
        assert_eq!(coordinator.take_system_events().len(), 1);
    }

    #[test]
    fn merged_system_events_keep_urgent_order_and_full_counts() {
        let mut coordinator = NotificationCoordinator::ready();
        let rows = [
            row("done", AttentionClass::Done, 10),
            row("input", AttentionClass::NeedsInput, 10),
            row("error", AttentionClass::Error, 10),
        ];
        assert!(coordinator
            .observe_with_channel(&rows, true, 10, true)
            .is_empty());
        let events = coordinator.take_system_events();
        assert_eq!(
            events.iter().map(|c| c.kind).collect::<Vec<_>>(),
            vec![
                AttentionClass::NeedsInput,
                AttentionClass::Error,
                AttentionClass::Done
            ]
        );
        let counts = attention_counts(&rows);
        assert_eq!((counts.needs_input, counts.error, counts.done), (1, 1, 1));
    }
}
