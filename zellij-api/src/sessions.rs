//! Session lifecycle: listing, creating, killing, and keeping one live
//! [`SessionLink`] per session that the API is driving.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use zellij_client::os_input_output::{get_cli_client_os_input, ClientOsApi};
use zellij_client::spawn_server_with_env;
use zellij_utils::data::{LayoutInfo, WebSharing};
use zellij_utils::envs::SESSION_NAME_ENV_KEY;
use zellij_utils::input::cli_assets::CliAssets;
use zellij_utils::input::options::Options;
use zellij_utils::ipc::{ClientToServerMsg, ServerToClientMsg};
use zellij_utils::pane_size::Size;
use zellij_utils::sessions::{
    generate_unique_session_name, get_resurrectable_session_names, get_sessions, session_exists,
    validate_session_name,
};

use crate::session_link::{session_socket_path, SessionLink};

/// Terminal geometry used for sessions created through the API. There is no
/// real terminal behind them, but panes still need a size — and it is the size
/// the canvas diffs are computed against.
pub const DEFAULT_ROWS: usize = 24;
pub const DEFAULT_COLS: usize = 80;

/// How long we wait for a freshly spawned session to accept a connection.
const SESSION_START_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub name: String,
    /// Seconds since the session was created.
    pub age_secs: u64,
    /// A dead session that can be resurrected rather than a running one.
    pub resurrectable: bool,
}

#[derive(Default)]
pub struct SessionManager {
    links: Mutex<HashMap<String, Arc<SessionLink>>>,
    /// Names of sessions currently being created. Creations run concurrently
    /// (the spawned server gets its name through its own environment, not a
    /// process-wide variable), so this set is what keeps two in-flight
    /// creations from claiming the same name.
    starting: Mutex<HashSet<String>>,
}

/// Removes a reserved name from [`SessionManager::starting`] however creation
/// exits, success or error.
#[derive(Debug)]
struct StartingReservation<'a> {
    starting: &'a Mutex<HashSet<String>>,
    name: String,
}

impl Drop for StartingReservation<'_> {
    fn drop(&mut self) {
        self.starting.lock().unwrap().remove(&self.name);
    }
}

impl SessionManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn list(&self) -> Vec<SessionSummary> {
        let mut summaries: Vec<SessionSummary> = get_sessions()
            .unwrap_or_default()
            .into_iter()
            .map(|(name, age)| SessionSummary {
                name,
                age_secs: age.as_secs(),
                resurrectable: false,
            })
            .collect();

