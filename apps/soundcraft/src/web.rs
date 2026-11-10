//! A web page in a tab: an embedded browser (WebView2 through `wry` on Windows) that sits over
//! the part of the window the page gets. Other platforms show a note instead.

use std::sync::{Arc, Mutex, PoisonError};

/// Where a search goes when the address bar holds words rather than an address.
const SEARCH: &str = "https://duckduckgo.com/?q=";

/// The address to load for what was typed in the address bar: an address with a scheme as it is;
/// something that looks like a domain (or `localhost`, an IP address) with `https://`/`http://`
/// in front; anything else is a web search.
pub fn normalize_url(input: &str) -> String {
    let text = input.trim();
    if text.is_empty() {
        return "about:blank".into();
    }
    let lower = text.to_ascii_lowercase();
    let has_scheme = lower.find("://").is_some_and(|i| i > 0 && lower[..i].chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)))
        || ["about:", "data:", "file:", "mailto:", "javascript:"].iter().any(|s| lower.starts_with(s));
    if has_scheme {
        return text.to_string();
    }
    if !text.chars().any(char::is_whitespace) {
        let end = text.find(['/', '?', '#']).unwrap_or(text.len());
        let host_port = &text[..end];
        let host = host_port.rsplit_once(':').filter(|(_, p)| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())).map_or(host_port, |(h, _)| h);
        let ipv4 = host.split('.').count() == 4 && host.split('.').all(|p| !p.is_empty() && p.len() <= 3 && p.chars().all(|c| c.is_ascii_digit()));
        if host.eq_ignore_ascii_case("localhost") || ipv4 {
            return format!("http://{text}");
        }
        let labels: Vec<&str> = host.split('.').collect();
        let domain = labels.len() >= 2
            && labels.iter().all(|l| !l.is_empty() && l.chars().all(|c| c.is_alphanumeric() || c == '-'))
            && labels.last().is_some_and(|tld| tld.len() >= 2 && tld.chars().all(char::is_alphabetic));
        if domain {
            return format!("https://{text}");
        }
    }
    format!("{SEARCH}{}", query_encode(text))
}

fn query_encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(b)),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// What the page tells the app, from the browser's callbacks.
#[derive(Default)]
pub struct PageState {
    pub title: String,
    pub url: String,
    /// Addresses the page wants in new tabs (`window.open`, `target=_blank`).
    pub new_tabs: Vec<String>,
    /// Tab shortcuts pressed while the page had the keyboard: ("t" | "w" | "tab", shift).
    pub keys: Vec<(String, bool)>,
}

pub type Page = Arc<Mutex<PageState>>;

pub fn lock(page: &Page) -> std::sync::MutexGuard<'_, PageState> {
    page.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Forwards the tab shortcuts to the app, which never sees keys the page has.
#[cfg(windows)]
const KEY_SCRIPT: &str = "window.addEventListener('keydown', function (e) { if ((e.ctrlKey || e.metaKey) && !e.altKey) { var k = String(e.key).toLowerCase(); if (k === 't' || k === 'w' || k === 'tab') { e.preventDefault(); window.ipc.postMessage(JSON.stringify({ k: k, s: e.shiftKey })); } } }, true);";

pub struct WebTab {
    pub page: Page,
    /// The text in the address bar.
    pub address: String,
    /// The address shown last, to follow the page when the bar is not being edited.
    seen_url: String,
    /// Where the page starts, until the browser exists.
    start: Option<String>,
    /// Focus the address bar on the next frame (a new tab).
    pub focus_address: bool,
    pub error: Option<String>,
    #[cfg(windows)]
    view: Option<wry::WebView>,
    #[cfg(windows)]
    shown: bool,
}

impl WebTab {
    pub fn new(url: Option<&str>) -> WebTab {
        WebTab {
            page: Arc::new(Mutex::new(PageState::default())),
            address: url.map(normalize_url).unwrap_or_default(),
            seen_url: String::new(),
            start: url.map(normalize_url),
            focus_address: url.is_none(),
            error: None,
            #[cfg(windows)]
            view: None,
            #[cfg(windows)]
            shown: false,
        }
    }

    /// The tab's name: the page title, else its address.
    pub fn title(&self) -> String {
        let p = lock(&self.page);
        if !p.title.trim().is_empty() {
            return p.title.trim().to_string();
        }
        let url = if p.url.is_empty() { self.start.clone().unwrap_or_default() } else { p.url.clone() };
        match url.as_str() {
            "" | "about:blank" => "New Tab".into(),
            u => u.split_once("://").map_or(u, |(_, rest)| rest).trim_end_matches('/').to_string(),
        }
    }

    /// Follows the page's address in the bar unless it is being typed in.
    pub fn sync_address(&mut self, editing: bool) {
        let url = lock(&self.page).url.clone();
        if url != self.seen_url {
            self.seen_url = url.clone();
            if !editing && url != "about:blank" {
                self.address = url;
            }
        }
    }

    pub fn go(&mut self, typed: &str) {
        let url = normalize_url(typed);
        self.address = url.clone();
        #[cfg(windows)]
        if let Some(v) = &self.view {
            if let Err(e) = v.load_url(&url) {
                self.error = Some(e.to_string());
            }
            return;
        }
        self.start = Some(url);
    }
}

