//! Drift diagnosis for the Katban / Cat Chat host surface.
//!
//! The failure modes this module looks for are all the same shape: the
//! *persisted* config (sites, Cat Chat links, admin store) says one thing, and
//! the host actually has something else installed. Observed in the wild:
//! a systemd unit pinned to a binary that no longer exists, a stale or
//! missing managed caddy include, and a Caddyfile `import` of a specific file
//! whose deletion silently takes every unrelated site down at the next reload.
//!
//! The checks are pure — no filesystem or process access — so they are unit
//! testable and the CLI gathers the facts and feeds them in.

use std::path::{Path, PathBuf};

/// Severity of a single diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Error,
}

/// One labelled finding, ready to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub level: Level,
    pub subject: String,
    pub message: String,
}

impl Finding {
    pub fn new(level: Level, subject: impl Into<String>, message: impl Into<String>) -> Self {
        Finding {
            level,
            subject: subject.into(),
            message: message.into(),
        }
    }
}

/// The managed include on disk compared with the freshly rendered desired
/// text. `actual` is `None` when the file does not exist.
pub fn check_managed_include(actual: Option<&str>, desired: &str) -> Finding {
    const SUBJECT: &str = "managed caddy include";
    match actual {
        None => Finding::new(
            Level::Warn,
            SUBJECT,
            "missing — re-run the relevant `expose` (or `doctor --fix`) to recreate it",
        ),
        Some(text) if text == desired => Finding::new(Level::Ok, SUBJECT, "matches desired state"),
        Some(_) => Finding::new(
            Level::Warn,
            SUBJECT,
            "on disk differs from desired state — re-run the relevant `expose` (or `doctor --fix`)",
        ),
    }
}

/// Check the Caddyfile `import` lines against `managed_path`, the include the
/// current `--caddy-dir` writes.
///
/// Two distinct failures, both of which leave every managed route dark:
///
/// - a **specific** `.conf` file is imported. Caddy aborts the whole reload
///   when a specifically named file is missing (an empty glob is a documented
///   no-op), so such an import means deleting the managed include takes down
///   every site on the box, not just Katban's routes.
/// - **nothing** imports the managed include: the bootstrap line was never
///   added, was removed, or points elsewhere. `import <snippet>` (no path) and
///   unrelated imports are fine, but one of them must load this file.
///
/// A relative token is resolved against the Caddyfile's directory, which is
/// the managed include's directory, matching caddy's own behaviour.
pub fn check_imports(caddyfile: &str, managed_path: &Path) -> Vec<Finding> {
    let managed = managed_path.display().to_string();
    let caddy_dir = managed_path.parent().unwrap_or(Path::new("."));
    let mut findings = Vec::new();
    let mut managed_loaded = false;
    for line in caddyfile.lines() {
        // Strip trailing comments; `#` inside a quoted arg is not valid here.
        let code = line.split('#').next().unwrap_or("").trim();
        let Some(rest) = code.strip_prefix("import ") else {
            continue;
        };
        let Some(pattern) = rest.split_whitespace().next().map(unquote) else {
            continue;
        };
        let relative = caddy_dir.join(pattern).display().to_string();
        // Two independent judgements: whether this line loads the managed
        // include at all, and whether it does so in the fragile form. A
        // specific import of the managed file counts as loaded *and* warns.
        if glob_matches(pattern, &managed) || glob_matches(&relative, &managed) {
            managed_loaded = true;
        }
        if pattern.ends_with(".conf") && !has_wildcard(pattern) {
            findings.push(Finding::new(
                Level::Warn,
                "caddy import",
                format!(
                    "`import {pattern}` names a specific file; if it is ever missing caddy \
                     refuses to reload and every site goes down — import a glob instead"
                ),
            ));
        }
    }
    if !managed_loaded {
        findings.insert(
            0,
            Finding::new(
                Level::Warn,
                "caddy import",
                format!(
                    "no `import` loads the managed include — caddy never serves these routes; \
                     add `import {}` to the Caddyfile (one-time bootstrap)",
                    expected_pattern(managed_path)
                ),
            ),
        );
    }
    findings
}