        let running: Vec<String> = summaries.iter().map(|s| s.name.clone()).collect();
        for name in get_resurrectable_session_names() {
            if !running.contains(&name) {
                summaries.push(SessionSummary {
                    name,
                    age_secs: 0,
                    resurrectable: true,
                });
            }
        }
        summaries.sort_by(|a, b| a.name.cmp(&b.name));
        summaries
    }

    /// Get (or open) the link to a session.
    ///
    /// The map is held for the whole operation, not just the lookup. Two
    /// commands arriving together for a session that is not linked yet would
    /// otherwise each open one — and since a link attaches a client, the
    /// session would end up with two of them, which quietly breaks focus
    /// resolution (it identifies our client by being the only one attached).
    ///
    /// A link whose session has ended is replaced rather than handed out, so a
    /// caller gets a clear "is not running" instead of commands timing out
    /// against a dead socket.
    ///
    /// Note that opening a link connects three sockets while the map is locked.
    /// `SessionLink::connect` proves the session is alive first (that check
    /// connects and round-trips a status message), because the underlying
    /// connect retries forever against a socket that is not answering — and
    /// with the map held, a wedged connect would block every session.
    pub fn link(&self, session_name: &str) -> Result<Arc<SessionLink>, String> {
        let mut links = self.links.lock().unwrap();
        match links.get(session_name) {
            Some(existing) if existing.is_alive() => return Ok(existing.clone()),
            Some(_) => {
                if let Some(dead) = links.remove(session_name) {
                    dead.shutdown();
                }
            },
            None => {},
        }
        let link = SessionLink::connect(session_name)?;
        links.insert(session_name.to_string(), link.clone());
        Ok(link)
    }

    /// The link to a session, if one is already open.
    ///
    /// Unlike [`Self::link`] this never opens one — for commands that only
    /// tidy up local state, opening three sockets and attaching a client would
    /// be a strange side effect.
    pub fn existing_link(&self, session_name: &str) -> Option<Arc<SessionLink>> {
        self.links.lock().unwrap().get(session_name).cloned()
    }

    /// Drop our link to a session (after it ended, or on unsubscribe).
    pub fn drop_link(&self, session_name: &str) {
        if let Some(link) = self.links.lock().unwrap().remove(session_name) {
            link.shutdown();
        }
    }

    pub fn linked_sessions(&self) -> Vec<String> {
        self.links.lock().unwrap().keys().cloned().collect()
    }

    /// Create a new session with no terminal attached to it.
    ///
    /// This is the same handshake the bundled web client uses: spawn the server
    /// process, then send `FirstClientConnected` to initialise it. We disconnect
    /// once the session is up — Zellij keeps a session alive with no clients
    /// attached, which is exactly the detached session we want to drive.
    pub fn create(
        &self,
        name: Option<String>,
        layout: Option<String>,
        cwd: Option<String>,
        rows: Option<usize>,
        cols: Option<usize>,
    ) -> Result<String, String> {
        let (session_name, _reservation) = self.reserve_name(name)?;

        let socket_path = session_socket_path(&session_name);
        // Note: `validate_session_name` above already rejects a name whose
        // socket path would exceed the OS's Unix-socket path limit — this
        // matters more here than it would interactively, since
        // `wait_until_started` below can't tell "still starting" from
        // "never going to start" if the daemonized server never gets far
        // enough to send anything back, and would otherwise just block for
        // the full timeout instead of failing immediately with a message
        // that says why.
        // The spawned server reads this from its own environment to identify
        // itself. Set on the child only — a process-wide variable here would
        // race with concurrent creations.
        spawn_server_with_env(&socket_path, false, &[(SESSION_NAME_ENV_KEY, &session_name)])
            .map_err(|e| format!("could not spawn the session server: {}", e))?;

        let os_input =
            get_cli_client_os_input().map_err(|e| format!("could not open client IPC: {}", e))?;
        os_input.connect_to_server(&socket_path);

        let cli_assets = CliAssets {
            config_file_path: None,
            config_dir: None,
            should_ignore_config: false,
            configuration_options: Some(api_session_options()),
            layout: layout.as_deref().map(layout_info_from).transpose()?,
            terminal_window_size: Size {
                rows: rows.unwrap_or(DEFAULT_ROWS),
                cols: cols.unwrap_or(DEFAULT_COLS),
            },
            data_dir: None,
            is_debug: false,
            max_panes: None,
            force_run_layout_commands: false,
            cwd: cwd.map(PathBuf::from),
        };
        os_input.send_to_server(ClientToServerMsg::FirstClientConnected {
            cli_assets,
            is_web_client: false,
        });

        // Wait for the server to come up before handing the session back, so a
        // caller can immediately issue commands against it.
        wait_until_started(&os_input)?;
        os_input.send_to_server(ClientToServerMsg::ClientExited);

        log::info!("session '{}' is up", session_name);
        Ok(session_name)
    }

    /// Claim `wanted` (or a generated name) as being-created, failing if it is
    /// already running or already being created by a concurrent call. The
    /// returned reservation frees the claim when dropped, however creation
    /// exits — so a failed creation never blocks the name forever.
    fn reserve_name(
        &self,
        wanted: Option<String>,
    ) -> Result<(String, StartingReservation<'_>), String> {
        let mut starting = self.starting.lock().unwrap();
        let session_name = match wanted {
            Some(name) => name,
            None => generate_unique_session_name()
                .ok_or_else(|| "could not generate a session name".to_string())?,
        };
        validate_session_name(&session_name)?;
        if starting.contains(&session_name) || session_exists(&session_name).unwrap_or(false) {
            return Err(format!("session '{}' already exists", session_name));
        }
        starting.insert(session_name.clone());
        Ok((
            session_name.clone(),
            StartingReservation {
                starting: &self.starting,
                name: session_name,
            },
        ))
    }

    pub fn kill(&self, session_name: &str) -> Result<(), String> {
        // See `session_exists_settled`: a plain `session_exists` can still
        // report false right after this session was renamed into
        // `session_name`, even though it is definitely running.
        if !crate::session_link::session_exists_settled(session_name).unwrap_or(false) {
            return Err(format!("session '{}' is not running", session_name));
        }
        self.drop_link(session_name);

        let os_input =
            get_cli_client_os_input().map_err(|e| format!("could not open client IPC: {}", e))?;
        os_input.connect_to_server(&session_socket_path(session_name));
        os_input.send_to_server(ClientToServerMsg::KillSession);
        // The server acknowledges by closing the connection or sending Exit.
        let _ = os_input.recv_from_server();
        Ok(())
    }
}

