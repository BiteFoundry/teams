//! Desktop notifications via `notify-rust` on a background thread.

use std::sync::atomic::{AtomicUsize, Ordering};

pub const MAX_BODY_CHARS: usize = 120;
const MAX_IN_FLIGHT_NOTIFICATIONS: usize = 2;
static IN_FLIGHT_NOTIFICATIONS: AtomicUsize = AtomicUsize::new(0);

#[cfg(any(target_os = "linux", test))]
fn has_non_empty_env(val: Option<&str>) -> bool {
    val.map(|s| !s.trim().is_empty()).unwrap_or(false)
}

#[cfg(any(target_os = "linux", test))]
fn linux_desktop_notifications_available_with(
    dbus_addr: Option<&str>,
    display: Option<&str>,
    wayland_display: Option<&str>,
) -> bool {
    has_non_empty_env(dbus_addr)
        && (has_non_empty_env(display) || has_non_empty_env(wayland_display))
}

fn desktop_notifications_supported() -> bool {
    #[cfg(target_os = "linux")]
    {
        return linux_desktop_notifications_available_with(
            std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().as_deref(),
            std::env::var("DISPLAY").ok().as_deref(),
            std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
        );
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

struct InFlightGuard {
    counter: &'static AtomicUsize,
}

impl InFlightGuard {
    fn try_acquire(counter: &'static AtomicUsize, limit: usize) -> Option<Self> {
        let mut current = counter.load(Ordering::Relaxed);
        loop {
            if current >= limit {
                return None;
            }
            match counter.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(Self { counter }),
                Err(actual) => current = actual,
            }
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Returns whether a notification should be emitted for an incoming message.
pub fn should_notify(notify_enabled: bool, is_active_conv: bool, is_self: bool) -> bool {
    notify_enabled && !is_active_conv && !is_self
}

/// Truncates text by character count and collapses newlines to spaces.
pub fn truncate(text: &str, max_chars: usize) -> String {
    let single_line = text.replace(['\n', '\r'], " ");
    let mut chars = single_line.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

pub fn chat_body(sender: &str, preview: &str) -> String {
    truncate(&format!("{sender}: {preview}"), MAX_BODY_CHARS)
}

/// Fires a desktop notification on a detached thread; errors are silently ignored.
/// On headless Linux (missing D-Bus session bus or X11/Wayland display) or when
/// too many notification threads are already in flight, this returns immediately.
pub fn show(summary: &str, body: &str) {
    if !desktop_notifications_supported() {
        return;
    }
    let Some(guard) =
        InFlightGuard::try_acquire(&IN_FLIGHT_NOTIFICATIONS, MAX_IN_FLIGHT_NOTIFICATIONS)
    else {
        return;
    };
    let summary = summary.to_string();
    let body = body.to_string();
    let _ = std::thread::Builder::new()
        .name("teams-notify".to_string())
        .spawn(move || {
            let _guard = guard;
            let _ = notify_rust::Notification::new()
                .appname("teams")
                .summary(&summary)
                .body(&body)
                .timeout(notify_rust::Timeout::Milliseconds(6000))
                .show();
        });
}

#[cfg(test)]
mod tests {
    use super::{
        chat_body, linux_desktop_notifications_available_with, should_notify, truncate,
        InFlightGuard, MAX_BODY_CHARS, MAX_IN_FLIGHT_NOTIFICATIONS,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn notify_skips_active_room_self_and_disabled() {
        assert!(should_notify(true, false, false));
        assert!(!should_notify(false, false, false));
        assert!(!should_notify(true, true, false));
        assert!(!should_notify(true, false, true));
    }

    #[test]
    fn truncate_strips_newlines_and_limits_chars() {
        assert_eq!(truncate("hello\nworld", 100), "hello world");
        assert_eq!(truncate("가나다라마", 3), "가나다…");
        assert_eq!(truncate("abc", 3), "abc");
    }

    #[test]
    fn chat_body_contains_sender_and_fits_limit() {
        let body = chat_body("From", "Very long message text");
        assert!(body.starts_with("From: "));
        assert!(body.chars().count() <= MAX_BODY_CHARS + 1);
    }

    #[test]
    fn headless_linux_notification_gate_requires_dbus_and_display() {
        assert!(!linux_desktop_notifications_available_with(
            None,
            Some(":0"),
            None
        ));
        assert!(!linux_desktop_notifications_available_with(
            Some("  "),
            Some(":0"),
            None
        ));
        assert!(!linux_desktop_notifications_available_with(
            Some("unix:path=/run/user/1000/bus"),
            None,
            None
        ));
        assert!(!linux_desktop_notifications_available_with(
            Some("unix:path=/run/user/1000/bus"),
            Some(""),
            Some("   ")
        ));
        assert!(linux_desktop_notifications_available_with(
            Some("unix:path=/run/user/1000/bus"),
            Some(":0"),
            None
        ));
        assert!(linux_desktop_notifications_available_with(
            Some("unix:path=/run/user/1000/bus"),
            None,
            Some("wayland-0")
        ));
    }

    #[test]
    fn in_flight_notification_guard_caps_concurrency_and_releases_on_drop() {
        static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

        let g1 = InFlightGuard::try_acquire(&TEST_COUNTER, MAX_IN_FLIGHT_NOTIFICATIONS).unwrap();
        let g2 = InFlightGuard::try_acquire(&TEST_COUNTER, MAX_IN_FLIGHT_NOTIFICATIONS).unwrap();
        assert!(InFlightGuard::try_acquire(&TEST_COUNTER, MAX_IN_FLIGHT_NOTIFICATIONS).is_none());
        assert_eq!(TEST_COUNTER.load(Ordering::SeqCst), 2);

        drop(g1);
        assert_eq!(TEST_COUNTER.load(Ordering::SeqCst), 1);

        let g3 = InFlightGuard::try_acquire(&TEST_COUNTER, MAX_IN_FLIGHT_NOTIFICATIONS).unwrap();
        assert_eq!(TEST_COUNTER.load(Ordering::SeqCst), 2);

        drop(g2);
        drop(g3);
        assert_eq!(TEST_COUNTER.load(Ordering::SeqCst), 0);
    }
}