#[cfg(windows)]
impl WebTab {
    /// Creates the browser over `rect` (physical pixels of the window) once there is a window to
    /// put it in.
    pub fn ensure_view(
        &mut self,
        ctx: &egui::Context,
        window: &impl wry::raw_window_handle::HasWindowHandle,
        web_ctx: &mut wry::WebContext,
        rect: [i32; 4],
    ) {
        if self.view.is_some() || self.error.is_some() {
            return;
        }
        let page = Arc::clone(&self.page);
        let (p_title, p_load, p_new, p_ipc) = (Arc::clone(&page), Arc::clone(&page), Arc::clone(&page), Arc::clone(&page));
        let (c1, c2, c3, c4) = (ctx.clone(), ctx.clone(), ctx.clone(), ctx.clone());
        let url = self.start.clone().unwrap_or_else(|| "about:blank".into());
        let built = wry::WebViewBuilder::new_with_web_context(web_ctx)
            .with_url(&url)
            .with_bounds(bounds(rect))
            .with_visible(true)
            .with_initialization_script(KEY_SCRIPT)
            .with_document_title_changed_handler(move |t| {
                lock(&p_title).title = t;
                c1.request_repaint();
            })
            .with_on_page_load_handler(move |_, u| {
                lock(&p_load).url = u;
                c2.request_repaint();
            })
            .with_new_window_req_handler(move |u, _| {
                lock(&p_new).new_tabs.push(u);
                c3.request_repaint();
                wry::NewWindowResponse::Deny
            })
            .with_ipc_handler(move |req| {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(req.body())
                    && let Some(k) = v.get("k").and_then(serde_json::Value::as_str)
                {
                    lock(&p_ipc).keys.push((k.to_string(), v.get("s").and_then(serde_json::Value::as_bool).unwrap_or(false)));
                    c4.request_repaint();
                }
            })
            .build_as_child(window);
        match built {
            Ok(v) => {
                self.view = Some(v);
                self.shown = true;
            }
            Err(e) => {
                log::warn!("web tab: {e}");
                self.error = Some(format!("The web view could not start: {e}. Install the Microsoft Edge WebView2 Runtime."));
            }
        }
    }

    pub fn place(&mut self, rect: [i32; 4], visible: bool) {
        let Some(v) = &self.view else { return };
        let visible = visible && rect[2] > 2 && rect[3] > 2;
        if visible {
            let _ = v.set_bounds(bounds(rect));
        }
        if visible != self.shown {
            let _ = v.set_visible(visible);
            self.shown = visible;
        }
    }

    /// Hides the page (not the active tab, or something of the app's is drawn over it).
    pub fn hide(&mut self) {
        if let Some(v) = &self.view
            && self.shown
        {
            let _ = v.set_visible(false);
            self.shown = false;
        }
    }

    pub fn back(&self) {
        self.script("history.back()");
    }

    pub fn forward(&self) {
        self.script("history.forward()");
    }

    pub fn reload(&self) {
        if let Some(v) = &self.view {
            let _ = v.reload();
        }
    }

    pub fn focus_page(&self) {
        if let Some(v) = &self.view {
            let _ = v.focus();
        }
    }

    pub fn focus_app(&self) {
        if let Some(v) = &self.view {
            let _ = v.focus_parent();
        }
    }

    fn script(&self, js: &str) {
        if let Some(v) = &self.view {
            let _ = v.evaluate_script(js);
        }
    }
}

#[cfg(windows)]
fn bounds([x, y, w, h]: [i32; 4]) -> wry::Rect {
    wry::Rect {
        position: wry::dpi::PhysicalPosition::new(x, y).into(),
        size: wry::dpi::PhysicalSize::new(u32::try_from(w.max(1)).unwrap_or(1), u32::try_from(h.max(1)).unwrap_or(1)).into(),
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_url;

    #[test]
    fn addresses_keep_their_scheme_and_domains_get_https() {
        assert_eq!(normalize_url("https://example.com/a?b=1"), "https://example.com/a?b=1");
        assert_eq!(normalize_url("  http://localhost:8080 "), "http://localhost:8080");
        assert_eq!(normalize_url("about:blank"), "about:blank");
        assert_eq!(normalize_url("example.com"), "https://example.com");
        assert_eq!(normalize_url("docs.rs/wry/latest"), "https://docs.rs/wry/latest");
        assert_eq!(normalize_url("sub.example.co.uk:8443/x"), "https://sub.example.co.uk:8443/x");
        assert_eq!(normalize_url("localhost:3000"), "http://localhost:3000");
        assert_eq!(normalize_url("192.168.1.1/admin"), "http://192.168.1.1/admin");
        assert_eq!(normalize_url(""), "about:blank");
    }

    #[test]
    fn words_become_a_search() {
        assert_eq!(normalize_url("rust egui"), "https://duckduckgo.com/?q=rust+egui");
        assert_eq!(normalize_url("what is 1.5 + 2?"), "https://duckduckgo.com/?q=what+is+1.5+%2B+2%3F");
        assert_eq!(normalize_url("cats"), "https://duckduckgo.com/?q=cats");
    }
}