/// Options for sessions the API creates.
///
/// Zellij greets a new session with the release notes and a startup tip. Both
/// are floating plugin panes, and a floating pane takes focus — so a session
/// created for a program to drive would start with focus on a welcome screen
/// rather than on a shell, and input sent without naming a pane would go to
/// that welcome screen.
///
/// These are the supported switches for exactly this, so the greeting is simply
/// never created. Only the fields set here are applied; everything else still
/// comes from the user's config, because `Options` merges field by field.
fn api_session_options() -> Options {
    Options {
        show_release_notes: Some(false),
        show_startup_tips: Some(false),
        // Web clients are permitted because a session created through this
        // API exists to be driven and observed by a program, and the web UI
        // is the only way to SEE one as a picture — the control API returns
        // plain text lines with no styling, so `read_image` renders through
        // the web client or not at all.
        //
        // `WebSharing` defaults to `Off`, and that default is what made
        // every capture fail in a thoroughly misleading way: the terminal
        // WebSocket upgraded with 101 and was then closed by the server, so
        // the browser showed "CONNECTION LOST" and the picture came back as
        // a screenshot of that dialog. `/session-list` reported
        // `web_clients_allowed: false` for every session, which is the only
        // place the real reason was visible.
        //
        // Scoped deliberately: this applies ONLY to sessions this API
        // creates, never to ones started in a terminal, and the web server
        // it enables binds loopback and requires its own login token.
        web_sharing: Some(WebSharing::On),
        ..Default::default()
    }
}

/// Wait for the first message from a freshly created session.
fn wait_until_started(os_input: &dyn ClientOsApi) -> Result<(), String> {
    let deadline = std::time::Instant::now() + SESSION_START_TIMEOUT;
    // `recv_from_server` blocks, so this thread parks until the server speaks.
    // The server always sends something once it has initialised the session.
    loop {
        match os_input.recv_from_server() {
            Some((ServerToClientMsg::Exit { exit_reason }, _)) => {
                return Err(format!("session exited during startup: {}", exit_reason));
            },
            Some((ServerToClientMsg::LogError { lines }, _)) => {
                return Err(lines.join("\n"));
            },
            // A render means the session has laid out its panes and is live.
            // `UnblockInputThread` only means our message was routed, which
            // happens before the session finishes coming up — so it is not
            // enough to report success on.
            Some((ServerToClientMsg::Connected, _))
            | Some((ServerToClientMsg::Render { .. }, _)) => return Ok(()),
            Some(_) => {
                if std::time::Instant::now() > deadline {
                    return Err("timed out waiting for the session to start".to_string());
                }
            },
            None => return Err("lost the connection while starting the session".to_string()),
        }
    }
}

