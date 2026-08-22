//! Screenshotting a session's terminal, by driving Zellij's own web UI.
//!
//! The control API only ever hands back plain text lines — no styled or
//! colour cell data reaches that layer (see `zellij-api/src/canvas.rs`), so
//! there is nothing to rasterize server-side and `read_screen` is as much as
//! text can give you. But some output only makes sense as a picture: box
//! drawing, colour-coded state, progress meters, and anything where layout
//! carries the meaning rather than the characters do.
//!
//! Zellij already ships a full browser-based terminal renderer — its web
//! server serves a real xterm.js UI at `GET /{session}`. So rather than
//! reimplementing terminal rendering here, this drives that existing UI with
//! a headless browser and captures what it draws. The picture an agent gets
//! is exactly the one a person would see.
//!
//! Auth, per `zellij-client/src/web_client/http_handlers.rs`: obtain a token
//! out of band (`zellij web --create-token`), `POST /command/login` with
//! `{auth_token, remember_me}`, and the `session_token` cookie that comes
//! back is what `GET /{session}` requires. That POST is issued from inside
//! the page rather than from Rust, so the browser stores the resulting
//! cookie itself — no HTTP client, no cookie jar, and no chance of the two
//! disagreeing about domain or path.

use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::{CaptureScreenshotFormat, Viewport};
use futures_util::StreamExt;

/// Where the Zellij web server lives, and how to authenticate to it.
#[derive(Clone, Debug)]
pub struct WebCaptureConfig {
    /// Base URL of the running Zellij web server, e.g. `http://127.0.0.1:8082`.
    pub base_url: String,
    /// Token from `zellij web --create-token`. Omit if the server has none.
    pub auth_token: Option<String>,
    /// How long to let the terminal paint before capturing.
    pub settle: Duration,
    /// Which binary serves the web UI — see `ensure_web_server`.
    pub binary: String,
    /// The capture viewport, in pixels — see `WebCaptureConfig::new`.
    pub viewport: (u32, u32),
}

impl WebCaptureConfig {
    /// `viewport` is the browser window the terminal is rendered into, and
    /// it decides what the picture actually contains.
    ///
    /// It needs a deliberate default rather than whatever a headless
    /// browser happens to open with. A session driven through this API is
    /// attached by an oversized focus client (999 rows), so its pane is far
    /// taller than any window; the web client renders it into the window it
    /// has, and a capture then shows a slice with the recent output
    /// scrolled out of frame — a screenshot of blank terminal, taken of a
    /// session that had just printed exactly what was asked for.
    ///
    /// The default is a conventional large terminal: wide enough that
    /// ordinary command output does not wrap, tall enough to hold a screen's
    /// worth of it.
    pub fn new(
        base_url: String,
        auth_token: Option<String>,
        settle_ms: u64,
        binary: String,
        viewport: (u32, u32),
    ) -> Self {
        Self {
            base_url,
            auth_token,
            settle: Duration::from_millis(settle_ms),
            binary,
            viewport,
        }
    }

    /// Terminal rows and columns that fill this viewport without overflowing it.
    ///
    /// The session is resized to these for the capture, so the grid and the
    /// window agree: every row the session renders has somewhere to be
    /// drawn, and no rows are drawn that the picture cannot hold.
    ///
    /// Derived from the web UI's own font metrics rather than guessed —
    /// JetBrains Mono at the client's default size — with room reserved for
    /// Zellij's tab bar and status bar, which occupy a row each and are not
    /// part of the pane grid.
    pub fn capture_grid(&self) -> (u32, u32) {
        const CELL_WIDTH_PX: u32 = 10;
        const CELL_HEIGHT_PX: u32 = 19;
        const CHROME_ROWS: u32 = 2; // tab bar + status bar

        let cols = (self.viewport.0 / CELL_WIDTH_PX).max(20);
        let rows = (self.viewport.1 / CELL_HEIGHT_PX).saturating_sub(CHROME_ROWS).max(10);
        (rows, cols)
    }
}

/// A capture that did not happen, phrased for the caller rather than the log.
pub type CaptureError = String;

