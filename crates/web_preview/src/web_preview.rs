//! A web page in a workspace tab, rendered by a native WKWebView that agents
//! drive through the same view the user watches.

#[cfg(target_os = "macos")]
mod agent_tools;
#[cfg(target_os = "macos")]
mod native;
#[cfg(target_os = "macos")]
mod preview;

use std::collections::VecDeque;

use gpui::{Bounds, Pixels};
use serde::Deserialize;

gpui::actions!(
    web_preview,
    [
        /// Opens a web preview tab at http://localhost:3000.
        Open,
    ]
);

pub const DEFAULT_URL: &str = "http://localhost:3000";

/// `window.webkit.messageHandlers.<name>` the page script posts to.
const MESSAGE_HANDLER_NAME: &str = "cuePreview";

/// Injected at document start in every page. Messages are JSON strings so the
/// Rust side only ever decodes one shape. Page code can post the same
/// messages, so nothing here is trusted beyond display and focus.
const PAGE_SCRIPT: &str = r#"(() => {
  const handler = window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.cuePreview;
  if (!handler) return;
  const post = (message) => { try { handler.postMessage(JSON.stringify(message)); } catch (_) {} };
  const format = (value) => {
    if (typeof value === 'string') return value;
    if (value instanceof Error) return value.stack || String(value);
    try { return JSON.stringify(value); } catch (_) { return String(value); }
  };
  for (const level of ['log', 'info', 'warn', 'error', 'debug']) {
    const original = console[level];
    console[level] = function (...args) {
      post({ kind: 'console', level, text: args.map(format).join(' ') });
      return original.apply(this, args);
    };
  }
  window.addEventListener('error', (event) => {
    post({ kind: 'console', level: 'error', text: `${event.message} (${event.filename}:${event.lineno}:${event.colno})` });
  });
  window.addEventListener('unhandledrejection', (event) => {
    post({ kind: 'console', level: 'error', text: 'Unhandled rejection: ' + format(event.reason) });
  });
  window.addEventListener('pointerdown', () => post({ kind: 'focus' }), true);
  window.addEventListener('DOMContentLoaded', () => {
    post({ kind: 'loaded', url: location.href, title: document.title });
  });
})();"#;

#[cfg(target_os = "macos")]
pub fn init(cx: &mut gpui::App) {
    preview::init(cx);
}

#[cfg(not(target_os = "macos"))]
pub fn init(_: &mut gpui::App) {}

/// A rectangle in an AppKit view's coordinate space.
#[derive(Debug, Clone, Copy, PartialEq)]
struct AppKitFrame {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// Converts GPUI bounds (logical pixels, top-left origin, relative to GPUI's
/// view, which fills the content view) into a frame in the content view.
/// AppKit views are bottom-left origin unless flipped.
fn appkit_frame(
    bounds: Bounds<Pixels>,
    container_height: f64,
    container_is_flipped: bool,
) -> AppKitFrame {
    let x = f64::from(f32::from(bounds.origin.x));
    let top = f64::from(f32::from(bounds.origin.y));
    let width = f64::from(f32::from(bounds.size.width));
    let height = f64::from(f32::from(bounds.size.height));
    let y = if container_is_flipped {
        top
    } else {
        container_height - top - height
    };
    AppKitFrame {
        x,
        y,
        width,
        height,
    }
}

/// Accepts what people type into a URL bar: a bare host gets `http://`.
fn normalize_url(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if input.contains("://") || input.starts_with("about:") || input.starts_with("data:") {
        Some(input.to_string())
    } else {
        Some(format!("http://{input}"))
    }
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PageMessage {
    Console { level: String, text: String },
    Focus,
    Loaded { url: String, title: String },
}

impl PageMessage {
    fn parse(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok()
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct ConsoleEntry {
    level: String,
    text: String,
}

const CONSOLE_CAPACITY: usize = 500;
const CONSOLE_TEXT_LIMIT: usize = 4000;

/// The page's recent console output. Bounded because the page controls how
/// much it logs.
#[derive(Default)]
struct ConsoleBuffer {
    entries: VecDeque<ConsoleEntry>,
    dropped: usize,
}

impl ConsoleBuffer {
    fn push(&mut self, level: String, mut text: String) {
        if text.len() > CONSOLE_TEXT_LIMIT {
            let mut end = CONSOLE_TEXT_LIMIT;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push('…');
        }
        if self.entries.len() == CONSOLE_CAPACITY {
            self.entries.pop_front();
            self.dropped += 1;
        }
        self.entries.push_back(ConsoleEntry { level, text });
    }

    /// Everything since the last take, and how many older entries were lost.
    fn take(&mut self) -> (Vec<ConsoleEntry>, usize) {
        let dropped = std::mem::take(&mut self.dropped);
        (self.entries.drain(..).collect(), dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    #[test]
    fn test_appkit_frame_flips_unflipped_containers() {
        let bounds = Bounds {
            origin: point(px(100.), px(40.)),
            size: size(px(300.), px(200.)),
        };
        assert_eq!(
            appkit_frame(bounds, 800., false),
            AppKitFrame {
                x: 100.,
                y: 560.,
                width: 300.,
                height: 200.,
            }
        );
        assert_eq!(appkit_frame(bounds, 800., true).y, 40.);
        let flush_bottom = Bounds {
            origin: point(px(0.), px(600.)),
            size: size(px(10.), px(200.)),
        };
        assert_eq!(appkit_frame(flush_bottom, 800., false).y, 0.);
    }

    #[test]
    fn test_normalize_url() {
        assert_eq!(
            normalize_url(" localhost:3000/a "),
            Some("http://localhost:3000/a".to_string())
        );
        assert_eq!(
            normalize_url("https://example.com"),
            Some("https://example.com".to_string())
        );
        assert_eq!(normalize_url("  "), None);
    }

    #[test]
    fn test_page_messages_parse() {
        assert_eq!(
            PageMessage::parse(r#"{"kind":"console","level":"warn","text":"hi"}"#),
            Some(PageMessage::Console {
                level: "warn".into(),
                text: "hi".into(),
            })
        );
        assert_eq!(
            PageMessage::parse(r#"{"kind":"focus"}"#),
            Some(PageMessage::Focus)
        );
        assert_eq!(PageMessage::parse(r#"{"kind":"nope"}"#), None);
        assert_eq!(PageMessage::parse("not json"), None);
    }

    #[test]
    fn test_console_buffer_is_bounded() {
        let mut buffer = ConsoleBuffer::default();
        for index in 0..CONSOLE_CAPACITY + 3 {
            buffer.push("log".into(), index.to_string());
        }
        buffer.push("error".into(), "é".repeat(CONSOLE_TEXT_LIMIT));
        let (entries, dropped) = buffer.take();
        assert_eq!(entries.len(), CONSOLE_CAPACITY);
        assert_eq!(dropped, 4);
        assert_eq!(entries[0].text, "4");
        let last = &entries[CONSOLE_CAPACITY - 1].text;
        assert!(last.len() <= CONSOLE_TEXT_LIMIT + '…'.len_utf8());
        assert!(last.ends_with('…'));
        assert_eq!(buffer.take(), (Vec::new(), 0));
    }
}
