// Floating meeting-alert overlay logic. Loaded only inside the Tauri
// "meeting-alert" window (see src-tauri/src/meeting_detector.rs).
(function () {
    var AUTO_DISMISS_MS = 30000;

    var params = new URLSearchParams(window.location.search);
    var title = params.get('title');
    var body = params.get('body');
    if (title) document.getElementById('title').textContent = title;
    if (body) document.getElementById('body').textContent = body;

    function act(action) {
        try {
            window.__TAURI__.core.invoke('meeting_alert_action', { action: action });
        } catch (e) {
            console.error('meeting-alert: invoke failed', e);
        }
    }

    document.getElementById('start').addEventListener('click', function () { act('start'); });
    document.getElementById('dismiss').addEventListener('click', function () { act('dismiss'); });

    document.getElementById('bar').style.animationDuration = AUTO_DISMISS_MS + 'ms';
    setTimeout(function () { act('dismiss'); }, AUTO_DISMISS_MS);
})();