/// Logs in from inside the page, so the browser owns the resulting cookie.
///
/// Returns the server's own message on refusal — an expired or wrong token is
/// something the caller can act on, and is worth distinguishing from "the web
/// server is not running at all".
async fn login(page: &chromiumoxide::Page, token: &str) -> Result<(), CaptureError> {
    let script = format!(
        r#"(async () => {{
            const response = await fetch('/command/login', {{
                method: 'POST',
                headers: {{ 'content-type': 'application/json' }},
                body: JSON.stringify({{ auth_token: {token}, remember_me: false }}),
            }});
            const body = await response.json().catch(() => ({{}}));
            return JSON.stringify({{ ok: response.ok, status: response.status, body }});
        }})()"#,
        token = serde_json::Value::String(token.to_string()),
    );

    let raw: String = page
        .evaluate(script)
        .await
        .map_err(|e| format!("could not reach the Zellij web server to log in: {e}"))?
        .into_value()
        .map_err(|e| format!("Zellij web login returned something unreadable: {e}"))?;

    let parsed: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("Zellij web login returned malformed JSON: {e}"))?;

    if parsed.get("ok").and_then(|v| v.as_bool()) == Some(true)
        && parsed
            .get("body")
            .and_then(|b| b.get("success"))
            .and_then(|v| v.as_bool())
            != Some(false)
    {
        return Ok(());
    }

    let message = parsed
        .get("body")
        .and_then(|b| b.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("no reason given");
    Err(format!("Zellij web login was refused: {message}"))
}

/// Host and port to probe, from a base URL, defaulting the port by scheme.
fn host_and_port(base_url: &str) -> Option<(String, u16)> {
    let without_scheme = base_url
        .trim_end_matches('/')
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let authority = without_scheme.split('/').next()?;
    match authority.rsplit_once(':') {
        Some((host, port)) => Some((host.to_string(), port.parse().ok()?)),
        None => Some((
            authority.to_string(),
            if base_url.starts_with("https") { 443 } else { 80 },
        )),
    }
}

