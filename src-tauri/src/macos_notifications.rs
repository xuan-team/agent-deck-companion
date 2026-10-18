//! macOS UserNotifications adapter. Never called from an unbundled dev binary:
//! currentNotificationCenter can raise an Objective-C exception without a bundle.
use crate::{
    notifications::{matches_event, SessionNotification},
    state::{now_ms, AppRuntime, AttentionClass},
};
use block2::{DynBlock, RcBlock};
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{Bool, ProtocolObject},
    AnyThread, DefinedClass,
};
use objc2_foundation::{NSArray, NSBundle, NSError, NSObject, NSObjectProtocol, NSString};
use objc2_user_notifications::*;
use std::{
    collections::HashMap,
    sync::{atomic::Ordering, mpsc, Arc},
    thread,
};
use tauri::{AppHandle, Emitter};

pub(super) enum Command {
    Sync,
    Refresh,
    Authorize,
    Test,
    Clicked(String),
    AuthorizationDone(Option<String>),
    DeliveryDone(String, Option<String>),
}

define_class!(
    // NSObject imposes no subclassing requirements. Callbacks only enqueue
    // commands, and the worker retains this weak center delegate for its life.
    #[unsafe(super(NSObject))]
    #[name = "AgentDeckNotificationDelegate"]
    #[ivars = mpsc::Sender<Command>]
    struct NotificationDelegate;
    unsafe impl NSObjectProtocol for NotificationDelegate {}
    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(&self, _center: &UNUserNotificationCenter, _notification: &UNNotification,
            completion: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>) {
            completion.call((UNNotificationPresentationOptions::Banner | UNNotificationPresentationOptions::List,));
        }
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(&self, _center: &UNUserNotificationCenter, response: &UNNotificationResponse,
            completion: &DynBlock<dyn Fn()>) {
            // Dismissal is not an open/acknowledgement.
            if response.actionIdentifier().to_string() == unsafe { UNNotificationDefaultActionIdentifier.to_string() } {
                let id = response.notification().request().identifier().to_string();
                let _ = self.ivars().send(Command::Clicked(id));
            }
            completion.call(());
        }
    }
);

impl NotificationDelegate {
    fn new(tx: mpsc::Sender<Command>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(tx);
        // NSObject's designated init, with no new subclass requirements.
        unsafe { msg_send![super(this), init] }
    }
}

fn report(app: &AppHandle, state: &AppRuntime) {
    let _ = app.emit(
        "desktop_notification_status_changed",
        state.desktop_notifications.view(),
    );
}
fn error_text(error: *mut NSError) -> Option<String> {
    // Apple owns the optional error for the duration of the callback; copy text.
    unsafe { error.as_ref().map(|e| e.localizedDescription().to_string()) }
}
fn refresh(center: &UNUserNotificationCenter, app: &AppHandle, state: &Arc<AppRuntime>) {
    let app = app.clone();
    let state = state.clone();
    let handler = RcBlock::new(move |settings: std::ptr::NonNull<UNNotificationSettings>| {
        let status = unsafe { settings.as_ref() }.authorizationStatus();
        let permission = match status {
            UNAuthorizationStatus::Authorized | UNAuthorizationStatus::Provisional => "granted",
            UNAuthorizationStatus::Denied => "denied",
            _ => "not_requested",
        };
        state
            .desktop_notifications
            .view
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .permission = permission.into();
        report(&app, &state);
    });
    center.getNotificationSettingsWithCompletionHandler(&handler);
}
fn remove(center: &UNUserNotificationCenter, ids: &[String]) {
    let ids: Vec<_> = ids.iter().map(|id| NSString::from_str(id)).collect();
    let ids = NSArray::from_retained_slice(&ids);
    center.removePendingNotificationRequestsWithIdentifiers(&ids);
    center.removeDeliveredNotificationsWithIdentifiers(&ids);
}
fn rows(state: &AppRuntime) -> Vec<crate::state::DesktopSession> {
    state.desktop_rows(&state.snapshot.read().unwrap_or_else(|p| p.into_inner()))
}
fn live(card: &SessionNotification, rows: &[crate::state::DesktopSession]) -> bool {
    matches_event(rows, &card.key, card.kind, card.event_version)
}
fn send(
    center: &UNUserNotificationCenter,
    tx: &mpsc::Sender<Command>,
    id: &str,
    title: &str,
    subtitle: &str,
    body: &str,
) {
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setSubtitle(&NSString::from_str(subtitle));
    content.setBody(&NSString::from_str(body));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(id),
        &content,
        None,
    );
    let tx = tx.clone();
    let id = id.to_owned();
    let handler = RcBlock::new(move |error: *mut NSError| {
        let _ = tx.send(Command::DeliveryDone(id.clone(), error_text(error)));
    });
    center.addNotificationRequest_withCompletionHandler(&request, Some(&handler));
}

