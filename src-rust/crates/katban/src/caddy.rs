//! caddy-managed config generation for hosted sites (spec §10.1 / §10.1a).
//!
//! Katban owns a single include file (`katban.conf`) that a one-time `import`
//! line in the host Caddyfile pulls in. Generation and atomic writes live
//! here and are fully unit-tested without a caddy binary; the one-time host
//! bootstrap (import line + reloader units) is rendered as instructions the
//! user approves once. The host-side auto-reload watcher (systemd path unit)
//! is the primary reload trigger per the spec.

use crate::config::SiteConfig;
use std::path::{Path, PathBuf};

pub const MANAGED_START: &str = "# ---- KATBAN-MANAGED (do not edit by hand) ----";
pub const MANAGED_END: &str = "# ---- END KATBAN-MANAGED ----";
pub const DEFAULT_INCLUDE_NAME: &str = "katban.conf";

/// How a site is served once exposed through caddy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiteKind {
    /// caddy serves the folder directly (static / published, no Katban in the path).
    Static { root: PathBuf },
    /// caddy proxies to Katban's per-site live-reload handler on 127.0.0.1.
    Live { port: u16 },
}

/// Locked (published) sites are served statically by caddy; live sites proxy
/// to Katban's loopback handler so edits refresh browsers in real time.
pub fn site_kind(site: &SiteConfig) -> SiteKind {
    if site.locked {
        SiteKind::Static {
            root: site.root.clone(),
        }
    } else {
        SiteKind::Live { port: site.port }
    }
}

pub fn render_block(site: &SiteConfig, kind: &SiteKind) -> String {
    let host = site
        .public_subdomain
        .as_deref()
        .unwrap_or(site.name.as_str());
    match kind {
        SiteKind::Static { root } => {
            format!(
                "{host} {{\n    encode gzip\n    root * {}\n    file_server\n}}",
                root.display()
            )
        }
        SiteKind::Live { port } => {
            format!("{host} {{\n    encode gzip\n    reverse_proxy 127.0.0.1:{port}\n}}")
        }
    }
}

/// The guest chat server as a caddy block (spec §9/§10.1): a live proxy to
/// the loopback guest server, so `chat.example.com` reaches it.
pub fn render_guest_block(host: &str, port: u16) -> String {
    format!("{host} {{\n    encode gzip\n    reverse_proxy 127.0.0.1:{port}\n}}")
}

/// The admin board web app as a caddy block: a live proxy to the loopback
/// board server, so an admin subdomain (e.g. `board.example.com`)
/// reaches it over https with the Secure cookie honored via caddy's
/// `X-Forwarded-Proto`.
pub fn render_board_block(host: &str, port: u16) -> String {
    format!("{host} {{\n    encode gzip\n    reverse_proxy 127.0.0.1:{port}\n}}")
}

/// Concatenate the managed blocks for every exposed site (plus the guest chat
/// block when a public subdomain is configured, plus the admin board block
/// when it has a public subdomain) inside the KATBAN-MANAGED markers.
/// Idempotent: rendering the same set twice is equal.
pub fn render_config(
    sites: &[(SiteConfig, SiteKind)],
    guest: Option<(&str, u16)>,
    board: Option<(&str, u16)>,
) -> String {
    let mut out = String::new();
    out.push_str(MANAGED_START);
    out.push('\n');
    for (site, kind) in sites {
        out.push_str(&render_block(site, kind));
        out.push('\n');
    }
    if let Some((host, port)) = guest {
        out.push_str(&render_guest_block(host, port));
        out.push('\n');
    }
    if let Some((host, port)) = board {
        out.push_str(&render_board_block(host, port));
        out.push('\n');
    }
    out.push_str(MANAGED_END);
    out.push('\n');
    out
}