/// The glob the Caddyfile should import for a managed include, e.g.
/// `/etc/caddy/katban*.conf`.
fn expected_pattern(managed_path: &Path) -> String {
    let dir = managed_path.parent().unwrap_or(Path::new("."));
    crate::caddy::managed_import_pattern(dir)
}

/// Match a caddy-style glob against a path. Only `*` and `?` are honoured —
/// that is the whole of what `managed_import_pattern` emits.
fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut retry = 0usize;
    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star = Some(pi);
            retry = ti;
            pi += 1;
        } else if let Some(star_index) = star {
            pi = star_index + 1;
            retry += 1;
            ti = retry;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}

fn has_wildcard(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?')
}

/// Caddyfile tokens may be quoted (`import "katban.conf"`, `import \`x\``).
fn unquote(token: &str) -> &str {
    for quote in ['"', '`'] {
        if token.len() >= 2 && token.starts_with(quote) && token.ends_with(quote) {
            return &token[1..token.len() - 1];
        }
    }
    token
}

/// Check the `.path` unit that reloads caddy when the managed include changes.
/// Without it a fresh `expose` never reaches caddy until someone reloads by
/// hand, and a unit watching a *different* file than the active `--caddy-dir`
/// never fires at all. `contents` is the installed `katban-reload.path`;
/// `None` means it is not installed.
pub fn check_reloader_path_unit(contents: Option<&str>, managed_path: &Path) -> Vec<Finding> {
    const SUBJECT: &str = "katban-reload.path";
    let expected = managed_path.display().to_string();
    let Some(text) = contents else {
        return vec![Finding::new(
            Level::Warn,
            SUBJECT,
            format!(
                "not installed — caddy will not pick up new routes until someone reloads it by \
                 hand; install the rendered unit and run: sudo systemctl enable --now \
                 {SUBJECT}"
            ),
        )];
    };
    let watched: Vec<&str> = text
        .lines()
        .filter_map(|line| line.trim().strip_prefix("PathChanged="))
        .collect();
    if watched.is_empty() {
        return vec![Finding::new(
            Level::Error,
            SUBJECT,
            "installed unit has no `PathChanged=` — it can never fire",
        )];
    }
    if watched.iter().any(|path| *path == expected) {
        return vec![Finding::new(
            Level::Ok,
            SUBJECT,
            format!("watches {expected}"),
        )];
    }
    vec![Finding::new(
        Level::Error,
        SUBJECT,
        format!(
            "watches {} but this caddy include is {expected} — the include changes without caddy \
             reloading; re-run the `expose` that installs the watcher, or fix `PathChanged=`",
            watched.join(", ")
        ),
    )]
}

/// Where an exposed site's caddy block sends traffic.
#[derive(Debug, Clone, Copy)]
pub enum SiteTarget<'a> {
    /// A live (unlocked) site: caddy proxies to Katban's loopback handler.
    Proxy { port: u16 },
    /// A locked (published) site: caddy serves the folder itself.
    Static { root: &'a Path },
}

/// An exposed site whose backend is gone still has a perfectly valid caddy
/// config, so nothing else notices: visitors get a 502 from a dead proxy or a
/// 404 from a missing static root. `healthy` is "something is listening on the
/// proxy port" / "the static root exists".
pub fn check_site(name: &str, target: SiteTarget<'_>, healthy: bool) -> Finding {
    let subject = format!("site {name}");
    match target {
        SiteTarget::Proxy { port } => {
            if healthy {
                Finding::new(Level::Ok, subject, format!("127.0.0.1:{port} is up"))
            } else {
                Finding::new(
                    Level::Error,
                    subject,
                    format!(
                        "nothing listening on 127.0.0.1:{port} — visitors get 502; start it with \
                         `clawde katban site serve {name}`"
                    ),
                )
            }
        }
        SiteTarget::Static { root } => {
            if healthy {
                Finding::new(
                    Level::Ok,
                    subject,
                    format!("serves {} from disk", root.display()),
                )
            } else {
                Finding::new(
                    Level::Warn,
                    subject,
                    format!(
                        "root {} is missing — caddy serves 404s for this site",
                        root.display()
                    ),
                )
            }
        }
    }
}

