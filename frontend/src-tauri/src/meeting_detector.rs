//! Detects active meetings and notifies the user with an action to start recording.
//!
//! Currently supported (macOS only):
//! - Google Meet: an open `meet.google.com/<code>` tab in a running browser
//!   (Chrome, Brave, Edge, Arc, Safari), queried via AppleScript.
//! - Slack huddles: the microphone becoming active while Slack is the
//!   frontmost application (CoreAudio + Launch Services).
//!
//! The detector is gated by the `meeting_detection_enabled` notification
//! preference and stays quiet while a recording is already in progress.

use std::process::Command;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager, Wry};

use crate::notifications::commands::NotificationManagerState;

const POLL_INTERVAL: Duration = Duration::from_secs(3);
const SLACK_BUNDLE_ID: &str = "com.tinyspeck.slackmacgap";

/// Browsers that expose tab URLs through the same AppleScript interface.
const BROWSERS: &[&str] = &[
    "Google Chrome",
    "Brave Browser",
    "Microsoft Edge",
    "Arc",
    "Safari",
];

#[derive(Default)]
struct DetectorState {
    /// Meet code already notified about; resets when the tab disappears
    notified_meet_code: Option<String>,
    /// Whether the mic was in use on the previous poll (edge detection)
    mic_was_active: bool,
    /// Already notified during the current mic-active session
    notified_this_mic_session: bool,
}

/// Spawn the background detection loop. Call once during app setup.
pub fn start(app: AppHandle<Wry>) {
    tauri::async_runtime::spawn(async move {
        log::info!("Meeting detector started (Google Meet + Slack huddles)");

        // Debug hook: fire a fake detection shortly after startup so the
        // notification + start-recording plumbing can be tested in isolation.
        if std::env::var("MEETILY_TEST_MEETING_NOTIFICATION").is_ok() {
            let app_for_test = app.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                notify_meeting(
                    &app_for_test,
                    "Google Meet detected (test)",
                    "This is a test notification. Start recording?",
                );
            });
        }

        let mut state = DetectorState::default();
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            poll_once(&app, &mut state).await;
        }
    });
}

async fn poll_once(app: &AppHandle<Wry>, state: &mut DetectorState) {
    if !detection_enabled(app).await {
        return;
    }

    // Don't nag while already recording; also swallow the mic edge our own
    // recording produces so stopping doesn't immediately re-trigger Slack detection.
    if crate::audio::recording_commands::is_recording().await {
        state.mic_was_active = true;
        return;
    }

    // Google Meet: notify once per open meeting tab
    match find_meet_code().await {
        Some(code) => {
            if state.notified_meet_code.as_deref() != Some(code.as_str()) {
                state.notified_meet_code = Some(code.clone());
                notify_meeting(
                    app,
                    "Google Meet detected",
                    &format!("A Meet tab ({}) is open. Start recording?", code),
                );
            }
        }
        None => state.notified_meet_code = None,
    }

    // Slack huddle: mic just became active while Slack is frontmost
    let mic_active = mic_in_use();
    if mic_active && !state.mic_was_active && !state.notified_this_mic_session {
        if frontmost_bundle_id().as_deref() == Some(SLACK_BUNDLE_ID) {
            state.notified_this_mic_session = true;
            notify_meeting(
                app,
                "Slack huddle detected",
                "Looks like you joined a Slack huddle. Start recording?",
            );
        }
    }
    if !mic_active {
        state.notified_this_mic_session = false;
    }
    state.mic_was_active = mic_active;
}