/// Write a file atomically (temp + rename) so a reload never observes a
/// half-written config — same discipline the spec requires for `katban.conf`.
pub fn write_atomic(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("conf.tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Systemd path-unit that auto-reloads caddy whenever the managed config
/// changes. `managed_path` is the include file the admin chose (`--caddy-dir`
/// or the default `/etc/caddy/katban.conf`) — the unit must watch the same
/// file the expose commands write, or reloads silently never fire.
pub fn render_reloader_path_unit(managed_path: &Path) -> String {
    format!(
        "# Installed by clawde katban site expose — reloads caddy when the managed config changes.
[Unit]
Description=Reload caddy when Katban's managed config changes

[Path]
PathChanged={}

[Install]
WantedBy=multi-user.target
",
        managed_path.display()
    )
}

/// Systemd oneshot service the path unit triggers.
pub fn render_reloader_service_unit(reload_command: &str) -> String {
    format!(
        "# Installed by clawde katban site expose.
[Unit]
Description=Reload caddy after a Katban config change

[Service]
Type=oneshot
ExecStart={reload_command}
"
    )
}

/// The always-on Katban service unit (spec §11 — systemd is the default
/// runtime for this box because caddy is bare-metal; see the spec for why).
/// Runs the guest chat server on loopback, restarts on failure, survives
/// reboots, and logs to journalctl. `binary` is the resolved clawde binary
/// (`std::env::current_exe()` at render time) and `user` is the OS user that
/// owns `~/.clawde` + `/etc/caddy/katban.conf` — never root.
pub fn render_service_unit(binary: &str, user: &str, guest_port: u16) -> String {
    format!(
        "# Installed by clawde katban expose — Katban's always-on service.
# Rebuild the binary in place (e.g. `clawded`) and restart to update:
#   sudo systemctl restart katban
[Unit]
Description=Katban guest chat server
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
User={user}
ExecStart={binary} katban guest serve --port {guest_port}
Restart=always
RestartSec=5
PrivateTmp=true
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
"
    )
}

/// The always-on admin board + runner unit (spec §20.7 — the promise "an
/// always-on unit lands with the runner slice"). Serves the board web UI on
/// `port` and runs the card scheduler for `project` in the same process, so a
/// single unit keeps the board reachable *and* executing cards. One unit per
/// project (the spec leaves a multi-project scheduler open; until then N
/// boards = N units). `project` must be whitespace-free — it is embedded in
/// `ExecStart`.
pub fn render_board_service_unit(binary: &str, user: &str, project: &str, port: u16) -> String {
    format!(
        "# Installed by clawde katban board expose — the always-on board + runner.
# Rebuild the binary in place (e.g. `clawded`) and restart to update:
#   sudo systemctl restart katban-board
[Unit]
Description=Katban admin board + runner ({project})
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
User={user}
ExecStart={binary} katban board serve --port {port} --run {project}
Restart=always
RestartSec=5
PrivateTmp=true
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
"
    )
}

/// True when `s` is safe to embed as a hostname in a caddy block: only
/// `[A-Za-z0-9._-]`, no empty or dot-dot segments, no caddy syntax (`{{`,
/// `}}`, `#`, spaces), no path separators. Site names and public subdomains
/// are validated against this before they are rendered into the managed
/// config — an unvalidated value could inject caddy directives or break
/// every exposed site at the next reload.
pub fn valid_hostname(s: &str) -> bool {
    if s.is_empty() || s.len() > 253 || s.starts_with('.') || s.ends_with('.') {
        return false;
    }
    if s.contains("..") {
        return false;
    }
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// The one-time bootstrap steps the user approves (spec §10.1a). The import
/// line is the only change ever made to the host Caddyfile. `caddy_dir` is
/// where the managed include lives (`--caddy-dir` or the default `/etc/caddy`)
/// — the import uses the absolute path so the Caddyfile picks up the exact
/// file the expose commands write, whatever directory was chosen.
pub fn bootstrap_instructions(include_dir: &Path, caddy_dir: &Path) -> String {
    let managed = caddy_dir.join(DEFAULT_INCLUDE_NAME);
    format!(
        "One-time bootstrap (approve once, then Katban handles everything):\n\
         \n\
         1. Add this line to /etc/caddy/Caddyfile (site-block area, top level):\n\
            import {}\n\
         \n\
         2. Install Katban's always-on service (runs the guest chat server, \n\
            restarts on failure, survives reboots):\n\
            sudo install -m 644 {} /etc/systemd/system/katban.service\n\
            sudo systemctl daemon-reload\n\
            sudo systemctl enable --now katban.service\n\
         \n\
         3. Install the auto-reload watcher so future config changes need no \n\
            manual reload:\n\
            sudo install -m 644 {} /etc/systemd/system/katban-reload.path\n\
            sudo install -m 644 {} /etc/systemd/system/katban-reload.service\n\
            sudo systemctl daemon-reload\n\
            sudo systemctl enable --now katban-reload.path\n\
         \n\
         4. Apply the new routes once:\n\
            sudo systemctl reload caddy\n",
        managed.display(),
        include_dir.join("katban.service").display(),
        include_dir.join("katban-reload.path").display(),
        include_dir.join("katban-reload.service").display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(name: &str, subdomain: Option<&str>, locked: bool) -> SiteConfig {
        SiteConfig {
            name: name.to_string(),
            root: PathBuf::from(format!("/srv/{name}")),
            port: 8788,
            public_subdomain: subdomain.map(str::to_string),
            locked,
        }
    }

    #[test]
    fn live_site_renders_reverse_proxy_block() {
        let s = site("demo", Some("demo.example.com"), false);
        let block = render_block(&s, &site_kind(&s));
        assert!(block.contains("demo.example.com {"));
        assert!(block.contains("reverse_proxy 127.0.0.1:8788"));
        assert!(!block.contains("file_server"));
    }

    #[test]
    fn locked_site_renders_static_block() {
        let s = site("demo", Some("demo.example.com"), true);
        let block = render_block(&s, &site_kind(&s));
        assert!(block.contains("root * /srv/demo"));
        assert!(block.contains("file_server"));
        assert!(!block.contains("reverse_proxy"));
    }

    #[test]
    fn host_falls_back_to_site_name() {
        let s = site("demo", None, false);
        let block = render_block(&s, &site_kind(&s));
        assert!(block.starts_with("demo {"));
    }

    #[test]
    fn render_config_is_idempotent_and_marked() {
        let s1 = site("a", Some("a.example.com"), false);
        let s2 = site("b", Some("b.example.com"), true);
        let sites = vec![(s1.clone(), site_kind(&s1)), (s2.clone(), site_kind(&s2))];
        let once = render_config(&sites, None, None);
        let twice = render_config(&sites, None, None);
        assert_eq!(once, twice);
        assert!(once.starts_with(MANAGED_START));
        assert!(once.trim_end().ends_with(MANAGED_END));
        assert!(once.contains("a.example.com"));
        assert!(once.contains("b.example.com"));
    }

    #[test]
    fn empty_config_still_has_markers() {
        let text = render_config(&[], None, None);
        assert!(text.contains(MANAGED_START));
        assert!(text.contains(MANAGED_END));
    }

    #[test]
    fn guest_block_proxies_to_loopback_port() {
        let text = render_config(&[], Some(("chat.example.com", 8789)), None);
        assert!(text.contains("chat.example.com {"));
        assert!(text.contains("reverse_proxy 127.0.0.1:8789"));
        // Guest block coexists with site blocks.
        let s = site("demo", Some("demo.example.com"), false);
        let text = render_config(
            &[(s.clone(), site_kind(&s))],
            Some(("chat.example.com", 8789)),
            None,
        );
        assert!(text.contains("demo.example.com"));
        assert!(text.contains("chat.example.com"));
    }

    #[test]
    fn board_block_proxies_to_loopback_port() {
        let text = render_config(&[], None, Some(("board.example.com", 8790)));
        assert!(text.contains("board.example.com {"));
        assert!(text.contains("reverse_proxy 127.0.0.1:8790"));
        // Board + guest + sites coexist.
        let s = site("demo", Some("demo.example.com"), false);
        let text = render_config(
            &[(s.clone(), site_kind(&s))],
            Some(("chat.example.com", 8789)),
            Some(("board.example.com", 8790)),
        );
        assert!(text.contains("demo.example.com"));
        assert!(text.contains("chat.example.com"));
        assert!(text.contains("board.example.com"));
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("katban.conf");
        write_atomic(&path, "# content\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# content\n");
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind");
    }

    #[test]
    fn service_unit_runs_guest_serve_as_user_on_configured_port() {
        let unit = render_service_unit("/home/user/.local/bin/clawde", "user", 9000);
        assert!(unit.contains("Description=Katban guest chat server"));
        assert!(unit.contains("User=user"));
        assert!(
            unit.contains("ExecStart=/home/user/.local/bin/clawde katban guest serve --port 9000")
        );
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("WantedBy=multi-user.target"));
        assert!(!unit.contains("User=root"));
    }

    #[test]
    fn board_service_unit_runs_serve_with_runner_project() {
        let unit =
            render_board_service_unit("/home/user/.local/bin/clawde", "user", "demo", 8790);
        assert!(unit.contains("Description=Katban admin board + runner (demo)"));
        assert!(unit.contains("User=user"));
        assert!(unit.contains(
            "ExecStart=/home/user/.local/bin/clawde katban board serve --port 8790 --run demo"
        ));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("WantedBy=multi-user.target"));
        assert!(!unit.contains("User=root"));
    }

    #[test]
    fn bootstrap_mentions_service_and_reloader() {
        let dir = Path::new("/tmp/katban-units");
        let instructions = bootstrap_instructions(dir, Path::new("/etc/caddy"));
        assert!(instructions.contains("katban.service"));
        assert!(instructions.contains("systemctl enable --now katban.service"));
        assert!(instructions.contains("katban-reload.path"));
        assert!(instructions.contains("systemctl reload caddy"));
    }

    #[test]
    fn reloader_units_contain_expected_lines() {
        let path_unit = render_reloader_path_unit(Path::new("/etc/caddy/katban.conf"));
        assert!(path_unit.contains("[Path]"));
        assert!(path_unit.contains("PathChanged=/etc/caddy/katban.conf"));

        let custom = render_reloader_path_unit(Path::new("/home/user/caddy/katban.conf"));
        assert!(custom.contains("PathChanged=/home/user/caddy/katban.conf"));

        let service_unit = render_reloader_service_unit("systemctl reload caddy");
        assert!(service_unit.contains("[Service]"));
        assert!(service_unit.contains("Type=oneshot"));
        assert!(service_unit.contains("ExecStart=systemctl reload caddy"));
    }

    #[test]
    fn bootstrap_instructions_mention_import_and_reload() {
        let instructions =
            bootstrap_instructions(Path::new("/tmp/katban-caddy"), Path::new("/etc/caddy"));
        assert!(instructions.contains("import /etc/caddy/katban.conf"));
        assert!(instructions.contains("katban-reload.path"));
        assert!(instructions.contains("systemctl reload caddy"));

        // A custom caddy dir gets an absolute import of the actual file.
        let custom = bootstrap_instructions(
            Path::new("/tmp/katban-caddy"),
            Path::new("/home/user/caddy"),
        );
        assert!(custom.contains("import /home/user/caddy/katban.conf"));
    }

    #[test]
    fn valid_hostname_rejects_caddy_injection() {
        assert!(valid_hostname("demo.example.com"));
        assert!(valid_hostname("my_site-2"));
        assert!(!valid_hostname(""));
        assert!(!valid_hostname("demo } reverse_proxy evil {"));
        assert!(!valid_hostname("demo # comment"));
        assert!(!valid_hostname("a b"));
        assert!(!valid_hostname("../etc"));
        assert!(!valid_hostname("a..b"));
        assert!(!valid_hostname("a/b"));
        assert!(!valid_hostname(".leading"));
        assert!(!valid_hostname("trailing."));
        assert!(!valid_hostname(&"x".repeat(254)));
    }
}