/// Check an installed unit: present, has an `ExecStart`, and that unit's
/// binary actually exists. A unit pinned to a debug build (which `cargo
/// clean` removes and every rebuild replaces) is flagged as a warning even
/// though the file exists today. `contents` is the unit file text; `None`
/// means the unit is not installed.
pub fn check_service_unit(name: &str, contents: Option<&str>) -> Vec<Finding> {
    let subject = format!("{name}.service");
    let Some(text) = contents else {
        return vec![Finding::new(Level::Warn, subject, "not installed")];
    };
    let Some(exec) = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("ExecStart="))
    else {
        return vec![Finding::new(
            Level::Error,
            subject,
            "installed unit has no ExecStart",
        )];
    };
    // `ExecStart=-/path ...` — a leading `-` means "ignore non-zero exit".
    let exec = exec.trim_start_matches('-');
    let Some(binary) = exec.split_whitespace().next() else {
        return vec![Finding::new(Level::Error, subject, "ExecStart is empty")];
    };
    // A bare command name (`systemctl reload caddy`) is resolved via PATH
    // when the unit starts, so there is nothing on disk to validate; only a
    // path-like token can be checked here.
    if !binary.contains('/') {
        return vec![Finding::new(
            Level::Ok,
            subject,
            format!("installed, ExecStart runs `{binary}` via PATH"),
        )];
    }
    // A relative token is resolved by systemd against the unit's
    // `WorkingDirectory`, not against whatever directory ran `doctor`.
    // `join` keeps a leading `./`, and the message should name the path
    // systemd would actually execute.
    let resolved = if binary.starts_with('/') {
        PathBuf::from(binary)
    } else {
        working_directory(text).join(binary.strip_prefix("./").unwrap_or(binary))
    };
    if !resolved.exists() {
        return vec![Finding::new(
            Level::Error,
            subject,
            format!(
                "ExecStart binary is missing: {} — rebuild or re-run the `expose` that rendered it",
                resolved.display()
            ),
        )];
    }
    if resolved
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "debug")
    {
        return vec![Finding::new(
            Level::Warn,
            subject,
            format!(
                "pinned to a debug build ({}); `cargo clean` or a rebuild will break it — \
                 re-run the `expose` from a release build",
                resolved.display()
            ),
        )];
    }
    vec![Finding::new(
        Level::Ok,
        subject,
        "installed, ExecStart binary present",
    )]
}