async fn detection_enabled(app: &AppHandle<Wry>) -> bool {
    let Some(manager_state) = app.try_state::<NotificationManagerState<Wry>>() else {
        return false;
    };
    let lock = manager_state.read().await;
    match lock.as_ref() {
        Some(manager) => {
            manager
                .get_settings()
                .await
                .notification_preferences
                .meeting_detection_enabled
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Google Meet detection (browser tabs via AppleScript)
// ---------------------------------------------------------------------------

async fn find_meet_code() -> Option<String> {
    for browser in BROWSERS {
        // Never `tell` a browser that isn't running: AppleScript would launch it.
        if !app_running(browser) {
            continue;
        }
        if let Some(code) = query_browser_tabs(browser).await {
            return Some(code);
        }
    }
    None
}

fn app_running(process_name: &str) -> bool {
    Command::new("pgrep")
        .args(["-x", process_name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn query_browser_tabs(browser: &str) -> Option<String> {
    let script = format!(
        r#"set urlList to {{}}
tell application "{browser}"
    repeat with w in windows
        repeat with t in tabs of w
            set end of urlList to URL of t
        end repeat
    end repeat
end tell
set AppleScript's text item delimiters to linefeed
return urlList as text"#
    );

    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new("osascript")
            .args(["-e", &script])
            // Without this, a timed-out osascript (e.g. waiting on the
            // Automation permission dialog) stays alive and piles up.
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;

    if !output.status.success() {
        // Most likely the Automation permission was denied for this browser
        log::debug!(
            "Meeting detector: could not read {} tabs: {}",
            browser,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return None;
    }

    extract_meet_code(&String::from_utf8_lossy(&output.stdout))
}

/// Extract a Meet meeting code (`xxx-xxxx-xxx`) from a block of URLs.
fn extract_meet_code(text: &str) -> Option<String> {
    for chunk in text.split(|c: char| c.is_whitespace() || c == ',') {
        let Some(idx) = chunk.find("meet.google.com/") else {
            continue;
        };
        let rest = &chunk[idx + "meet.google.com/".len()..];
        let code: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        let parts: Vec<&str> = code.split('-').collect();
        if parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric()))
        {
            return Some(code);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Slack huddle detection (mic in use + frontmost app)
// ---------------------------------------------------------------------------

#[repr(C)]
struct AudioObjectPropertyAddress {
    m_selector: u32,
    m_scope: u32,
    m_element: u32,
}

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyData(
        in_object_id: u32,
        in_address: *const AudioObjectPropertyAddress,
        in_qualifier_data_size: u32,
        in_qualifier_data: *const std::ffi::c_void,
        io_data_size: *mut u32,
        out_data: *mut std::ffi::c_void,
    ) -> i32;
}

const K_AUDIO_OBJECT_SYSTEM_OBJECT: u32 = 1;
const K_DEFAULT_INPUT_DEVICE: u32 = u32::from_be_bytes(*b"dIn ");
const K_DEVICE_IS_RUNNING_SOMEWHERE: u32 = u32::from_be_bytes(*b"gone");
const K_SCOPE_GLOBAL: u32 = u32::from_be_bytes(*b"glob");

/// Whether the default input device is being used by any process.
fn mic_in_use() -> bool {
    unsafe {
        let addr = AudioObjectPropertyAddress {
            m_selector: K_DEFAULT_INPUT_DEVICE,
            m_scope: K_SCOPE_GLOBAL,
            m_element: 0,
        };
        let mut device_id: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let status = AudioObjectGetPropertyData(
            K_AUDIO_OBJECT_SYSTEM_OBJECT,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut device_id as *mut u32 as *mut _,
        );
        if status != 0 || device_id == 0 {
            return false;
        }

        let addr = AudioObjectPropertyAddress {
            m_selector: K_DEVICE_IS_RUNNING_SOMEWHERE,
            m_scope: K_SCOPE_GLOBAL,
            m_element: 0,
        };
        let mut running: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let status = AudioObjectGetPropertyData(
            device_id,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut running as *mut u32 as *mut _,
        );
        status == 0 && running != 0
    }
}

/// Bundle id of the frontmost application via Launch Services (no TCC prompt).
fn frontmost_bundle_id() -> Option<String> {
    let front = Command::new("lsappinfo").arg("front").output().ok()?;
    let asn = String::from_utf8_lossy(&front.stdout).trim().to_string();
    if asn.is_empty() {
        return None;
    }
    let info = Command::new("lsappinfo")
        .args(["info", "-only", "bundleid", &asn])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&info.stdout);
    // Output looks like: "CFBundleIdentifier"="com.tinyspeck.slackmacgap"
    text.split('=')
        .last()
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Notification with "Start recording" action
// ---------------------------------------------------------------------------

fn notify_meeting(app: &AppHandle<Wry>, title: &str, body: &str) {
    log::info!("Meeting detector: {} — {}", title, body);

    // System notification through the same plugin path the rest of the app
    // uses. (mac-notification-sys with an action button was tried here and
    // crashed the app inside deprecated NSUserNotification XPC code.)
    use tauri_plugin_notification::NotificationExt;
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        log::warn!("Meeting detector: failed to show system notification: {}", e);
    }

    // In-app toast with a "Start recording" action, handled by the frontend
    if let Err(e) = app.emit(
        "meeting-detected",
        serde_json::json!({ "title": title, "body": body }),
    ) {
        log::error!("Meeting detector: failed to emit meeting-detected: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::extract_meet_code;

    #[test]
    fn extracts_meet_code_from_urls() {
        let text = "https://mail.google.com/mail/u/0\nhttps://meet.google.com/abc-defg-hij?authuser=0\n";
        assert_eq!(extract_meet_code(text), Some("abc-defg-hij".to_string()));
    }

    #[test]
    fn ignores_non_meeting_meet_urls() {
        assert_eq!(extract_meet_code("https://meet.google.com/landing"), None);
        assert_eq!(extract_meet_code("https://meet.google.com/"), None);
        assert_eq!(extract_meet_code("https://example.com/"), None);
    }
}
