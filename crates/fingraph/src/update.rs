//! Once-a-day check for a newer GitHub release; shows a toast with a link to it (same as Kestrel's).
//! ponytail: reads the tag from the /releases/latest redirect via curl, so no HTTP crate or JSON.

use gtk::{gio, glib};
use std::rc::Rc;

use crate::App;

const LATEST: &str = "https://github.com/Stolly345/fingraph/releases/latest";
const DAY: i64 = 24 * 60 * 60;
const KEY: &str = "last_update_check";

/// "v0.2.16" → [0, 2, 16]; None for anything that isn't dotted numbers.
fn parse(v: &str) -> Option<Vec<u32>> {
    v.trim().trim_start_matches('v').split('.').map(|p| p.parse().ok()).collect()
}

fn newer(tag: &str, current: &str) -> bool {
    matches!((parse(tag), parse(current)), (Some(t), Some(c)) if t > c)
}

pub fn check(app: &Rc<App>) {
    let now = glib::real_time() / 1_000_000;
    let last: i64 = app.db.setting(KEY).and_then(|v| v.parse().ok()).unwrap_or(0);
    if !app.db.prefs().check_updates || now.saturating_sub(last) < DAY {
        return;
    }
    // stamped before the request, so a failed check (offline) waits a day too
    let _ = app.db.set_setting(KEY, &now.to_string());
    let Ok(p) = gio::Subprocess::newv(
        &["curl", "-s", "--max-time", "10", "-o", "/dev/null", "-w", "%{redirect_url}", LATEST].map(std::ffi::OsStr::new),
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE,
    ) else {
        return; // no curl: stay quiet
    };
    let toasts = app.toasts.clone();
    glib::spawn_future_local(async move {
        let Ok((Some(out), _)) = p.communicate_utf8_future(None).await else { return };
        let url = out.trim().to_string();
        let Some(tag) = url.rsplit_once("/tag/").map(|(_, t)| t.to_string()) else { return };
        if !newer(&tag, env!("CARGO_PKG_VERSION")) {
            return;
        }
        let t = adw::Toast::builder()
            .title(format!("FinGraph {} is available", tag.trim_start_matches('v')))
            .button_label("View Release")
            .timeout(0)
            .build();
        t.connect_button_clicked(move |_| gtk::UriLauncher::new(&url).launch(None::<&gtk::Window>, None::<&gio::Cancellable>, |_| {}));
        toasts.add_toast(t);
    });
}

#[cfg(test)]
mod tests {
    use super::newer;
    #[test]
    fn compares_numerically() {
        assert!(newer("v0.2.17", "0.2.16"));
        assert!(newer("v0.10.0", "0.2.16"));
        assert!(newer("v1.0.0", "0.2.16"));
        assert!(!newer("v0.2.16", "0.2.16"));
        assert!(!newer("v0.2.9", "0.2.16"));
        assert!(!newer("vnightly", "0.2.16"));
    }
}