/// A unit's `WorkingDirectory=`, which systemd resolves relative `ExecStart`
/// paths against. Defaults to `/`, as systemd does.
fn working_directory(unit: &str) -> PathBuf {
    unit.lines()
        .find_map(|line| line.trim().strip_prefix("WorkingDirectory="))
        .map(|dir| dir.trim_start_matches('-'))
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// The value of a `--flag VALUE` / `--flag=VALUE` option in a unit's
/// `ExecStart`, when it passes one. `None` means the unit leaves the choice to
/// its program's own default.
pub fn unit_flag_value(contents: &str, flag: &str) -> Option<String> {
    let exec = contents
        .lines()
        .find_map(|line| line.trim().strip_prefix("ExecStart="))?;
    let equals = format!("{flag}=");
    let mut words = exec.split_whitespace();
    while let Some(word) = words.next() {
        if let Some(value) = word.strip_prefix(&equals) {
            return Some(value.to_string());
        }
        if word == flag {
            return words.next().map(str::to_string);
        }
    }
    None
}

/// The `--port N` / `--port=N` an installed unit's `ExecStart` passes, when it
/// passes one. `None` means the server falls back to its own default port.
pub fn unit_port_flag(contents: &str) -> Option<u16> {
    unit_flag_value(contents, "--port")?.parse().ok()
}

/// The reload watcher must run the **checked** reload, against the same caddy
/// dir the rest of `doctor` is looking at.
///
/// The unit is installed by hand from a rendered file, so a host that
/// bootstrapped before the gate existed keeps applying whatever is on disk: a
/// fragment that stopped adapting then fails every later reload, and nothing
/// else reports why. A unit with no `ExecStart` is left alone here —
/// `check_service_unit` already errors on that.
pub fn check_reloader_service_unit(contents: Option<&str>, caddy_dir: &Path) -> Vec<Finding> {
    const SUBJECT: &str = "katban-reload gate";
    let Some(text) = contents else {
        return Vec::new();
    };
    let Some(exec) = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("ExecStart="))
    else {
        return Vec::new();
    };
    if !exec.contains("katban caddy reload") {
        return vec![Finding::new(
            Level::Warn,
            SUBJECT,
            format!(
                "the installed watcher runs `{}` — it reloads whatever is on disk without \
                 validating; re-run the `expose` that installs it to render the checked watcher",
                exec.trim()
            ),
        )];
    }
    // No flag means the subcommand's own default, which is the default caddy
    // dir — `--caddy-dir` with no value is a parse error there, not a fallback.
    let checked = unit_flag_value(text, "--caddy-dir").unwrap_or_else(|| "/etc/caddy".to_string());
    let expected = caddy_dir.display().to_string();
    if checked == expected {
        return vec![Finding::new(
            Level::Ok,
            SUBJECT,
            format!("validates {expected} before reloading"),
        )];
    }
    vec![Finding::new(
        Level::Warn,
        SUBJECT,
        format!(
            "the watcher validates --caddy-dir {checked}, but this managed include is in \
             {expected} — it gates a different Caddyfile than the one being served"
        ),
    )]
}

/// The installed role unit and its caddy block are rendered at different times
/// and the install step is a human copy, so they can disagree about the port —
/// in which case the route is a 502 that nothing else reports.
pub fn check_unit_port(unit: &str, served: u16, proxied: u16) -> Finding {
    let subject = format!("{unit} port");
    if served == proxied {
        Finding::new(
            Level::Ok,
            subject,
            format!("serves {served}, caddy proxies {proxied}"),
        )
    } else {
        Finding::new(
            Level::Error,
            subject,
            format!(
                "serves {served} but its caddy block proxies to {proxied} — that route is a 502; \
                 re-run the `expose` for this role and reinstall the unit"
            ),
        )
    }
}

/// Whether caddy itself is up. `load_state`/`active_state` come from
/// `systemctl show caddy.service -p LoadState -p ActiveState --value`.
///
/// `None` when there is no caddy unit to ask about: the reverse proxy may be
/// run some other way (a container, a snap, another unit name), and guessing
/// would turn a working host into a false alarm.
pub fn check_caddy_service(load_state: &str, active_state: &str) -> Option<Finding> {
    const SUBJECT: &str = "caddy.service";
    let load_state = load_state.trim();
    if load_state.is_empty() || load_state == "not-found" {
        return None;
    }
    Some(match active_state.trim() {
        "active" | "activating" | "reloading" => Finding::new(Level::Ok, SUBJECT, "active"),
        // A unit that is loaded but stopped is as often deliberate
        // (maintenance, another proxy) as broken, so it warns; a *failed* unit
        // is broken and errors.
        "failed" => Finding::new(
            Level::Error,
            SUBJECT,
            "failed — every published route is down; check `journalctl -u caddy`",
        ),
        other => Finding::new(
            Level::Warn,
            SUBJECT,
            format!(
                "{other} — every published route is down until caddy is running; if that is not \
                 deliberate: sudo systemctl start caddy"
            ),
        ),
    })
}