/// Whether anything is listening where the web server should be.
///
/// A TCP connect rather than an HTTP request: this only needs to answer "is
/// the server up", and connecting avoids pulling an HTTP client into a
/// process that otherwise has no use for one.
async fn is_listening(base_url: &str) -> bool {
    let Some((host, port)) = host_and_port(base_url) else {
        return false;
    };
    tokio::time::timeout(
        Duration::from_millis(500),
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    .map(|result| result.is_ok())
    .unwrap_or(false)
}

/// Starts Zellij's web server if it is not already up.
///
/// `read_image` is the only thing that needs this server, so requiring a
/// human to have started it first would make the tool fail for a reason that
/// has nothing to do with what was asked — and the fix would be a command
/// the agent cannot run. Starting it here makes the tool self-sufficient:
/// asking to look at a session is enough.
///
/// `<binary> web --start -d` is Zellij's own supported entry point. It is
/// only invoked when nothing is listening, so the common path costs one TCP
/// connect.
///
/// WHICH binary matters, and getting it wrong fails in a way that looks like
/// success: a build and its remote-control variant both default to port
/// 8082 and each only serves ITS OWN sessions. Starting the wrong one gives
/// a web server that answers on the right port and cannot see the session
/// you asked to photograph. This MCP server drives the remote-control
/// api-server, so it starts the matching web server by default.
async fn ensure_web_server(base_url: &str, binary: &str) -> Result<(), CaptureError> {
    if is_listening(base_url).await {
        return Ok(());
    }

    let output = tokio::process::Command::new(binary)
        .args(["web", "--start", "-d"])
        .output()
        .await
        .map_err(|e| {
            format!("could not run `{binary} web --start` to bring the web UI up: {e}")
        })?;

    if !output.status.success() {
        return Err(format!(
            "`{binary} web --start` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    // It daemonizes, so the command returning is not the same as the port
    // being open. Poll rather than sleeping a guessed amount.
    for _ in 0..20 {
        if is_listening(base_url).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(format!(
        "started `{binary} web` but nothing came up at {base_url}"
    ))
}

/// A PNG of the session's web-rendered terminal, base64-encoded for MCP.
///
/// The browser is launched per capture rather than kept warm. A capture is a
/// rare, deliberate act — an agent asking to *look* at something — and a
/// long-lived Chrome process attached to this server would be a standing
/// cost and a standing failure mode (a crashed browser silently breaking
/// every later capture) in exchange for latency nobody is waiting on.
pub async fn capture_session(
    config: &WebCaptureConfig,
    session: &str,
) -> Result<String, CaptureError> {
    ensure_web_server(&config.base_url, &config.binary).await?;

    let (mut browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .window_size(config.viewport.0, config.viewport.1)
            .build()?,
    )
    .await
    .map_err(|e| {
        format!("could not start a headless browser to capture the session: {e}")
    })?;

    // chromiumoxide requires its event stream to be driven for the browser
    // to make progress at all; dropping this task tears the browser down.
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = capture_with(&browser, config, session).await;

    // Best-effort teardown: the capture's own outcome is what the caller
    // asked for, and failing to close a browser must not turn a good
    // screenshot into an error.
    let _ = browser.close().await;
    handler_task.abort();

    result
}

/// What the page is currently showing, from the page's own point of view.
///
/// The web UI covers the terminal with a modal when it cannot show one, so
/// "did the screenshot work" is not a question the browser can answer —
/// navigation resolves, the page paints, and the picture is of a dialog.
/// A capture that returned that would be worse than an error: it looks like
/// a successful reading of a terminal that says "CONNECTION LOST".
#[derive(Debug, PartialEq)]
enum PageState {
    /// A real terminal with real content — the only capturable state
    Terminal,
    /// The login modal: the token is missing, wrong, or expired
    NeedsAuth,
    /// The reconnect modal: the terminal WebSocket dropped or never opened
    Disconnected,
    /// Neither modal, but nothing painted yet — usually just early
    Blank,
}

/// Reads the page's state, using what a person looking at it would use: the
/// modals it shows, and whether the terminal grid has any content.
async fn page_state(page: &chromiumoxide::Page) -> Result<PageState, CaptureError> {
    // "Has the terminal painted" cannot be answered from the DOM text.
    // xterm.js renders to a CANVAS under its default renderers, so
    // `.xterm-rows` is empty even when the screen is full of output —
    // checking it reported "never painted anything" for a terminal that was
    // rendering perfectly well. So: the modals are read from text (they are
    // real DOM), and the terminal is detected by its canvas having actual
    // pixel area, with the DOM-renderer case kept as a fallback for a
    // build configured that way.
    const PROBE: &str = r#"(() => {
        const text = (document.body && document.body.innerText) || '';
        if (text.includes('SECURITY TOKEN REQUIRED')) return 'auth';
        if (text.includes('CONNECTION LOST')) return 'disconnected';
        const rows = document.querySelector('.xterm-rows');
        if (rows && rows.innerText && rows.innerText.trim().length > 0) return 'terminal';
        const canvas = document.querySelector('.xterm canvas, canvas');
        if (canvas && canvas.width > 0 && canvas.height > 0) return 'terminal';
        return 'blank';
    })()"#;

    let raw: String = page
        .evaluate(PROBE)
        .await
        .map_err(|e| format!("could not inspect the web UI's state: {e}"))?
        .into_value()
        .map_err(|e| format!("the web UI's state was unreadable: {e}"))?;

    Ok(match raw.as_str() {
        "terminal" => PageState::Terminal,
        "auth" => PageState::NeedsAuth,
        "disconnected" => PageState::Disconnected,
        _ => PageState::Blank,
    })
}

/// Waits for a real terminal, recovering from the states that are recoverable.
///
/// A fixed sleep cannot do this job. The terminal paints only once its own
/// WebSocket has connected, and that connection legitimately races page load
/// — so a short wait catches a blank canvas and a long one is dead time on
/// every successful capture. Worse, when the socket drops the page shows a
/// reconnect modal indefinitely, and no amount of waiting turns that into a
/// terminal; it needs a reload.
///
/// So this polls the page's own state and acts on it: reload on a dropped
/// connection, log in again if the session expired mid-flight, and return a
/// precise error if it never gets there. What it will not do is screenshot a
/// modal and call it a reading.
async fn wait_until_terminal_is_showing(
    page: &chromiumoxide::Page,
    config: &WebCaptureConfig,
    session: &str,
) -> Result<(), CaptureError> {
    let deadline = tokio::time::Instant::now() + config.settle.max(Duration::from_secs(15));
    let mut last = PageState::Blank;
    let mut reloads = 0;

    while tokio::time::Instant::now() < deadline {
        last = page_state(page).await?;
        match last {
            PageState::Terminal => {
                // Painted, but the first frame can be a partial redraw as the
                // scrollback replays. One short settle AFTER content exists
                // costs far less than a fixed one before it.
                tokio::time::sleep(Duration::from_millis(250)).await;
                return Ok(());
            },
            PageState::Disconnected => {
                // Reloading re-runs the whole attach handshake, which is what
                // actually clears this — the modal's own retry can sit in a
                // failing loop indefinitely.
                if reloads < 3 {
                    reloads += 1;
                    let _ = page.reload().await;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            },
            PageState::NeedsAuth => {
                let Some(token) = &config.auth_token else {
                    return Err(format!(
                        "the Zellij web server requires a login token but none is configured — \
                         create one with `{} web --create-token` and pass it as ZELLIJ_WEB_TOKEN",
                        config.binary
                    ));
                };
                login(page, token).await?;
                let _ = page.reload().await;
                tokio::time::sleep(Duration::from_millis(300)).await;
            },
            PageState::Blank => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }

    Err(match last {
        PageState::Disconnected => format!(
            "the web UI could not attach to session {session}: its terminal connection kept \
             dropping. The session exists and the web server is reachable, so this is the \
             web server failing to attach to it — check that `{}` is serving the same Zellij \
             build that owns the session.",
            config.binary
        ),
        PageState::NeedsAuth => {
            "the Zellij web server rejected the configured login token".to_string()
        },
        _ => format!("session {session}'s terminal never painted anything to capture"),
    })
}

/// Scrolls the terminal to its newest output before the picture is taken.
///
/// A session driven through this API is attached by an oversized focus
/// client, so its pane is hundreds of rows tall while the capture window
/// holds a few dozen. Landing at the top of that buffer means photographing
/// the beginning of the session — blank rows and a stale prompt — while the
/// command someone just ran sits far below the frame. Verified exactly that
/// way: `read_screen` showed the marker plainly, and the screenshot of the
/// same pane at the same moment did not contain it.
///
/// Best-effort by design: if the viewport element is not there, the capture
/// is still worth taking, and a scroll failure should not turn a usable
/// picture into an error.
async fn scroll_to_live_output(page: &chromiumoxide::Page) {
    const SCROLL: &str = r#"(() => {
        const viewport = document.querySelector('.xterm-viewport');
        if (viewport) viewport.scrollTop = viewport.scrollHeight;
        return 'done';
    })()"#;
    let _: Result<String, _> = match page.evaluate(SCROLL).await {
        Ok(result) => result.into_value(),
        Err(_) => return,
    };
    // Let the renderer repaint at the new offset before capturing it.
    tokio::time::sleep(Duration::from_millis(200)).await;
}

/// The terminal's own rectangle on the page, for clipping the screenshot to it.
///
/// Measured from the live DOM rather than computed from the configured
/// viewport: the web UI decides where it puts the terminal and how big it
/// makes it, and any arithmetic here would be a second opinion that drifts
/// the moment that layout changes.
///
/// `None` when the element cannot be measured, which the caller treats as
/// "capture the whole page" — a picture with margins beats no picture.
async fn terminal_bounds(page: &chromiumoxide::Page) -> Option<Viewport> {
    const BOUNDS: &str = r#"(() => {
        const el = document.querySelector('.xterm') || document.querySelector('canvas');
        if (!el) return '';
        const r = el.getBoundingClientRect();
        if (r.width < 1 || r.height < 1) return '';
        return JSON.stringify({x: r.x, y: r.y, width: r.width, height: r.height});
    })()"#;

    let raw: String = page.evaluate(BOUNDS).await.ok()?.into_value().ok()?;
    if raw.is_empty() {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some(Viewport {
        x: parsed.get("x")?.as_f64()?,
        y: parsed.get("y")?.as_f64()?,
        width: parsed.get("width")?.as_f64()?,
        height: parsed.get("height")?.as_f64()?,
        scale: 1.0,
    })
}

async fn capture_with(
    browser: &Browser,
    config: &WebCaptureConfig,
    session: &str,
) -> Result<String, CaptureError> {
    let page = browser
        .new_page(config.base_url.as_str())
        .await
        .map_err(|e| {
            format!(
                "could not open the Zellij web server at {} — is `zellij web` running? ({e})",
                config.base_url
            )
        })?;

    if let Some(token) = &config.auth_token {
        login(&page, token).await?;
    }

    let url = format!(
        "{}/{}",
        config.base_url.trim_end_matches('/'),
        urlencoding_encode(session)
    );
    page.goto(&url)
        .await
        .map_err(|e| format!("could not open session {session} in the web UI: {e}"))?;

    wait_until_terminal_is_showing(&page, config, session).await?;
    scroll_to_live_output(&page).await;

    // Framed on the terminal itself, not the browser window.
    //
    // The window is whatever size the capture was configured with, and the
    // web UI centres the terminal inside it — so a full-page shot carries
    // page background around the content, which is margin the caller has to
    // look past and pay tokens for. Clipping to the terminal's own bounding
    // box means the image IS the pane.
    let clip = terminal_bounds(&page).await;
    let mut params = chromiumoxide::page::ScreenshotParams::builder()
        .format(CaptureScreenshotFormat::Png);
    if let Some(clip) = clip {
        params = params.clip(clip);
    }

    let png = page
        .screenshot(params.build())
        .await
        .map_err(|e| format!("could not capture the session's screen: {e}"))?;

    Ok(BASE64.encode(png))
}

/// Percent-encodes a session name for use as a single path segment.
///
/// Session names are user-chosen and reach us verbatim; one containing a
/// space or a slash would otherwise build a URL pointing at something else
/// entirely, or at nothing.
fn urlencoding_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::urlencoding_encode;

    #[test]
    fn leaves_an_ordinary_session_name_alone() {
        assert_eq!(urlencoding_encode("home-dan-dev-todo_list"), "home-dan-dev-todo_list");
    }

    #[test]
    fn encodes_characters_that_would_change_which_url_is_requested() {
        assert_eq!(urlencoding_encode("my session"), "my%20session");
        assert_eq!(urlencoding_encode("a/b"), "a%2Fb");
    }
}