/// Interpret the `layout` parameter: a bare name is a built-in layout, anything
/// that looks like a path is a layout file.
///
/// A layout file that is not there is rejected here. The session would
/// otherwise start perfectly happily with the default layout, and the caller
/// would be told the session was created — with no hint that the layout it
/// asked for was never applied.
fn layout_info_from(layout: &str) -> Result<LayoutInfo, String> {
    let looks_like_a_path = layout.contains(std::path::MAIN_SEPARATOR) || layout.ends_with(".kdl");
    if looks_like_a_path {
        if !std::path::Path::new(layout).exists() {
            return Err(format!("no layout file at '{}'", layout));
        }
        Ok(LayoutInfo::File(layout.to_string(), Default::default()))
    } else {
        Ok(LayoutInfo::BuiltIn(layout.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_layout_names_are_built_ins() {
        assert!(matches!(
            layout_info_from("compact"),
            Ok(LayoutInfo::BuiltIn(name)) if name == "compact"
        ));
    }

    #[test]
    fn layout_paths_are_files() {
        let existing = std::env::current_exe().expect("test binary path");
        let path = existing.display().to_string();
        assert!(matches!(
            layout_info_from(&path),
            Ok(LayoutInfo::File(found, _)) if found == path
        ));
    }

    #[test]
    fn a_layout_file_that_is_not_there_is_refused() {
        // Otherwise the session starts with the default layout and reports
        // success, having quietly ignored what was asked for.
        let result = layout_info_from("/nonexistent/definitely/not/here.kdl");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no layout file"));
    }

    #[test]
    fn creating_a_session_with_an_invalid_name_is_refused() {
        let manager = SessionManager::new();
        // Session names may not contain path separators — that would escape
        // the socket directory.
        let result = manager.create(Some("../evil".to_string()), None, None, None, None);
        assert!(result.is_err(), "expected an invalid name to be refused");
    }

    #[test]
    fn killing_a_session_that_is_not_running_is_an_error() {
        let manager = SessionManager::new();
        let result = manager.kill("zellij-api-definitely-not-a-session");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not running"));
    }

    #[test]
    fn linking_to_a_missing_session_does_not_cache_anything() {
        let manager = SessionManager::new();
        assert!(manager.link("zellij-api-definitely-not-a-session").is_err());
        assert!(manager.linked_sessions().is_empty());
    }

    #[test]
    fn a_name_already_being_created_cannot_be_claimed_again() {
        let manager = SessionManager::new();
        let name = "zellij-api-test-reservation".to_string();

        let held = manager.reserve_name(Some(name.clone())).expect("first claim");

        let second = manager.reserve_name(Some(name.clone()));
        assert!(second.is_err(), "a concurrent claim on the same name must be refused");
        assert!(second.unwrap_err().contains("already exists"));
        drop(held);
    }

    #[test]
    fn dropping_a_reservation_frees_the_name_for_the_next_attempt() {
        let manager = SessionManager::new();
        let name = "zellij-api-test-retry".to_string();

        let first = manager.reserve_name(Some(name.clone())).expect("first claim");
        drop(first);

        // A failed creation drops its reservation on the way out — the same
        // name must then be claimable again rather than blocked forever.
        assert!(manager.reserve_name(Some(name)).is_ok());
    }

    #[test]
    fn concurrent_generated_reservations_never_collide() {
        let manager = std::sync::Arc::new(SessionManager::new());

        let claims: Vec<_> = (0..16)
            .map(|_| {
                let manager = manager.clone();
                std::thread::spawn(move || {
                    manager.reserve_name(None).map(|(name, reservation)| {
                        // Hold the reservation long enough for the others to
                        // run, as overlapping creations would.
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        drop(reservation);
                        name
                    })
                })
            })
            .collect();

        let mut names: Vec<String> =
            claims.into_iter().map(|t| t.join().unwrap().expect("a generated claim")).collect();
        let total = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), total, "every concurrent creation must get a distinct name");
    }

    #[test]
    fn a_reservation_is_freed_even_when_creation_fails() {
        let manager = SessionManager::new();
        let name = "zellij-api-test-failed-create".to_string();

        // This create fails long after the name is reserved (there is no
        // real zellij server to talk to in a unit test).
        let result = manager.create(Some(name.clone()), None, None, None, None);
        assert!(result.is_err());

        // The failure must not leave the name permanently claimed.
        assert!(
            manager.reserve_name(Some(name)).is_ok(),
            "a failed creation must release its name reservation"
        );
    }
}