/// A configured role (Cat Chat / admin board) with nothing listening on its
/// loopback port is a 502 waiting for the next visitor.
pub fn check_listener(role: &str, port: u16, listening: bool) -> Finding {
    let subject = format!("{role} listener");
    if listening {
        Finding::new(Level::Ok, subject, format!("127.0.0.1:{port} is up"))
    } else {
        Finding::new(
            Level::Error,
            subject,
            format!("nothing listening on 127.0.0.1:{port} — start the server"),
        )
    }
}

/// The most severe level present, or `None` when there are no findings.
pub fn worst(findings: &[Finding]) -> Option<Level> {
    findings
        .iter()
        .map(|f| f.level)
        .reduce(|a, b| match (a, b) {
            (Level::Error, _) | (_, Level::Error) => Level::Error,
            (Level::Warn, _) | (_, Level::Warn) => Level::Warn,
            _ => Level::Ok,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_include_missing_is_a_warning() {
        let f = check_managed_include(None, "# x");
        assert_eq!(f.level, Level::Warn);
        assert!(f.message.contains("missing"));
    }

    #[test]
    fn managed_include_equal_is_ok_and_differing_is_a_warning() {
        assert_eq!(check_managed_include(Some("# x"), "# x").level, Level::Ok);
        let drift = check_managed_include(Some("# old"), "# new");
        assert_eq!(drift.level, Level::Warn);
        assert!(drift.message.contains("differs"));
    }

    #[test]
    fn specific_conf_import_is_flagged_but_glob_and_snippet_are_not() {
        let managed = Path::new("/etc/caddy/katban.conf");
        let findings = check_imports(
            "{\n\tacme_dns duckdns x\n}\n\nimport katban.conf\nimport sites-enabled/*\nimport cors\n",
            managed,
        );
        assert_eq!(findings.len(), 1, "only the specific .conf import warns");
        assert_eq!(findings[0].level, Level::Warn);
        assert!(findings[0].message.contains("import katban.conf"));

        // The safe glob form is silent.
        assert!(check_imports("import katban*.conf\n", managed).is_empty());
        // An absolute glob matching the include is silent too.
        assert!(check_imports("import /etc/caddy/katban*.conf\n", managed).is_empty());
        // A comment mentioning the import loads nothing, so it is the
        // coverage failure — not the fragility warning.
        let commented = check_imports("# import katban.conf\n", managed);
        assert_eq!(commented.len(), 1);
        assert!(commented[0].message.contains("never serves these routes"));
    }

    #[test]
    fn a_quoted_import_is_unquoted_before_it_is_judged() {
        let managed = Path::new("/etc/caddy/katban.conf");
        // A quoted *specific* file is still the fragile form.
        let findings = check_imports("import \"katban.conf\"\n", managed);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("names a specific file"));

        // A quoted glob counts as loading the include.
        assert!(check_imports("import \"katban*.conf\"\n", managed).is_empty());
        assert!(check_imports("import `katban*.conf`\n", managed).is_empty());
    }

    #[test]
    fn a_caddyfile_that_never_imports_the_include_is_flagged() {
        let managed = Path::new("/etc/caddy/katban.conf");
        let findings = check_imports(
            "{\n\tacme_dns duckdns x\n}\n\nimport sites-enabled/*\nimport cors\n",
            managed,
        );
        assert_eq!(findings.len(), 1, "unrelated imports are not enough");
        assert_eq!(findings[0].level, Level::Warn);
        assert!(findings[0].message.contains("never serves these routes"));
        assert!(findings[0]
            .message
            .contains("import /etc/caddy/katban*.conf"));

        // An empty or absent import section is the same failure.
        assert_eq!(check_imports("", managed).len(), 1);
        // A custom caddy dir names its own glob.
        let custom = check_imports("", Path::new("/home/user/caddy/katban.conf"));
        assert!(custom[0].message.contains("/home/user/caddy/katban*.conf"));
    }

    #[test]
    fn glob_matching_handles_the_patterns_we_emit() {
        assert!(glob_matches("katban*.conf", "katban.conf"));
        assert!(glob_matches("katban*.conf", "katban-sites.conf"));
        assert!(!glob_matches("katban*.conf", "katban.conf.tmp"));
        assert!(glob_matches("a?c", "abc"));
        assert!(!glob_matches("a?c", "abbc"));
        assert!(glob_matches("/etc/caddy/*", "/etc/caddy/katban.conf"));
    }

    #[test]
    fn service_unit_checks_installed_execstart_and_binary() {
        // Not installed -> warn.
        assert_eq!(check_service_unit("catchat", None)[0].level, Level::Warn);

        // No ExecStart -> error.
        let bad = check_service_unit("catchat", Some("[Service]\nType=simple\n"));
        assert_eq!(bad[0].level, Level::Error);

        // ExecStart pointing at a nonexistent binary -> error.
        let missing = check_service_unit(
            "catchat",
            Some("[Service]\nExecStart=/nope/clawde catchat serve\n"),
        );
        assert_eq!(missing[0].level, Level::Error);
        assert!(missing[0].message.contains("/nope/clawde"));
    }

    #[test]
    fn service_unit_flags_a_debug_binary_it_can_see() {
        let dir = tempfile::tempdir().unwrap();
        let debug_dir = dir.path().join("debug");
        std::fs::create_dir_all(&debug_dir).unwrap();
        let binary = debug_dir.join("clawde");
        std::fs::write(&binary, "#!/bin/true\n").unwrap();
        let unit = format!("[Service]\nExecStart={} catchat serve\n", binary.display());
        let findings = check_service_unit("catchat", Some(&unit));
        assert_eq!(findings[0].level, Level::Warn);
        assert!(findings[0].message.contains("debug build"));
    }

    #[test]
    fn service_unit_accepts_a_bare_path_command() {
        // A unit may run a bare command name, which systemd resolves through
        // its own PATH — not a file `doctor` can check, so it must not be
        // reported as a missing binary.
        let findings = check_service_unit(
            "reload-helper",
            Some("[Service]\nExecStart=systemctl reload caddy\n"),
        );
        assert_eq!(findings[0].level, Level::Ok);
        assert!(findings[0].message.contains("via PATH"));
    }

    #[test]
    fn service_unit_resolves_a_relative_binary_against_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("app");
        std::fs::create_dir_all(work.join("bin")).unwrap();
        std::fs::write(work.join("bin/clawde"), "#!/bin/true\n").unwrap();

        // A relative token is resolved against the unit's WorkingDirectory —
        // not against the directory doctor happened to be run from.
        let present = check_service_unit(
            "catchat",
            Some(&format!(
                "[Service]\nWorkingDirectory={}\nExecStart=./bin/clawde catchat serve\n",
                work.display()
            )),
        );
        assert_eq!(present[0].level, Level::Ok, "{present:#?}");

        // Missing -> the error names the *resolved* path, so the operator can
        // see what systemd would have executed.
        let absent = check_service_unit(
            "catchat",
            Some(&format!(
                "[Service]\nWorkingDirectory={}\nExecStart=./gone/clawde catchat serve\n",
                work.display()
            )),
        );
        assert_eq!(absent[0].level, Level::Error);
        assert!(absent[0]
            .message
            .contains(&work.join("gone/clawde").display().to_string()));

        // No WorkingDirectory -> systemd's default of `/`.
        let rooted = check_service_unit("catchat", Some("[Service]\nExecStart=./clawde\n"));
        assert_eq!(rooted[0].level, Level::Error);
        assert!(rooted[0].message.contains("/clawde"));
    }

    #[test]
    fn unit_port_flag_reads_both_spellings() {
        assert_eq!(
            unit_port_flag("ExecStart=/x/clawde catchat serve --port 8789\n"),
            Some(8789)
        );
        assert_eq!(
            unit_port_flag("ExecStart=/x/clawde catchat serve --port=9000\n"),
            Some(9000)
        );
        // No flag -> the server's own default applies, which the caller owns.
        assert_eq!(unit_port_flag("ExecStart=/x/clawde catchat serve\n"), None);
        assert_eq!(unit_port_flag("[Service]\nType=simple\n"), None);
        // A non-numeric value is not a port we can compare.
        assert_eq!(unit_port_flag("ExecStart=x --port nope\n"), None);
    }

    #[test]
    fn unit_flag_value_reads_a_string_flag() {
        assert_eq!(
            unit_flag_value(
                "ExecStart=/x/clawde katban caddy reload --caddy-dir /srv/caddy\n",
                "--caddy-dir"
            ),
            Some("/srv/caddy".to_string())
        );
        assert_eq!(
            unit_flag_value(
                "ExecStart=/x/clawde katban caddy reload --caddy-dir=/srv\n",
                "--caddy-dir"
            ),
            Some("/srv".to_string())
        );
        assert_eq!(
            unit_flag_value("ExecStart=/x/clawde katban caddy reload\n", "--caddy-dir"),
            None
        );
        assert_eq!(
            unit_flag_value("[Service]\nType=oneshot\n", "--caddy-dir"),
            None
        );
    }

    #[test]
    fn reloader_gate_requires_a_validating_watcher_for_this_caddy_dir() {
        let managed_dir = Path::new("/srv/caddy");
        // Not installed -> the unit check reports that; this one stays quiet.
        assert!(check_reloader_service_unit(None, managed_dir).is_empty());
        // No ExecStart -> same division of labour.
        assert!(
            check_reloader_service_unit(Some("[Service]\nType=oneshot\n"), managed_dir).is_empty()
        );

        // The pre-gate shape: applies whatever is on disk.
        let unchecked = check_reloader_service_unit(
            Some("[Service]\nExecStart=systemctl reload caddy\n"),
            managed_dir,
        );
        assert_eq!(unchecked.len(), 1);
        assert_eq!(unchecked[0].subject, "katban-reload gate");
        assert_eq!(unchecked[0].level, Level::Warn);
        assert!(unchecked[0].message.contains("without validating"));

        // The checked shape, for the dir doctor is looking at.
        let checked = check_reloader_service_unit(
            Some("[Service]\nExecStart=/usr/local/bin/clawde katban caddy reload --caddy-dir /srv/caddy\n"),
            managed_dir,
        );
        assert_eq!(checked[0].level, Level::Ok);
        assert!(checked[0].message.contains("/srv/caddy"));

        // Checked, but gating another directory's Caddyfile.
        let elsewhere = check_reloader_service_unit(
            Some("[Service]\nExecStart=/usr/local/bin/clawde katban caddy reload --caddy-dir /etc/caddy\n"),
            managed_dir,
        );
        assert_eq!(elsewhere[0].level, Level::Warn);
        assert!(elsewhere[0].message.contains("/etc/caddy"));
        assert!(elsewhere[0].message.contains("/srv/caddy"));

        // No flag: the subcommand's default is /etc/caddy, which matches there
        // and drifts anywhere else.
        let defaulted = check_reloader_service_unit(
            Some("[Service]\nExecStart=/usr/local/bin/clawde katban caddy reload\n"),
            Path::new("/etc/caddy"),
        );
        assert_eq!(defaulted[0].level, Level::Ok);
        let drifted = check_reloader_service_unit(
            Some("[Service]\nExecStart=/usr/local/bin/clawde katban caddy reload\n"),
            managed_dir,
        );
        assert_eq!(drifted[0].level, Level::Warn);
        assert!(drifted[0].message.contains("/etc/caddy"));
    }

    #[test]
    fn unit_port_drift_is_an_error() {
        let same = check_unit_port("catchat.service", 8789, 8789);
        assert_eq!(same.level, Level::Ok);
        let drift = check_unit_port("catchat.service", 9000, 8789);
        assert_eq!(drift.subject, "catchat.service port");
        assert_eq!(drift.level, Level::Error);
        assert!(drift.message.contains("9000"));
        assert!(drift.message.contains("8789"));
    }

    #[test]
    fn caddy_service_state_is_graded_without_guessing() {
        // No caddy unit on this host -> not a finding; the proxy may be run
        // some other way.
        assert!(check_caddy_service("not-found", "inactive").is_none());
        assert!(check_caddy_service("", "").is_none());

        assert_eq!(
            check_caddy_service("loaded", "active").unwrap().level,
            Level::Ok
        );
        assert_eq!(
            check_caddy_service("loaded", "reloading").unwrap().level,
            Level::Ok
        );
        // Loaded but stopped warns (often deliberate); failed errors.
        let stopped = check_caddy_service("loaded", "inactive").unwrap();
        assert_eq!(stopped.level, Level::Warn);
        assert!(stopped.message.contains("every published route is down"));
        assert_eq!(
            check_caddy_service("loaded", "failed").unwrap().level,
            Level::Error
        );
    }

    #[test]
    fn reloader_path_unit_must_watch_the_active_include() {
        let managed = Path::new("/etc/caddy/katban.conf");
        // Not installed -> warn.
        assert_eq!(
            check_reloader_path_unit(None, managed)[0].level,
            Level::Warn
        );

        // Installed but inert -> error.
        let inert = check_reloader_path_unit(Some("[Path]\nPathExists=/x\n"), managed);
        assert_eq!(inert[0].level, Level::Error);
        assert!(inert[0].message.contains("never fire"));

        // Watching the include -> ok.
        let good = check_reloader_path_unit(
            Some("[Path]\nPathChanged=/etc/caddy/katban.conf\n"),
            managed,
        );
        assert_eq!(good[0].level, Level::Ok);

        // Watching a *different* include (a stale --caddy-dir) -> error, and
        // the message names both paths.
        let stale = check_reloader_path_unit(
            Some("[Path]\nPathChanged=/etc/caddy/old-katban.conf\n"),
            Path::new("/srv/caddy/katban.conf"),
        );
        assert_eq!(stale[0].level, Level::Error);
        assert!(stale[0].message.contains("/etc/caddy/old-katban.conf"));
        assert!(stale[0].message.contains("/srv/caddy/katban.conf"));

        // Several watched paths: any match is enough.
        let multi = check_reloader_path_unit(
            Some("[Path]\nPathChanged=/other/x\nPathChanged=/etc/caddy/katban.conf\n"),
            managed,
        );
        assert_eq!(multi[0].level, Level::Ok);
    }

    #[test]
    fn site_routes_are_graded_by_backend() {
        // A live site with a dead proxy is an error naming the port.
        let down = check_site("demo", SiteTarget::Proxy { port: 8788 }, false);
        assert_eq!(down.subject, "site demo");
        assert_eq!(down.level, Level::Error);
        assert!(down.message.contains("8788"));
        assert!(down.message.contains("katban site serve demo"));

        assert_eq!(
            check_site("demo", SiteTarget::Proxy { port: 8788 }, true).level,
            Level::Ok
        );

        // A static site whose root is gone still serves, badly -> warn.
        let root = Path::new("/srv/site");
        let missing = check_site("locked", SiteTarget::Static { root }, false);
        assert_eq!(missing.level, Level::Warn);
        assert!(missing.message.contains("/srv/site"));
        assert_eq!(
            check_site("locked", SiteTarget::Static { root }, true).level,
            Level::Ok
        );
    }

    #[test]
    fn listener_reflects_the_port_state() {
        assert_eq!(check_listener("cat chat", 8789, true).level, Level::Ok);
        let down = check_listener("cat chat", 8789, false);
        assert_eq!(down.level, Level::Error);
        assert!(down.message.contains("8789"));
    }

    #[test]
    fn worst_reduces_to_the_most_severe() {
        assert_eq!(worst(&[]), None);
        let ok = check_listener("x", 1, true);
        let warn = check_managed_include(None, "# x");
        let err = check_listener("x", 1, false);
        assert_eq!(worst(std::slice::from_ref(&ok)), Some(Level::Ok));
        assert_eq!(worst(&[ok.clone(), warn.clone()]), Some(Level::Warn));
        assert_eq!(worst(&[warn, err]), Some(Level::Error));
    }
}