pub(super) fn start(app: AppHandle, state: Arc<AppRuntime>) {
    let bundled = std::env::current_exe()
        .ok()
        .is_some_and(|p| p.parent().is_some_and(|p| p.ends_with("Contents/MacOS")));
    if !bundled || NSBundle::mainBundle().bundleIdentifier().is_none() {
        return;
    }
    let (tx, rx) = mpsc::channel();
    *state
        .desktop_notifications
        .tx
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(tx.clone());
    thread::spawn(move || {
        let center =
            match objc2::exception::catch(UNUserNotificationCenter::currentNotificationCenter) {
                Ok(center) => center,
                Err(error) => {
                    state
                        .desktop_notifications
                        .view
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .last_error = Some(format!("Notification center unavailable: {error:?}"));
                    report(&app, &state);
                    return;
                }
            };
        let delegate = NotificationDelegate::new(tx.clone());
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        // A process restart never leaves old notifications pointing at a new
        // in-memory event map. This removes only this app's own notifications.
        center.removeAllPendingNotificationRequests();
        center.removeAllDeliveredNotifications();
        state
            .desktop_notifications
            .view
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .available = true;
        refresh(&center, &app, &state);
        let mut delivered: HashMap<String, Vec<SessionNotification>> = HashMap::new();
        let mut authorizing = false;
        while let Ok(command) = rx.recv() {
            objc2::rc::autoreleasepool(|_| match command {
                Command::Refresh => refresh(&center, &app, &state),
                Command::Authorize => {
                    if authorizing {
                        return;
                    }
                    authorizing = true;
                    let tx = tx.clone();
                    let handler = RcBlock::new(move |_granted: Bool, error: *mut NSError| {
                        let _ = tx.send(Command::AuthorizationDone(error_text(error)));
                    });
                    center.requestAuthorizationWithOptions_completionHandler(
                        UNAuthorizationOptions::Alert,
                        &handler,
                    );
                }
                Command::AuthorizationDone(error) => {
                    authorizing = false;
                    state
                        .desktop_notifications
                        .view
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .last_error = error;
                    refresh(&center, &app, &state);
                }
                Command::Test => {
                    if state.desktop_notifications.view().permission == "granted" {
                        send(
                            &center,
                            &tx,
                            "agent-deck-test",
                            "Agent Deck",
                            "通知测试 / Notification test",
                            "点击查看需要关注 / Click to view sessions",
                        );
                    }
                }
                Command::Sync => {
                    let pending = {
                        let mut inbox = state
                            .desktop_notifications
                            .inbox
                            .lock()
                            .unwrap_or_else(|p| p.into_inner());
                        state
                            .desktop_notifications
                            .sync_queued
                            .store(false, Ordering::SeqCst);
                        std::mem::take(&mut inbox.pending)
                    };
                    let current = rows(&state);
                    let enabled = state.desktop_notifications.enabled.load(Ordering::SeqCst);
                    let expired: Vec<_> = delivered
                        .iter()
                        .filter(|(_, cards)| !enabled || cards.iter().all(|c| !live(c, &current)))
                        .map(|(id, _)| id.clone())
                        .collect();
                    remove(&center, &expired);
                    for id in expired {
                        delivered.remove(&id);
                    }
                    // Channel was fixed at ingress; re-showing the panel does
                    // not turn this batch into cards. Re-check permission and
                    // target before OS submission, without retrying old events.
                    if !enabled || state.desktop_notifications.view().permission != "granted" {
                        return;
                    }
                    let pending: Vec<_> =
                        pending.into_iter().filter(|c| live(c, &current)).collect();
                    if pending.is_empty() {
                        return;
                    }
                    if delivered.len() >= 32 {
                        if let Some(oldest) = delivered
                            .iter()
                            .min_by_key(|(_, cards)| {
                                cards.first().map(|c| c.created_at_ms).unwrap_or(0)
                            })
                            .map(|(id, _)| id.clone())
                        {
                            remove(&center, &[oldest.clone()]);
                            delivered.remove(&oldest);
                        }
                    }
                    let mut pending = pending;
                    pending.sort_by_key(|c| match c.kind {
                        AttentionClass::NeedsInput => 0,
                        AttentionClass::Error => 1,
                        AttentionClass::Done => 2,
                    });
                    let lead = &pending[0];
                    let kind = match lead.kind {
                        AttentionClass::NeedsInput => "需要输入 / Needs input",
                        AttentionClass::Error => "发生错误 / Error",
                        AttentionClass::Done => "任务完成 / Done",
                    };
                    let title = format!("{} · {}", lead.agent_short, kind);
                    let subtitle = lead.project_name.as_deref().unwrap_or("Agent Deck");
                    let body = if pending.len() == 1 {
                        lead.label.clone()
                    } else {
                        format!(
                            "{} · 另有 {} 个新提醒，点击查看需要关注",
                            lead.label,
                            pending.len() - 1
                        )
                    };
                    let id = uuid::Uuid::new_v4().to_string();
                    send(&center, &tx, &id, &title, subtitle, &body);
                    delivered.insert(id, pending);
                }
                Command::DeliveryDone(id, error) => {
                    if error.is_some() {
                        delivered.remove(&id);
                    }
                    if id != "agent-deck-test" && !delivered.contains_key(&id) {
                        remove(&center, &[id.clone()]);
                    }
                    let mut view = state
                        .desktop_notifications
                        .view
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    if error.is_none() {
                        view.last_submission_ms = Some(now_ms());
                    }
                    view.last_error = error;
                    drop(view);
                    // A request can complete after its target was revoked.
                    // Re-run cleanup so a late delivery cannot leave a stale OS card.
                    state.desktop_notifications.sync(Vec::new());
                    report(&app, &state);
                }
                Command::Clicked(id) => {
                    let is_test = id == "agent-deck-test";
                    let cards = delivered.remove(&id);
                    remove(&center, &[id]);
                    let outcome = if let Some(cards) = cards {
                        if cards.len() == 1 {
                            let card = &cards[0];
                            let navigation_revision = crate::session_navigation::revision(&state);
                            let generation = crate::activation::reserve_focus_request(&state);
                            let outcome = crate::activation::open_notification_request(
                                &state,
                                &card.key,
                                generation,
                                (card.kind, card.event_version),
                            );
                            if outcome == crate::activation::OpenOutcome::Opened {
                                crate::session_navigation::finish_open(
                                    &app,
                                    &state,
                                    navigation_revision,
                                );
                            }
                            let text = serde_json::to_value(outcome)
                                .ok()
                                .and_then(|v| v.as_str().map(str::to_owned))
                                .unwrap_or_else(|| "error".into());
                            if !matches!(text.as_str(), "opened" | "switched_not_raised") {
                                crate::session_window::reveal_attention(&app);
                            }
                            text
                        } else {
                            crate::session_window::reveal_attention(&app);
                            "attention_list".into()
                        }
                    } else {
                        crate::session_window::reveal_attention(&app);
                        if is_test {
                            "attention_list".into()
                        } else {
                            "expired".into()
                        }
                    };
                    state
                        .desktop_notifications
                        .view
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .last_click = Some(outcome.clone());
                    let _ = app.emit("desktop_notification_click", &outcome);
                    report(&app, &state);
                    crate::state::emit_state(&app, &state);
                }
            });
        }
    });
}
