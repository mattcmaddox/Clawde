// `clawde katban` — the self-hosted web surface for Clawde.
//
// Hosted-sites slice (spec §10): add/list/show/remove sites, serve a folder
// (or a registered site by name) on loopback with live reload, and `expose`
// a site through caddy via the owned include file + one-time bootstrap.
// Board, guest tiers, and auth arrive in later slices.

use anyhow::Context;
use std::path::{Path, PathBuf};

pub(crate) const USAGE: &str = r#"Usage: clawde katban <command> [OPTIONS]

Self-hosted web surface for Clawde (v0: dev-site hosting with live reload).

Commands:
  site add <DIR> [--name NAME] [--port N] [--public-subdomain HOST] [--locked]
  site list                                   List hosted sites
  site show <NAME>                            Show a site and its caddy block
  site remove <NAME>                          Stop hosting a site
  site serve <NAME|DIR> [--port N] [--host IP] [--allow-non-loopback] [--no-reload] [--locked]
  site expose <NAME> [--dry-run] [--caddy-dir DIR] [--subdomain HOST]
                [--kind static|live] [--duckdns-token TOKEN]
  board ...                                   Kanban-style task board (cards, links)
  project ...                                 Register a board to a git repo (for running)
  link ...                                    Guest links (share a chat URL with friends)
  guest serve [--port N] [--host IP] [--allow-non-loopback]
                                              Run the guest chat server
  status                                      Overview of sites, boards, caddy config
  help                                        Show this help

Defaults: state lives in ~/.clawde/katban/ (CLAWDE_HOME overrides); sites
and the guest server serve on 127.0.0.1. Binding a non-loopback address
requires --allow-non-loopback.
"#;

const SITE_USAGE: &str = r#"Usage: clawde katban site <command> [OPTIONS]

  site add <DIR> [--name NAME] [--port N] [--public-subdomain HOST] [--locked]
  site list
  site show <NAME>
  site remove <NAME>
  site serve <NAME|DIR> [--port N] [--host IP] [--allow-non-loopback] [--no-reload] [--locked]
  site expose <NAME> [--dry-run] [--caddy-dir DIR] [--subdomain HOST]
                [--kind static|live] [--duckdns-token TOKEN]

Board commands:
  board init [--project NAME]
  board card add <PROMPT> [--project NAME]
  board card list [--project NAME]
  board card set <ID> <backlog|queued|running|blocked|review|failed|done> [--project NAME]
  board card merge <ID> [--project NAME]   (merge a review card into the project)
  board card remove <ID> [--project NAME]  (archive; discards its pinned branch)
  board card show <ID> [--project NAME]    (status, result, commit, diff)
  board link <A> <B> [--project NAME]      (B must finish before A starts)
  board unlink <A> <B> [--project NAME]
  board ready [--project NAME] [--cap N]   (cards that can start now)
  board run [--project NAME]                Run the scheduler (executes ready cards)
  project set <NAME> <DIR>                  Register NAME's board to a git repo for running
  project list                              Show project -> repo mappings
"#;

pub async fn run_command(args: &[String]) -> anyhow::Result<()> {
    let Some(command) = args.first().map(|s| s.as_str()) else {
        print!("{USAGE}");
        return Ok(());
    };
    match command {
        "site" => run_site(&args[1..]).await,
        "board" => run_board(&args[1..]).await,
        "project" => run_project(&args[1..]),
        "link" => run_link(&args[1..]),
        "guest" => run_guest(&args[1..]).await,
        "status" => run_status(),
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        other => anyhow::bail!("unknown katban command: {other}\n\n{USAGE}"),
    }
}

#[derive(Debug, Default)]
struct SiteOpts {
    dir: Option<String>,
    name: Option<String>,
    port: Option<u16>,
    public_subdomain: Option<String>,
    locked: bool,
    no_reload: bool,
    dry_run: bool,
    caddy_dir: Option<PathBuf>,
    kind: Option<String>,
    host: Option<String>,
    allow_non_loopback: bool,
    duckdns_token: Option<String>,
}

fn parse_site_args(args: &[String], allow_expose_flags: bool) -> anyhow::Result<SiteOpts> {
    let mut opts = SiteOpts::default();
    let mut positionals = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        let mut take_value = |name: &str| -> anyhow::Result<String> {
            index += 1;
            args.get(index)
                .with_context(|| format!("{name} needs a value"))
                .cloned()
        };
        match flag {
            "--port" => {
                opts.port = Some(
                    take_value("--port")?
                        .parse()
                        .context("--port must be a number")?,
                )
            }
            "--name" => opts.name = Some(take_value("--name")?),
            "--public-subdomain" => opts.public_subdomain = Some(take_value("--public-subdomain")?),
            "--no-reload" => opts.no_reload = true,
            "--locked" => opts.locked = true,
            "--host" => opts.host = Some(take_value("--host")?),
            "--allow-non-loopback" => opts.allow_non_loopback = true,
            "--duckdns-token" if allow_expose_flags => {
                opts.duckdns_token = Some(take_value("--duckdns-token")?);
            }
            "--dry-run" if allow_expose_flags => opts.dry_run = true,
            "--caddy-dir" if allow_expose_flags => {
                opts.caddy_dir = Some(PathBuf::from(take_value("--caddy-dir")?));
            }
            "--subdomain" if allow_expose_flags => {
                opts.public_subdomain = Some(take_value("--subdomain")?);
            }
            "--kind" if allow_expose_flags => {
                opts.kind = Some(take_value("--kind")?);
            }
            flag if flag.starts_with("--port=") => {
                opts.port = Some(
                    flag["--port=".len()..]
                        .parse()
                        .context("--port must be a number")?,
                );
            }
            flag if flag.starts_with("--name=") => {
                opts.name = Some(flag["--name=".len()..].to_string());
            }
            flag if flag.starts_with("--public-subdomain=") => {
                opts.public_subdomain = Some(flag["--public-subdomain=".len()..].to_string());
            }
            flag if flag.starts_with("--subdomain=") && allow_expose_flags => {
                opts.public_subdomain = Some(flag["--subdomain=".len()..].to_string());
            }
            flag if flag.starts_with("--host=") => {
                opts.host = Some(flag["--host=".len()..].to_string());
            }
            flag if flag.starts_with("--duckdns-token=") && allow_expose_flags => {
                opts.duckdns_token = Some(flag["--duckdns-token=".len()..].to_string());
            }
            flag if flag.starts_with("--caddy-dir=") && allow_expose_flags => {
                opts.caddy_dir = Some(PathBuf::from(flag["--caddy-dir=".len()..].to_string()));
            }
            flag if flag.starts_with("--kind=") && allow_expose_flags => {
                opts.kind = Some(flag["--kind=".len()..].to_string());
            }
            flag if flag.starts_with('-') => {
                anyhow::bail!("unknown option: {flag}\n\n{SITE_USAGE}");
            }
            positional => positionals.push(positional.to_string()),
        }
        index += 1;
    }
    opts.dir = positionals.first().cloned();
    Ok(opts)
}

async fn run_site(args: &[String]) -> anyhow::Result<()> {
    let Some(subcommand) = args.first().map(|s| s.as_str()) else {
        print!("{SITE_USAGE}");
        return Ok(());
    };
    match subcommand {
        "add" => {
            let opts = parse_site_args(&args[1..], false)?;
            site_add(opts)
        }
        "list" => site_list(),
        "show" => {
            let opts = parse_site_args(&args[1..], false)?;
            site_show(opts)
        }
        "remove" => {
            let opts = parse_site_args(&args[1..], false)?;
            site_remove(opts)
        }
        "serve" => {
            let opts = parse_site_args(&args[1..], false)?;
            site_serve(opts).await
        }
        "expose" => {
            let opts = parse_site_args(&args[1..], true)?;
            site_expose(opts).await
        }
        "help" | "--help" | "-h" => {
            print!("{SITE_USAGE}");
            Ok(())
        }
        other => anyhow::bail!("unknown katban site command: {other}\n\n{SITE_USAGE}"),
    }
}

fn find_site<'a>(
    config: &'a clawde_katban::config::KatbanConfig,
    name: &str,
) -> Option<&'a clawde_katban::config::SiteConfig> {
    config.sites.iter().find(|site| site.name == name)
}

fn site_add(opts: SiteOpts) -> anyhow::Result<()> {
    use clawde_katban::config::{canonical_site_root, load, save, SiteConfig};

    let dir = opts
        .dir
        .as_deref()
        .context("site add needs a directory: clawde katban site add <DIR>")?;
    let root = canonical_site_root(Path::new(dir))?;
    let name = opts.name.unwrap_or_else(|| {
        root.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "site".to_string())
    });
    // The name doubles as the caddy hostname fallback; an injected value here
    // breaks the whole managed config at reload (see caddy::valid_hostname).
    check_hostname("site name", &name)?;
    if let Some(subdomain) = &opts.public_subdomain {
        check_hostname("public subdomain", subdomain)?;
    }
    let port = opts
        .port
        .unwrap_or(clawde_katban::config::DEFAULT_SITE_PORT);

    let mut config = load()?;
    if find_site(&config, &name).is_some() {
        anyhow::bail!("a site named '{name}' already exists");
    }
    config.sites.push(SiteConfig {
        name: name.clone(),
        root: root.clone(),
        port,
        public_subdomain: opts.public_subdomain,
        locked: opts.locked,
    });
    save(&config)?;
    println!(
        "added site '{name}' ({}) on port {port} — serve with: clawde katban site serve {name}",
        root.display()
    );
    Ok(())
}

fn site_list() -> anyhow::Result<()> {
    use clawde_katban::config::load;

    let config = load()?;
    if config.sites.is_empty() {
        println!("no sites yet — add one with: clawde katban site add <DIR>");
        return Ok(());
    }
    for site in &config.sites {
        let subdomain = site
            .public_subdomain
            .as_deref()
            .map(|s| format!("  {s}"))
            .unwrap_or_default();
        let state = if site.locked { "locked" } else { "live" };
        println!(
            "{:<20} {}  port {:<5} [{state}]{}",
            site.name,
            site.root.display(),
            site.port,
            subdomain
        );
    }
    Ok(())
}

fn site_show(opts: SiteOpts) -> anyhow::Result<()> {
    use clawde_katban::caddy::{render_block, site_kind};
    use clawde_katban::config::load;

    let name = opts
        .name
        .or(opts.dir)
        .context("site show needs a name: clawde katban site show <NAME>")?;
    let config = load()?;
    let site = find_site(&config, &name).with_context(|| format!("no site named '{name}'"))?;
    let state = if site.locked {
        "locked (published, no live reload)"
    } else {
        "live (reloads on save)"
    };
    println!("name:        {}", site.name);
    println!("root:        {}", site.root.display());
    println!("port:        {}", site.port);
    println!(
        "subdomain:   {}",
        site.public_subdomain.as_deref().unwrap_or("(none)")
    );
    println!("state:       {state}");
    if site.public_subdomain.is_some() {
        println!("caddy block (managed):");
        for line in render_block(site, &site_kind(site)).lines() {
            println!("    {line}");
        }
    }
    Ok(())
}

fn site_remove(opts: SiteOpts) -> anyhow::Result<()> {
    use clawde_katban::config::load;

    let name = opts
        .name
        .or(opts.dir)
        .context("site remove needs a name: clawde katban site remove <NAME>")?;
    let mut config = load()?;
    let original_len = config.sites.len();
    config.sites.retain(|site| site.name != name);
    if config.sites.len() == original_len {
        anyhow::bail!("no site named '{name}'");
    }
    clawde_katban::config::save(&config)?;
    println!("removed site '{name}'");
    // The managed caddy config still lists removed sites until something
    // re-renders it — point the admin at the cheapest regeneration.
    let remaining: Vec<_> = config
        .sites
        .iter()
        .filter(|site| site.public_subdomain.is_some())
        .collect();
    if let Some(site) = remaining.first() {
        println!(
            "note: the managed caddy config still lists removed sites — re-run 'clawde katban site expose {}' to regenerate it",
            site.name
        );
    } else if clawde_katban::guest::load().is_ok_and(|store| store.public_subdomain.is_some()) {
        println!("note: run 'clawde katban guest expose' to regenerate the managed caddy config without this site");
    }
    Ok(())
}

/// Resolve `serve`'s argument: a registered site name, else a directory.
/// Returns (root, port, live).
fn resolve_serve_target(
    config: &clawde_katban::config::KatbanConfig,
    arg: &str,
    port_override: Option<u16>,
    no_reload: bool,
    locked_override: bool,
) -> anyhow::Result<(PathBuf, u16, bool)> {
    if let Some(site) = find_site(config, arg) {
        let live = !(site.locked || no_reload || locked_override);
        return Ok((site.root.clone(), port_override.unwrap_or(site.port), live));
    }
    let root = clawde_katban::config::canonical_site_root(Path::new(arg))?;
    let port = port_override.unwrap_or(clawde_katban::config::DEFAULT_SITE_PORT);
    Ok((root, port, !no_reload && !locked_override))
}
async fn site_serve(opts: SiteOpts) -> anyhow::Result<()> {
    use clawde_katban::config::load;
    use clawde_katban::host;

    let arg = opts
        .dir
        .clone()
        .context("site serve needs a site name or directory")?;
    let config = load()?;
    let (root, port, live) =
        resolve_serve_target(&config, &arg, opts.port, opts.no_reload, opts.locked)?;
    let name = find_site(&config, &arg)
        .map(|s| s.name.as_str())
        .unwrap_or("site");
    let addr = host::parse_bind_addr(opts.host.as_deref().unwrap_or("127.0.0.1"), port)?;
    if !host::is_loopback(addr) && !opts.allow_non_loopback {
        anyhow::bail!(
            "refusing to bind {addr} — pass --allow-non-loopback to serve beyond loopback"
        );
    }
    println!("serving '{name}' at http://{addr}/  (Ctrl-C to stop)");
    if !host::is_loopback(addr) {
        println!(
            "WARNING: bound to {addr} — reachable by other machines on this network; no auth yet."
        );
    }
    if live {
        println!("live reload: on — edit files and the page refreshes");
    } else {
        println!("live reload: off (locked or --no-reload)");
    }
    host::run_on(root, addr, live).await
}

async fn site_expose(opts: SiteOpts) -> anyhow::Result<()> {
    use clawde_katban::caddy::{
        bootstrap_instructions, render_config, site_kind, write_atomic, DEFAULT_INCLUDE_NAME,
    };
    use clawde_katban::config::load;

    if let Some(kind) = &opts.kind {
        if kind != "static" && kind != "live" {
            anyhow::bail!("--kind must be 'static' or 'live'");
        }
    }

    let name = opts
        .name
        .or(opts.dir)
        .context("site expose needs a name: clawde katban site expose <NAME>")?;
    let mut config = load()?;
    let target_index = config
        .sites
        .iter()
        .position(|site| site.name == name)
        .with_context(|| format!("no site named '{name}'"))?;

    if let Some(subdomain) = opts.public_subdomain {
        config.sites[target_index].public_subdomain = Some(subdomain);
        clawde_katban::config::save(&config)?;
    }
    let subdomain = config.sites[target_index]
        .public_subdomain
        .as_deref()
        .context("site has no subdomain — pass --subdomain HOST (e.g. demo.example.com)")?;
    check_hostname("public subdomain", subdomain)?;

    // Regenerate the managed config for every exposed site (idempotent).
    let exposed: Vec<_> = config
        .sites
        .iter()
        .filter(|site| site.public_subdomain.is_some())
        .map(|site| {
            let kind = match opts.kind.as_deref() {
                Some("static") if site.name == name => clawde_katban::caddy::SiteKind::Static {
                    root: site.root.clone(),
                },
                Some("live") if site.name == name => {
                    clawde_katban::caddy::SiteKind::Live { port: site.port }
                }
                _ => site_kind(site),
            };
            (site.clone(), kind)
        })
        .collect();

    let board = admin_board_block()?;
    let text = render_config(
        &exposed,
        None,
        board.as_ref().map(|(h, p)| (h.as_str(), *p)),
    );
    let caddy_dir = opts
        .caddy_dir
        .unwrap_or_else(|| PathBuf::from("/etc/caddy"));
    let managed_path = caddy_dir.join(DEFAULT_INCLUDE_NAME);

    // A live site 502s for visitors unless something is actually serving its
    // port — surface that at expose time instead of discovering it later.
    if let Some((_, clawde_katban::caddy::SiteKind::Live { port })) =
        exposed.iter().find(|(site, _)| site.name == name)
    {
        if !port_is_open(*port) {
            println!(
                "WARNING: '{name}' is a live site but nothing is listening on 127.0.0.1:{port} — visitors get 502 until you run: clawde katban site serve {name}"
            );
        }
    }

    println!("exposing '{name}' at https://{subdomain}");
    println!(
        "Managed caddy block (written to {}):",
        managed_path.display()
    );
    println!("{text}");

    if opts.dry_run {
        println!("[dry-run] no files were written.");
        return Ok(());
    }

    write_atomic(&managed_path, &text)?;

    // Write the systemd units next to the data so the bootstrap can install
    // them; the instructions reference these files. `katban.service` runs the
    // always-on guest chat server (systemd is the default runtime, spec §11)
    // on the guest port the store last recorded, so a custom `guest expose
    // --port` keeps working after a later `site expose` regenerates the unit.
    let guest_port = clawde_katban::guest::load()?
        .guest_port
        .unwrap_or(clawde_katban::guest_server::DEFAULT_GUEST_PORT);
    let units_dir = write_systemd_units(&caddy_dir, guest_port, (&[], 0))?;

    println!();
    println!("{}", bootstrap_instructions(&units_dir, &caddy_dir));

    // Best-effort DuckDNS subdomain creation (spec C4): warn, never fail the
    // expose, so a missing token/dashboard entry is easy to recover from.
    let token = opts.duckdns_token.or_else(|| {
        std::env::var("DUCKDNS_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
    });
    if let Some(token) = token {
        let label = clawde_katban::duckdns::duckdns_label(subdomain);
        match clawde_katban::duckdns::update_subdomain(label, &token, None).await {
            Ok(true) => println!("duckdns: pointed '{label}' at your public IP (OK)"),
            Ok(false) => println!(
                "duckdns: update for '{label}' returned KO — create the subdomain in the DuckDNS dashboard first"
            ),
            Err(error) => println!(
                "duckdns: warning — could not update '{label}': {error:#}"
            ),
        }
    } else {
        println!("duckdns: no token — pass --duckdns-token or set DUCKDNS_TOKEN to auto-create the subdomain");
    }
    Ok(())
}

const BOARD_USAGE: &str = r#"Usage: clawde katban board <command> [--project NAME]

  serve [--port N] [--host IP] [--allow-non-loopback] [--run NAME,...|all]
                                         Run the admin board web UI (loopback);
                                         --run also schedules the listed projects'
                                         cards ('all' = every registered project)
  expose [--subdomain HOST] [--port N] [--dry-run] [--caddy-dir DIR]
                [--duckdns-token TOKEN] [--run NAME,...|all]
                                         Publish the board behind caddy (https);
                                         --run renders the always-on board unit
                                         with one scheduler per listed project
  password <PASSWORD>                    Set/rotate the admin write password
  init                                   Create/ensure the board file
  card add <PROMPT>                      Add a card
  card list                              List cards with status
  card set <ID> <STATUS>                 backlog|queued|running|blocked|review|failed|done
  card merge <ID>                        Merge a review card into the project
  card remove <ID>                       Archive (discards its pinned branch)
  link <A> <B>                           B must finish before A starts (cycle-checked)
  unlink <A> <B>
  ready [--cap N]                        Cards that can start now (queue order)
  run [--project NAME]                   Run the scheduler for one project (executes ready cards)

The board UI serves on 127.0.0.1:<port> (default 8790). Binding a
non-loopback address requires --allow-non-loopback.

The admin board has two access levels: reads are open on the loopback board;
writes (add/advance/archive cards) require signing in with the admin password
set by `board password`.

To execute cards automatically, register the board to a git repo with
`clawde katban project set <NAME> <DIR>`, then run the scheduler with
`board run --project NAME`: ready cards get a git worktree and run a headless
`clawde --print` to completion (review on success, failed on a non-zero exit
with auto-retry up to the board's cap).
"#;

/// Run the admin board web UI, optionally with the card schedulers in the
/// same process (`--run NAME,...` schedules those projects; `--run all`
/// schedules every registered project now and live-joins new ones as they are
/// registered — the always-on unit's command). Mirrors `guest serve`'s flag
/// parsing and loopback guard; refuses non-loopback unless
/// `--allow-non-loopback` (the board is admin-only).
async fn board_serve(args: &[String]) -> anyhow::Result<()> {
    use clawde_katban::board_server::DEFAULT_BOARD_PORT;

    let mut port = DEFAULT_BOARD_PORT;
    let mut host = "127.0.0.1".to_string();
    let mut allow_non_loopback = false;
    let mut run_arg: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--port" => {
                port = args
                    .get(index + 1)
                    .context("--port needs a number")?
                    .parse()
                    .context("--port must be a number")?;
                index += 1;
            }
            "--host" => {
                host = args
                    .get(index + 1)
                    .context("--host needs an IP")?
                    .to_string();
                index += 1;
            }
            "--allow-non-loopback" => allow_non_loopback = true,
            "--run" => {
                let value = args
                    .get(index + 1)
                    .context("--run needs project names (or 'all')")?
                    .to_string();
                run_arg = Some(value);
                index += 1;
            }
            flag if flag.starts_with("--port=") => {
                port = flag["--port=".len()..]
                    .parse()
                    .context("--port must be a number")?;
            }
            flag if flag.starts_with("--host=") => {
                host = flag["--host=".len()..].to_string();
            }
            flag if flag.starts_with("--run=") => {
                run_arg = Some(flag["--run=".len()..].to_string());
            }
            flag => anyhow::bail!("unknown option: {flag}\n\n{BOARD_USAGE}"),
        }
        index += 1;
    }

    let addr = clawde_katban::host::parse_bind_addr(&host, port)?;
    if !clawde_katban::host::is_loopback(addr) && !allow_non_loopback {
        anyhow::bail!(
            "refusing to bind {addr} — pass --allow-non-loopback to serve beyond loopback"
        );
    }
    if let Some(arg) = &run_arg {
        let executor = std::sync::Arc::new(clawde_katban::runner::ClawdeExecutor::new());
        if arg.trim().eq_ignore_ascii_case("all") {
            // Live-join mode (`--run all`, the always-on path): resolve what's
            // registered now, warn about any that can't run, and hand the whole
            // set to the coordinator which keeps joining newly-registered
            // projects without a restart. A project with no repo is skipped by
            // `registered_projects()` (it must be `project set` first) — if the
            // set is empty we print a hint, not an error, because new
            // registrations will be picked up live.
            println!(
                "runner        scheduling all registered projects (new ones join as registered)"
            );
            let executor = executor.clone();
            tokio::spawn(async move {
                if let Err(error) = clawde_katban::runner::run_all(executor).await {
                    tracing::error!(error = %error, "board runner (all) exited");
                }
            });
        } else {
            let projects = resolve_runner_projects(arg)?;
            if projects.is_empty() {
                anyhow::bail!("no projects to schedule — '{arg}' matched none");
            }
            for project in &projects {
                if clawde_katban::projects::repo_root(project).is_none() {
                    println!(
                        "warning: no git repo registered for '{project}' — cards will run in empty scratch dirs. Register one with: clawde katban project set {project} <DIR>"
                    );
                }
            }
            println!(
                "runner        scheduling {} (Ctrl-C to stop): {}",
                if projects.len() == 1 {
                    "1 project".to_string()
                } else {
                    format!("{} projects", projects.len())
                },
                projects.join(", ")
            );
            // One scheduler process per project, all inside this same
            // unit/process. Each exits independently (e.g. a held lock during
            // start-up recovery); the board stays up and the unit's
            // Restart=always covers a full crash.
            for project in projects {
                let executor = executor.clone();
                tokio::spawn(async move {
                    if let Err(error) = clawde_katban::runner::run_loop(&project, executor).await {
                        tracing::error!(project = %project, error = %error, "board runner exited");
                    }
                });
            }
        }
    }
    println!("board UI      at http://{addr}/  (Ctrl-C to stop)");
    if !clawde_katban::host::is_loopback(addr) {
        println!(
            "WARNING: bound to {addr} — reachable by other machines; writes are admin-password-gated, but reads are open. Use `clawde katban board password` if you have not already."
        );
    }
    clawde_katban::board_server::run_on(addr).await
}

/// Set/rotate the admin board password (`board password <PW>`), mirroring the
/// guest `link password` parity. Anything given is a strong gate on who can
/// write to the board, so we refuse a too-short password rather than accept a
/// typosquattable one.
async fn board_password(args: Vec<String>) -> anyhow::Result<()> {
    let password = args.first().map(String::as_str).unwrap_or_default().trim();
    if password.is_empty() {
        anyhow::bail!("board password needs a password: board password <PASSWORD>");
    }
    if password.len() < 8 {
        anyhow::bail!("board password must be at least 8 characters");
    }
    let mut store = clawde_katban::board_admin::load().context("load admin store")?;
    store.set_password(password);
    clawde_katban::board_admin::save(&store).context("save admin store")?;
    println!(
        "admin board password set at {}",
        clawde_katban::board_admin::admin_path().display()
    );
    Ok(())
}

/// The admin board's caddy block target: `(subdomain, port)` when the board
/// has been exposed (`board expose`), else `None`. Reads the AdminStore so
/// every expose path (`site expose` / `guest expose` / `board expose`)
/// regenerates the managed config with the board block intact.
fn admin_board_block() -> anyhow::Result<Option<(String, u16)>> {
    let store = clawde_katban::board_admin::load().context("load admin store")?;
    let Some(subdomain) = store.public_subdomain else {
        return Ok(None);
    };
    let port = store
        .board_port
        .unwrap_or(clawde_katban::board_server::DEFAULT_BOARD_PORT);
    Ok(Some((subdomain, port)))
}

/// Publish the admin board behind caddy at an https subdomain, mirroring
/// `guest expose`. Renders the board block into the managed caddy config,
/// writes the reloader units, prints the one-time bootstrap instructions, and
/// optionally points DuckDNS at the subdomain.
async fn board_expose(args: &[String]) -> anyhow::Result<()> {
    use clawde_katban::board_server::DEFAULT_BOARD_PORT;
    use clawde_katban::caddy::{
        bootstrap_instructions, render_config, write_atomic, DEFAULT_INCLUDE_NAME,
    };

    let mut subdomain: Option<String> = None;
    let mut port = DEFAULT_BOARD_PORT;
    let mut dry_run = false;
    let mut caddy_dir: Option<PathBuf> = None;
    let mut duckdns_token: Option<String> = None;
    let mut run_spec: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        let mut take_value = |name: &str| -> anyhow::Result<String> {
            index += 1;
            args.get(index)
                .with_context(|| format!("{name} needs a value"))
                .cloned()
        };
        match flag {
            "--subdomain" => subdomain = Some(take_value("--subdomain")?),
            "--port" => {
                port = take_value("--port")?
                    .parse()
                    .context("--port must be a number")?;
            }
            "--dry-run" => dry_run = true,
            "--caddy-dir" => caddy_dir = Some(PathBuf::from(take_value("--caddy-dir")?)),
            "--duckdns-token" => duckdns_token = Some(take_value("--duckdns-token")?),
            flag if flag.starts_with("--subdomain=") => {
                subdomain = Some(flag["--subdomain=".len()..].to_string());
            }
            flag if flag.starts_with("--port=") => {
                port = flag["--port=".len()..]
                    .parse()
                    .context("--port must be a number")?;
            }
            flag if flag.starts_with("--caddy-dir=") => {
                caddy_dir = Some(PathBuf::from(flag["--caddy-dir=".len()..].to_string()));
            }
            flag if flag.starts_with("--duckdns-token=") => {
                duckdns_token = Some(flag["--duckdns-token=".len()..].to_string());
            }
            flag if flag.starts_with("--run=") => {
                run_spec = Some(flag["--run=".len()..].to_string());
            }
            "--run" => {
                let value = take_value("--run")?;
                run_spec = Some(value);
            }
            flag => anyhow::bail!("unknown option: {flag}\n\n{BOARD_USAGE}"),
        }
        index += 1;
    }

    // Persist the `--run` value exactly as given: a concrete comma-list stays
    // concrete so the unit schedules exactly those projects; the sentinel
    // `all` stays the sentinel so the unit keeps the live-join path (it
    // re-resolves every registered project at serve time and picks up new
    // ones without a re-expose). Validate the list form eagerly here so a bad
    // name can never be persisted.
    let runner_spec = if let Some(spec) = &run_spec {
        if spec.trim().eq_ignore_ascii_case(RUN_ALL) {
            vec![RUN_ALL.to_string()]
        } else {
            resolve_runner_projects(spec)?
        }
    } else {
        Vec::new()
    };
    let mut store = clawde_katban::board_admin::load().context("load admin store")?;
    if let Some(subdomain) = subdomain {
        store.public_subdomain = Some(subdomain);
    }
    let subdomain = store.public_subdomain.clone().context(
        "the board has no public subdomain — pass --subdomain HOST (e.g. board.example.com)",
    )?;
    check_hostname("public subdomain", &subdomain)?;
    store.board_port = Some(port);
    // Additive, like the subdomain: once set, re-exposes keep the runner unit.
    if !runner_spec.is_empty() {
        store.runner_projects = runner_spec.clone();
    }
    let runner_projects = store.runner_projects.clone();
    let runner_is_all = store.runner_projects.len() == 1 && store.runner_projects[0] == RUN_ALL;
    if !dry_run {
        clawde_katban::board_admin::save(&store)?;
    }

    // Regenerate the managed config with every exposed site + guest block +
    // the board block, so running any expose never drops the others.
    let sites_config = clawde_katban::config::load()?;
    let exposed: Vec<_> = sites_config
        .sites
        .iter()
        .filter(|site| site.public_subdomain.is_some())
        .map(|site| {
            let kind = clawde_katban::caddy::site_kind(site);
            (site.clone(), kind)
        })
        .collect();
    let guest = clawde_katban::guest::load()?.public_subdomain.map(|host| {
        let port = clawde_katban::guest::load()
            .ok()
            .and_then(|s| s.guest_port)
            .unwrap_or(clawde_katban::guest_server::DEFAULT_GUEST_PORT);
        (host, port)
    });
    let text = render_config(
        &exposed,
        guest.as_ref().map(|(h, p)| (h.as_str(), *p)),
        Some((&subdomain, port)),
    );
    let caddy_dir = caddy_dir.unwrap_or_else(|| PathBuf::from("/etc/caddy"));
    let managed_path = caddy_dir.join(DEFAULT_INCLUDE_NAME);

    println!("exposing admin board at https://{subdomain}");
    if runner_projects.is_empty() {
        println!(
            "note: the board is only reachable while `clawde katban board serve --port {port}` is running — pass --run <NAME,...> or --run all to render the always-on unit that also runs the card scheduler"
        );
    } else if runner_is_all {
        println!("always-on: board + runner for ALL registered projects via katban-board.service (new projects join live)");
    } else if runner_projects.len() == 1 {
        println!(
            "always-on: board + runner for '{}' via katban-board.service",
            runner_projects[0]
        );
    } else {
        println!(
            "always-on: board + runner for {} projects via katban-board.service (one scheduler per project): {}",
            runner_projects.len(),
            runner_projects.join(", ")
        );
    }
    println!(
        "Managed caddy block (written to {}):",
        managed_path.display()
    );
    println!("{text}");

    if dry_run {
        println!("[dry-run] no files were written.");
        return Ok(());
    }

    write_atomic(&managed_path, &text)?;

    // Reloader units watch the managed file; the board unit is rendered when
    // runner projects are configured (`--run <NAME,...>` / `--run all`).
    let units_dir = write_systemd_units(
        &caddy_dir,
        guest
            .as_ref()
            .map(|(_, p)| *p)
            .unwrap_or(clawde_katban::guest_server::DEFAULT_GUEST_PORT),
        (&runner_projects, port),
    )?;
    println!();
    println!("{}", bootstrap_instructions(&units_dir, &caddy_dir));
    let run_list = runner_projects.join(",");
    if !runner_projects.is_empty() {
        println!(
            "\nAlways-on board unit (install once; board + runner survive reboots):\n\
             \n\
               sudo install -m 644 {} /etc/systemd/system/katban-board.service\n\
               sudo systemctl daemon-reload\n\
               sudo systemctl enable --now katban-board.service\n\
             (runs `board serve --port {port} --run {run_list}`)",
            units_dir.join("katban-board.service").display()
        );
    }

    let token = duckdns_token.or_else(|| {
        std::env::var("DUCKDNS_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
    });
    if let Some(token) = token {
        let label = clawde_katban::duckdns::duckdns_label(&subdomain);
        match clawde_katban::duckdns::update_subdomain(label, &token, None).await {
            Ok(true) => println!("duckdns: pointed '{label}' at your public IP (OK)"),
            Ok(false) => println!(
                "duckdns: update for '{label}' returned KO — create the subdomain in the DuckDNS dashboard first"
            ),
            Err(error) => println!("duckdns: warning — could not update '{label}': {error:#}"),
        }
    } else {
        println!("duckdns: no token — pass --duckdns-token or set DUCKDNS_TOKEN to auto-create the subdomain");
    }
    Ok(())
}

/// Extract `--project NAME` (or `--project=NAME`) and return the rest as
/// positionals.
fn parse_project_flag(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut project = None;
    let mut positionals = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--project" => {
                if let Some(value) = args.get(index + 1) {
                    project = Some(value.clone());
                    index += 1;
                }
            }
            flag if flag.starts_with("--project=") => {
                project = Some(flag["--project=".len()..].to_string());
            }
            flag => positionals.push(flag.to_string()),
        }
        index += 1;
    }
    (project, positionals)
}

const PROJECT_USAGE: &str = "Usage: clawde katban project <command>\n\n\n\
  set <NAME> <DIR>      Register NAME's board to a git repo DIR (needed to run cards)\n\n\
  list                  Show project -> repo mappings\n";

/// `clawde katban project` — the name->repo registry the board runner consults.
fn run_project(args: &[String]) -> anyhow::Result<()> {
    use clawde_katban::projects;

    let Some(subcommand) = args.first().map(|s| s.as_str()) else {
        print!("{PROJECT_USAGE}");
        return Ok(());
    };
    match subcommand {
        "set" => {
            if args.len() < 3 {
                anyhow::bail!("project set needs a name and a dir: project set <NAME> <DIR>");
            }
            let name = args[1].clone();
            let dir = PathBuf::from(&args[2]);
            let canon = projects::set_repo_root(&name, &dir)?;
            println!("project '{name}' -> {}", canon.display());
            println!(
                "cards on this board now run `clawde --print` in a worktree of that repo with: clawde katban board run --project {name}"
            );
            Ok(())
        }
        "list" => {
            let registry = projects::load()?;
            if registry.projects.is_empty() {
                println!(
                    "no projects registered — add one with: clawde katban project set <NAME> <DIR>"
                );
                return Ok(());
            }
            println!("{:<20} REPO ROOT", "PROJECT");
            for (name, root) in &registry.projects {
                println!("{name:<20} {root}");
            }
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print!("{PROJECT_USAGE}");
            Ok(())
        }
        other => anyhow::bail!("unknown project command: {other}\n\n{PROJECT_USAGE}"),
    }
}

async fn run_board(args: &[String]) -> anyhow::Result<()> {
    use clawde_katban::board::{load_board, save_board, DEFAULT_PROJECT};

    let Some(subcommand) = args.first().map(|s| s.as_str()) else {
        print!("{BOARD_USAGE}");
        return Ok(());
    };
    let (project, rest) = parse_project_flag(&args[1..]);
    let project = project.as_deref().unwrap_or(DEFAULT_PROJECT);

    match subcommand {
        "serve" => board_serve(&args[1..]).await,
        "expose" => board_expose(&args[1..]).await,
        "password" => board_password(rest).await,
        "init" => {
            let _guard = clawde_katban::board::BoardLock::acquire(project)?;
            let board = load_board(project)?.unwrap_or_default();
            save_board(&board, project)?;
            println!(
                "initialized board for project '{project}' at {}",
                clawde_katban::board::board_path(project).display()
            );
            Ok(())
        }
        "card" => run_card(project, &rest),
        "link" => {
            if rest.len() < 2 {
                anyhow::bail!("board link needs two card ids: board link <A> <B>");
            }
            // Hold the board lock across the read-modify-write so a concurrent
            // writer (TUI / CLI / future runner) can't silently drop either change.
            let _guard = clawde_katban::board::BoardLock::acquire(project)?;
            let mut board = load_board(project)?.unwrap_or_default();
            match board.add_dependency(&rest[0], &rest[1]) {
                Ok(()) => {
                    save_board(&board, project)?;
                    println!("'{}' now waits for '{}'", rest[0], rest[1]);
                    Ok(())
                }
                Err(message) => {
                    anyhow::bail!("cannot link: {message}")
                }
            }
        }
        "unlink" => {
            if rest.len() < 2 {
                anyhow::bail!("board unlink needs two card ids: board unlink <A> <B>");
            }
            let _guard = clawde_katban::board::BoardLock::acquire(project)?;
            let mut board = load_board(project)?.unwrap_or_default();
            if board.remove_dependency(&rest[0], &rest[1]) {
                save_board(&board, project)?;
                println!("removed link '{}' -> '{}'", rest[0], rest[1]);
            } else {
                anyhow::bail!("no such link between '{}' and '{}'", rest[0], rest[1]);
            }
            Ok(())
        }
        "run" => {
            if clawde_katban::board::load_board(project)?.is_none() {
                anyhow::bail!("no board for project '{project}' — add cards first");
            }
            if clawde_katban::projects::repo_root(project).is_none() {
                println!(
                    "warning: no git repo registered for '{project}' — cards will run in empty scratch dirs. Register one with: clawde katban project set {project} <DIR>"
                );
            }
            println!(
                "running board scheduler for '{project}' (Ctrl-C to stop) — serve the board at: clawde katban board serve"
            );
            let executor = std::sync::Arc::new(clawde_katban::runner::ClawdeExecutor::new());
            clawde_katban::runner::run_loop(project, executor).await
        }
        "ready" => {
            let board = load_board(project)?.unwrap_or_default();
            let mut cap = board.parallel_cap;
            let mut cap_index = 0;
            while cap_index < rest.len() {
                if let Some(value) = rest[cap_index].strip_prefix("--cap=") {
                    cap = value.parse().context("--cap must be a number")?;
                } else if rest[cap_index] == "--cap" {
                    if let Some(value) = rest.get(cap_index + 1) {
                        cap = value.parse().context("--cap must be a number")?;
                    }
                }
                cap_index += 1;
            }
            let running: std::collections::HashSet<String> = board
                .cards
                .iter()
                .filter(|card| card.status == clawde_katban::board::CardStatus::Running)
                .map(|card| card.id.clone())
                .collect();
            let ready = board.queued_ids(&running, cap);
            if ready.is_empty() {
                println!("nothing ready to run");
            } else {
                for id in &ready {
                    let prompt = board.card(id).map(|c| c.prompt.as_str()).unwrap_or("?");
                    println!("{id}  {prompt}");
                }
            }
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print!("{BOARD_USAGE}");
            Ok(())
        }
        other => anyhow::bail!("unknown board command: {other}\n\n{BOARD_USAGE}"),
    }
}

fn run_card(project: &str, args: &[String]) -> anyhow::Result<()> {
    use clawde_katban::board::{load_board, save_board, CardStatus};

    let Some(action) = args.first().map(|s| s.as_str()) else {
        anyhow::bail!("board card needs an action: add|list|set|merge|remove|show");
    };
    let rest = &args[1..];
    match action {
        "add" => {
            let prompt = rest.join(" ").trim().to_string();
            if prompt.is_empty() {
                anyhow::bail!("board card add needs a prompt");
            }
            let _guard = clawde_katban::board::BoardLock::acquire(project)?;
            let mut board = load_board(project)?.unwrap_or_default();
            let id = board.add_card(&prompt);
            save_board(&board, project)?;
            println!("{id}  {prompt}");
            Ok(())
        }
        "list" => {
            let board = load_board(project)?.unwrap_or_default();
            if board.cards.is_empty() {
                println!("no cards — add one with: clawde katban board card add <PROMPT>");
            } else {
                for card in &board.cards {
                    println!(
                        "{:<12} {:>8}  {}",
                        card.id,
                        status_name(card.status),
                        card.prompt
                    );
                }
            }
            Ok(())
        }
        "set" => {
            if rest.len() < 2 {
                anyhow::bail!("board card set needs an id and a status");
            }
            let status = CardStatus::parse(&rest[1])
                .with_context(|| format!("unknown status '{}'", rest[1]))?;
            let _guard = clawde_katban::board::BoardLock::acquire(project)?;
            let mut board = load_board(project)?.unwrap_or_default();
            if board.set_status(&rest[0], status) {
                save_board(&board, project)?;
                println!("'{}' -> {}", rest[0], status_name(status));
                Ok(())
            } else {
                anyhow::bail!("no card with id '{}'", rest[0]);
            }
        }
        "merge" => {
            if rest.is_empty() {
                anyhow::bail!("board card merge needs an id");
            }
            // Option B — pin-commit flow: merge the review card's branch into
            // the project and close it (dependents then unblock).
            clawde_katban::commit::merge_card(project, &rest[0]).map_err(|e| anyhow::anyhow!(e))?;
            println!("'{}' merged into the project", rest[0]);
            Ok(())
        }
        "remove" => {
            if rest.is_empty() {
                anyhow::bail!("board card remove needs an id");
            }
            // Discard: archive the card AND delete its pinned branch (if any).
            clawde_katban::commit::discard_card(project, &rest[0])
                .map_err(|e| anyhow::anyhow!(e))?;
            println!("'{}' archived (branch cleaned up)", rest[0]);
            Ok(())
        }
        "show" => card_show(project, rest),
        other => anyhow::bail!("unknown card action: {other}"),
    }
}

/// Print every detail of one card — status, retries, last result, and (when
/// present) the runner-captured diff — so a terminal admin can review a card's
/// work without a browser or a git checkout.
fn card_show(project: &str, args: &[String]) -> anyhow::Result<()> {
    let id = args
        .first()
        .context("card show needs an id: board card show <ID>")?;
    let board = clawde_katban::board::load_board(project)?.unwrap_or_default();
    let card = board
        .card(id)
        .with_context(|| format!("no card with id '{id}'"))?;
    println!("id:       {}", card.id);
    println!("prompt:   {}", card.prompt);
    println!("status:   {}", status_name(card.status));
    if let Some(branch) = &card.branch {
        println!("branch:   {branch}");
    }
    if let Some(work_dir) = &card.work_dir {
        println!("work dir: {work_dir}");
    }
    if card.retries > 0 {
        println!("retries:  {}", card.retries);
    }
    if let Some(commit) = &card.commit {
        let short = commit.get(..commit.len().min(12)).unwrap_or(commit);
        println!("commit:   {short}");
    }
    if let Some(result) = &card.result {
        println!("result:");
        println!("{result}");
    }
    if let Some(diff) = &card.diff {
        println!("diff ({} ch):", diff.len());
        println!("{diff}");
    }
    Ok(())
}

fn status_name(status: clawde_katban::board::CardStatus) -> &'static str {
    match status {
        clawde_katban::board::CardStatus::Backlog => "backlog",
        clawde_katban::board::CardStatus::Queued => "queued",
        clawde_katban::board::CardStatus::Running => "running",
        clawde_katban::board::CardStatus::Blocked => "blocked",
        clawde_katban::board::CardStatus::Review => "review",
        clawde_katban::board::CardStatus::Failed => "failed",
        clawde_katban::board::CardStatus::Done => "done",
    }
}

/// Where the rendered systemd units live so the bootstrap can install them
/// (the expose commands write them here and print install instructions).
fn units_dir() -> PathBuf {
    clawde_katban::config::katban_data_dir().join("caddy")
}

/// A runner project is embedded in a systemd `ExecStart` line, so it must be
/// a single argv word: no whitespace, no quotes, no shell metacharacters.
fn validate_runner_project(project: &str) -> anyhow::Result<()> {
    if project.trim().is_empty() {
        anyhow::bail!("--run needs a non-empty project name");
    }
    if project.chars().any(char::is_whitespace)
        || project.chars().any(|c| {
            matches!(
                c,
                '"' | '\'' | '\\' | '$' | '`' | ';' | '&' | '|' | '>' | '<' | '(' | ')' | ','
            )
        })
    {
        anyhow::bail!(
            "--run project name cannot contain spaces, commas, or shell metacharacters: '{project}'"
        );
    }
    Ok(())
}

/// Sentinel `--run all`: defined in the katban crate (shared with the board web
/// server) and re-exported here so the CLI, its tests, and the web board all
/// agree on the one value.
pub use clawde_katban::board_admin::RUN_ALL;

/// Resolve a `--run` value (a comma-separated project list, or `all`) to the
/// concrete project names the scheduler should run. `all` means every project
/// with a registered git repo — so the always-on unit picks up new projects
/// without editing the unit, and the scheduler only ever touches boards that
/// can actually execute.
fn resolve_runner_projects(value: &str) -> anyhow::Result<Vec<String>> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("all") {
        // Resolve to everything the runner can execute, in registry order.
        let registered = clawde_katban::projects::registered_projects();
        if registered.is_empty() {
            anyhow::bail!(
                "--run all: no projects have a registered git repo. Register one with: clawde katban project set <NAME> <DIR>"
            );
        }
        return Ok(registered);
    }
    let mut projects = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            anyhow::bail!("--run has an empty project name in the list '{value}'");
        }
        validate_runner_project(part)?;
        projects.push(part.to_string());
    }
    if projects.is_empty() {
        anyhow::bail!("--run needs a project name, a comma-separated list, or 'all'");
    }
    Ok(projects)
}

/// Validate a value before it is embedded in the managed caddy config.
fn check_hostname(what: &str, value: &str) -> anyhow::Result<()> {
    if clawde_katban::caddy::valid_hostname(value) {
        Ok(())
    } else {
        anyhow::bail!(
            "invalid {what} '{value}' — use letters, digits, '.', '-', '_' (no spaces, '#', '{{', '}}', or path separators)"
        )
    }
}

/// True when something is listening on 127.0.0.1:port (a live site that is
/// actually being served).
fn port_is_open(port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
}

/// The OS user the always-on unit runs as: `$USER`, falling back to the owner
/// of the katban data dir when invoked as root (e.g. via sudo). Never "root"
/// — the unit must not run as root (spec §11).
fn unit_user() -> anyhow::Result<String> {
    let user = std::env::var("USER").unwrap_or_default();
    if !user.is_empty() && user != "root" {
        return Ok(user);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = std::fs::metadata(clawde_katban::config::katban_data_dir()) {
            let uid = meta.uid();
            if let Ok(passwd) = std::fs::read_to_string("/etc/passwd") {
                for line in passwd.lines() {
                    let mut fields = line.split(':');
                    let (Some(name), Some(_), Some(uid_str)) =
                        (fields.next(), fields.next(), fields.next())
                    else {
                        continue;
                    };
                    if uid_str == uid.to_string() && name != "root" {
                        return Ok(name.to_string());
                    }
                }
            }
        }
    }
    anyhow::bail!(
        "refusing to render a systemd unit that runs as root — re-run 'clawde katban site expose' as the OS user that owns ~/.clawde (not via sudo)"
    )
}

/// Render + write `katban.service` (always-on guest chat server) and the
/// caddy reloader units. Returns the directory the files were written to.
/// The binary path is resolved from the currently running clawde, so the
/// unit always starts the exact build the admin is using; rebuild in place
/// (e.g. `clawded`) + `sudo systemctl restart katban` to update. `caddy_dir`
/// is where the managed include lives (the reloader must watch that exact
/// file) and `guest_port` is the port the service runs the guest server on.
/// Write the systemd units the expose flow installs. `board` is the runner
/// project list + board port when the board should be always-on (`board
/// expose --run <NAME,...>` / `--run all`); an empty list keeps the units
/// guest-only (board not always-on).
fn write_systemd_units(
    caddy_dir: &Path,
    guest_port: u16,
    board: (&[String], u16),
) -> anyhow::Result<PathBuf> {
    let dir = units_dir();
    std::fs::create_dir_all(&dir)?;
    let binary = std::env::current_exe()?
        .canonicalize()
        .unwrap_or_else(|_| std::env::current_exe().unwrap());
    let user = unit_user()?;
    std::fs::write(
        dir.join("katban.service"),
        clawde_katban::caddy::render_service_unit(&binary.display().to_string(), &user, guest_port),
    )?;
    std::fs::write(
        dir.join("katban-reload.path"),
        clawde_katban::caddy::render_reloader_path_unit(
            &caddy_dir.join(clawde_katban::caddy::DEFAULT_INCLUDE_NAME),
        ),
    )?;
    std::fs::write(
        dir.join("katban-reload.service"),
        clawde_katban::caddy::render_reloader_service_unit("systemctl reload caddy"),
    )?;
    let (projects, board_port) = board;
    if !projects.is_empty() {
        std::fs::write(
            dir.join("katban-board.service"),
            clawde_katban::caddy::render_board_service_unit(
                &binary.display().to_string(),
                &user,
                projects,
                board_port,
            ),
        )?;
    }
    Ok(dir)
}

fn run_status() -> anyhow::Result<()> {
    let summary = clawde_katban::status::status();
    println!("data dir:      {}", summary.data_dir.display());
    println!(
        "sites:         {} hosted, {} exposed",
        summary.site_count, summary.exposed_count
    );
    println!(
        "boards:        {}",
        if summary.board_projects.is_empty() {
            "none".to_string()
        } else {
            summary.board_projects.join(", ")
        }
    );
    println!(
        "runnable:      {}",
        if summary.runnable_projects.is_empty() {
            "none (register with: clawde katban project set <NAME> <DIR>)".to_string()
        } else {
            summary.runnable_projects.join(", ")
        }
    );
    println!("managed caddy: {}", summary.managed_caddy_path.display());
    if summary.managed_caddy_exists {
        println!("               (present)");
    } else if summary.exposed_count > 0 {
        println!("               (MISSING — run: clawde katban site expose <name>)");
    }
    Ok(())
}

const LINK_USAGE: &str = r#"Usage: clawde katban link <command>

  create <NAME> [--expires-in DAYS|never] [--max-concurrent N]
                                             Create a guest link (prints the URL + one-time password)
  list                                       List guest links
  show <ID>                                  Show one link (URL, expiry, devices)
  revoke <ID>                                Revoke a link (kicks its devices)
  password <ID>                              Rotate a link's password (prints the new one once)
"#;

const GUEST_USAGE: &str = r#"Usage: clawde katban guest <command> [OPTIONS]

  serve [--port N] [--host IP] [--allow-non-loopback]
                                              Run the guest chat server
  expose [--subdomain HOST] [--port N] [--dry-run] [--caddy-dir DIR]
         [--duckdns-token TOKEN]              Put the guest chat behind caddy
                                              (writes the managed katban.conf)
  unblock <IP>                                Clear an IP's lockouts / permanent block

Runs the guest chat server: friends open the URL, type the shared password,
and chat with Clawde (chat + web search only — no files, no shell, nothing
else). Guest chat rides the host's free/limited providers; if none are
configured the chat explains that politely.
"#;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn run_link(args: &[String]) -> anyhow::Result<()> {
    use clawde_katban::guest::{generate_password, load, save};

    let Some(subcommand) = args.first().map(|s| s.as_str()) else {
        print!("{LINK_USAGE}");
        return Ok(());
    };
    match subcommand {
        "create" => {
            // The name is everything before the first flag, so multi-word
            // names work without shell quoting (mirrors `/katban link create`).
            let mut name_parts = Vec::new();
            let mut index = 1;
            while index < args.len() && !args[index].starts_with("--") {
                name_parts.push(args[index].as_str());
                index += 1;
            }
            let name = name_parts.join(" ");
            if name.trim().is_empty() {
                anyhow::bail!("link create needs a name: clawde katban link create <NAME>");
            }
            let mut expires_at = None;
            let mut max_concurrent = clawde_katban::guest::DEFAULT_MAX_CONCURRENT;
            while index < args.len() {
                match args[index].as_str() {
                    "--expires-in" => {
                        let value = args
                            .get(index + 1)
                            .context("--expires-in needs DAYS or 'never'")?;
                        if value == "never" {
                            expires_at = None;
                        } else {
                            let days: u64 = value
                                .parse()
                                .context("--expires-in must be DAYS or 'never'")?;
                            expires_at = Some(now_secs() + days * 24 * 3600);
                        }
                        index += 1;
                    }
                    "--max-concurrent" => {
                        let value = args
                            .get(index + 1)
                            .context("--max-concurrent needs a number")?;
                        max_concurrent =
                            value.parse().context("--max-concurrent must be a number")?;
                        index += 1;
                    }
                    flag if flag.starts_with("--expires-in=") => {
                        let value = &flag["--expires-in=".len()..];
                        if value == "never" {
                            expires_at = None;
                        } else {
                            let days: u64 = value
                                .parse()
                                .context("--expires-in must be DAYS or 'never'")?;
                            expires_at = Some(now_secs() + days * 24 * 3600);
                        }
                    }
                    flag if flag.starts_with("--max-concurrent=") => {
                        max_concurrent = flag["--max-concurrent=".len()..]
                            .parse()
                            .context("--max-concurrent must be a number")?;
                    }
                    flag => anyhow::bail!("unknown option: {flag}\n\n{LINK_USAGE}"),
                }
                index += 1;
            }

            let password = generate_password();
            let mut store = load()?;
            store.prune(now_secs());
            let id = store.create_link(&name, &password, expires_at, max_concurrent);
            save(&store)?;
            let expiry_text = match expires_at {
                Some(unix) => {
                    let days = unix.saturating_sub(now_secs()) / 86400;
                    format!("{days} days")
                }
                None => "never".to_string(),
            };
            println!("created guest link '{name}' ({id})");
            println!(
                "url:      http://127.0.0.1:{}/",
                clawde_katban::guest_server::DEFAULT_GUEST_PORT
            );
            println!("password: {password}");
            println!("expires:  {expiry_text}");
            println!("max chat: {max_concurrent} at once");
            println!();
            println!("share the URL + password with friends. The password is shown once — ");
            println!("keep it safe. Serve the link with: clawde katban guest serve");
            Ok(())
        }
        "list" => {
            let store = load()?;
            if store.links.is_empty() {
                println!("no guest links — create one with: clawde katban link create <NAME>");
                return Ok(());
            }
            println!("{:<8} {:<20} {:<10} EXPIRES", "ID", "NAME", "STATE");
            for link in &store.links {
                let state = if link.revoked {
                    "revoked"
                } else if link.expires_at.is_some_and(|expiry| expiry <= now_secs()) {
                    "expired"
                } else {
                    "active"
                };
                let expiry = match link.expires_at {
                    Some(unix) => format!("in {}d", unix.saturating_sub(now_secs()) / 86400),
                    None => "never".to_string(),
                };
                println!("{:<8} {:<20} {:<10} {}", link.id, link.name, state, expiry);
            }
            Ok(())
        }
        "show" => {
            let id = args
                .get(1)
                .context("link show needs an id: clawde katban link show <ID>")?;
            let store = load()?;
            let link = store
                .link(id)
                .with_context(|| format!("no guest link '{id}'"))?;
            let devices = store.devices.get(id).map(|d| d.len()).unwrap_or(0);
            println!("id:          {}", link.id);
            println!("name:        {}", link.name);
            println!(
                "state:       {}",
                if link.revoked { "revoked" } else { "active" }
            );
            println!(
                "expires:     {}",
                link.expires_at
                    .map(|unix| format!("in {}d", unix.saturating_sub(now_secs()) / 86400))
                    .unwrap_or_else(|| "never".to_string())
            );
            println!("devices:     {devices}");
            println!("max chat:    {}", link.max_concurrent);
            println!(
                "url:         http://127.0.0.1:{}/",
                clawde_katban::guest_server::DEFAULT_GUEST_PORT
            );
            Ok(())
        }
        "revoke" => {
            let id = args
                .get(1)
                .context("link revoke needs an id: clawde katban link revoke <ID>")?;
            let mut store = load()?;
            if store.revoke_link(id) {
                save(&store)?;
                println!("revoked guest link '{id}' — its devices can no longer chat");
                Ok(())
            } else {
                anyhow::bail!("no guest link '{id}'");
            }
        }
        "password" => {
            let id = args
                .get(1)
                .context("link password needs an id: clawde katban link password <ID>")?;
            let password = generate_password();
            let mut store = load()?;
            if !store.set_password(id, &password) {
                anyhow::bail!("no guest link '{id}'");
            }
            save(&store)?;
            println!("rotated password for guest link '{id}'");
            println!("password: {password}");
            println!();
            println!("The old password no longer works. The new one is shown once — keep it safe.");
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print!("{LINK_USAGE}");
            Ok(())
        }
        other => anyhow::bail!("unknown link command: {other}\n\n{LINK_USAGE}"),
    }
}

async fn run_guest(args: &[String]) -> anyhow::Result<()> {
    let Some(subcommand) = args.first().map(|s| s.as_str()) else {
        print!("{GUEST_USAGE}");
        return Ok(());
    };
    match subcommand {
        "serve" => guest_serve(&args[1..]).await,
        "expose" => guest_expose(&args[1..]).await,
        "unblock" => guest_unblock(&args[1..]),
        "help" | "--help" | "-h" => {
            print!("{GUEST_USAGE}");
            Ok(())
        }
        other => anyhow::bail!("unknown guest command: {other}\n\n{GUEST_USAGE}"),
    }
}

async fn guest_serve(args: &[String]) -> anyhow::Result<()> {
    use clawde_katban::guest_server::{GuestServer, DEFAULT_GUEST_PORT};
    use std::sync::Arc;

    let mut port = DEFAULT_GUEST_PORT;
    let mut host = "127.0.0.1".to_string();
    let mut allow_non_loopback = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--port" => {
                port = args
                    .get(index + 1)
                    .context("--port needs a number")?
                    .parse()
                    .context("--port must be a number")?;
                index += 1;
            }
            "--host" => {
                host = args
                    .get(index + 1)
                    .context("--host needs an IP")?
                    .to_string();
                index += 1;
            }
            "--allow-non-loopback" => allow_non_loopback = true,
            flag if flag.starts_with("--port=") => {
                port = flag["--port=".len()..]
                    .parse()
                    .context("--port must be a number")?;
            }
            flag if flag.starts_with("--host=") => {
                host = flag["--host=".len()..].to_string();
            }
            flag => anyhow::bail!("unknown option: {flag}\n\n{GUEST_USAGE}"),
        }
        index += 1;
    }

    let addr = clawde_katban::host::parse_bind_addr(&host, port)?;
    if !clawde_katban::host::is_loopback(addr) && !allow_non_loopback {
        anyhow::bail!(
            "refusing to bind {addr} — pass --allow-non-loopback to serve beyond loopback"
        );
    }

    let mut store = clawde_katban::guest::load()?;
    if store.links.is_empty() {
        println!("WARNING: no guest links yet — create one with: clawde katban link create <NAME>");
    }
    let public_url = store.public_subdomain.clone();
    store.prune(now_secs());
    let store = Arc::new(std::sync::Mutex::new(store));
    let search: Arc<dyn clawde_katban::search::GuestSearch> = Arc::new(
        clawde_katban::search::SearxClient::new(clawde_katban::search::DEFAULT_ENDPOINT),
    );
    let backend: Arc<dyn clawde_katban::chat::GuestBackend> =
        Arc::new(clawde_katban::chat::FreeBackend::new());
    let engine = Arc::new(clawde_katban::chat::ChatEngine::new(backend, search));
    let server = GuestServer::new(engine, store);
    println!("guest chat serving at http://{addr}/  (Ctrl-C to stop)");
    if !clawde_katban::host::is_loopback(addr) {
        println!("WARNING: bound to {addr} — reachable by other machines; guests can chat but have no access to your system.");
    }
    if let Some(subdomain) = &public_url {
        println!("public:      https://{subdomain} (via caddy — see: clawde katban guest expose)");
    }
    println!(
        "guest search: local SearXNG at {}",
        clawde_katban::search::DEFAULT_ENDPOINT
    );
    println!(
        "guests ride free/limited providers; chat degrades gracefully if none are configured."
    );
    server.run(addr).await?;
    Ok(())
}

/// Put the guest chat behind caddy: writes the managed `katban.conf` (all
/// exposed sites + the guest block), emits the reloader units, prints the
/// one-time bootstrap, and best-effort updates the DuckDNS subdomain.
async fn guest_expose(args: &[String]) -> anyhow::Result<()> {
    use clawde_katban::caddy::{
        bootstrap_instructions, render_config, write_atomic, DEFAULT_INCLUDE_NAME,
    };
    use clawde_katban::config::load as load_sites;
    use clawde_katban::guest::{load, save};

    let mut subdomain: Option<String> = None;
    let mut port = clawde_katban::guest_server::DEFAULT_GUEST_PORT;
    let mut dry_run = false;
    let mut caddy_dir: Option<PathBuf> = None;
    let mut duckdns_token: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        let mut take_value = |name: &str| -> anyhow::Result<String> {
            index += 1;
            args.get(index)
                .with_context(|| format!("{name} needs a value"))
                .cloned()
        };
        match flag {
            "--subdomain" => subdomain = Some(take_value("--subdomain")?),
            "--port" => {
                port = take_value("--port")?
                    .parse()
                    .context("--port must be a number")?;
            }
            "--dry-run" => dry_run = true,
            "--caddy-dir" => caddy_dir = Some(PathBuf::from(take_value("--caddy-dir")?)),
            "--duckdns-token" => duckdns_token = Some(take_value("--duckdns-token")?),
            flag if flag.starts_with("--subdomain=") => {
                subdomain = Some(flag["--subdomain=".len()..].to_string());
            }
            flag if flag.starts_with("--port=") => {
                port = flag["--port=".len()..]
                    .parse()
                    .context("--port must be a number")?;
            }
            flag if flag.starts_with("--caddy-dir=") => {
                caddy_dir = Some(PathBuf::from(flag["--caddy-dir=".len()..].to_string()));
            }
            flag if flag.starts_with("--duckdns-token=") => {
                duckdns_token = Some(flag["--duckdns-token=".len()..].to_string());
            }
            flag => anyhow::bail!("unknown option: {flag}\n\n{GUEST_USAGE}"),
        }
        index += 1;
    }

    let mut store = load()?;
    if let Some(subdomain) = subdomain {
        store.public_subdomain = Some(subdomain);
    }
    let subdomain = store.public_subdomain.clone().context(
        "guest chat has no public subdomain — pass --subdomain HOST (e.g. chat.example.com)",
    )?;
    check_hostname("public subdomain", &subdomain)?;
    // Record the port the caddy block proxies to so the always-on unit is
    // regenerated with the same port on every future expose.
    store.guest_port = Some(port);
    if !dry_run {
        save(&store)?;
    }

    // The managed config holds every exposed site PLUS the guest block, so
    // running `guest expose` never drops previously exposed sites.
    let sites_config = load_sites()?;
    let exposed: Vec<_> = sites_config
        .sites
        .iter()
        .filter(|site| site.public_subdomain.is_some())
        .map(|site| {
            let kind = clawde_katban::caddy::site_kind(site);
            (site.clone(), kind)
        })
        .collect();
    let board = admin_board_block()?;
    let text = render_config(
        &exposed,
        Some((&subdomain, port)),
        board.as_ref().map(|(h, p)| (h.as_str(), *p)),
    );
    let caddy_dir = caddy_dir.unwrap_or_else(|| PathBuf::from("/etc/caddy"));
    let managed_path = caddy_dir.join(DEFAULT_INCLUDE_NAME);

    println!("exposing guest chat at https://{subdomain}");
    println!(
        "Managed caddy block (written to {}):",
        managed_path.display()
    );
    println!("{text}");

    if dry_run {
        println!("[dry-run] no files were written.");
        return Ok(());
    }

    write_atomic(&managed_path, &text)?;

    // Write the systemd units (service + reloader) so the bootstrap can
    // install them; the instructions reference these files. The reloader
    // watches the actual managed file (honoring --caddy-dir) and the service
    // runs `guest serve` on the port just rendered.
    let units_dir = write_systemd_units(&caddy_dir, port, (&[], 0))?;
    println!();
    println!("{}", bootstrap_instructions(&units_dir, &caddy_dir));

    let token = duckdns_token.or_else(|| {
        std::env::var("DUCKDNS_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
    });
    if let Some(token) = token {
        let label = clawde_katban::duckdns::duckdns_label(&subdomain);
        match clawde_katban::duckdns::update_subdomain(label, &token, None).await {
            Ok(true) => println!("duckdns: pointed '{label}' at your public IP (OK)"),
            Ok(false) => println!(
                "duckdns: update for '{label}' returned KO — create the subdomain in the DuckDNS dashboard first"
            ),
            Err(error) => println!("duckdns: warning — could not update '{label}': {error:#}"),
        }
    } else {
        println!("duckdns: no token — pass --duckdns-token or set DUCKDNS_TOKEN to auto-create the subdomain");
    }
    Ok(())
}

fn guest_unblock(args: &[String]) -> anyhow::Result<()> {
    use clawde_katban::guest::{load, save};

    let ip = args
        .first()
        .context("guest unblock needs an IP: clawde katban guest unblock <IP>")?;
    let mut store = load()?;
    store.reset_failed_attempts(ip);
    save(&store)?;
    println!("cleared lockouts and permanent blocks for '{ip}'");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_katban::config::{KatbanConfig, SiteConfig};

    fn site(name: &str, locked: bool, subdomain: Option<&str>) -> SiteConfig {
        SiteConfig {
            name: name.to_string(),
            root: PathBuf::from(format!("/srv/{name}")),
            port: 8799,
            public_subdomain: subdomain.map(str::to_string),
            locked,
        }
    }

    fn config_with(sites: Vec<SiteConfig>) -> KatbanConfig {
        KatbanConfig {
            version: clawde_katban::config::CONFIG_VERSION,
            sites,
        }
    }

    #[test]
    fn resolve_serve_target_uses_registered_site() {
        let config = config_with(vec![site("demo", false, None), site("blog", true, None)]);
        let (root, port, live) = resolve_serve_target(&config, "demo", None, false, false).unwrap();
        assert_eq!(root, PathBuf::from("/srv/demo"));
        assert_eq!(port, 8799);
        assert!(live);

        // Locked site -> live reload off unless overridden.
        let (_, _, live) = resolve_serve_target(&config, "blog", None, false, false).unwrap();
        assert!(!live);
        let (_, _, live) = resolve_serve_target(&config, "blog", None, false, true).unwrap();
        assert!(!live);
    }

    #[test]
    fn resolve_serve_target_falls_back_to_directory() {
        let config = config_with(vec![]);
        let tmp = tempfile::tempdir().unwrap();
        let (root, port, live) =
            resolve_serve_target(&config, tmp.path().to_str().unwrap(), None, false, false)
                .unwrap();
        assert!(root.is_dir());
        assert_eq!(port, clawde_katban::config::DEFAULT_SITE_PORT);
        assert!(live);
    }

    #[test]
    fn resolve_serve_target_errors_on_missing_dir_and_name() {
        let config = config_with(vec![site("demo", false, None)]);
        assert!(resolve_serve_target(&config, "/no/such/dir-12345", None, false, false).is_err());
    }

    #[test]
    fn parse_site_args_handles_flags_and_positional() {
        let args: Vec<String> = vec![
            "/srv/demo".into(),
            "--name".into(),
            "demo".into(),
            "--port=8801".into(),
            "--no-reload".into(),
        ];
        let opts = parse_site_args(&args, false).unwrap();
        assert_eq!(opts.dir.as_deref(), Some("/srv/demo"));
        assert_eq!(opts.name.as_deref(), Some("demo"));
        assert_eq!(opts.port, Some(8801));
        assert!(opts.no_reload);
    }

    #[test]
    fn find_site_matches_by_name() {
        let config = config_with(vec![site("demo", false, None)]);
        assert!(find_site(&config, "demo").is_some());
        assert!(find_site(&config, "other").is_none());
    }

    #[test]
    fn systemd_units_follow_caddy_dir_and_guest_port() {
        // Serialize CLAWDE_HOME mutation on the binary-wide lock (repo rule).
        let _guard = crate::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().unwrap();
        let previous = std::env::var("CLAWDE_HOME").ok();
        std::env::set_var("CLAWDE_HOME", tmp.path());
        let result = (|| -> anyhow::Result<()> {
            let caddy_dir = tmp.path().join("custom-caddy");
            let units = write_systemd_units(&caddy_dir, 9000, (&[], 0))?;
            let service = std::fs::read_to_string(units.join("katban.service"))?;
            assert!(
                service.contains("katban guest serve --port 9000"),
                "service unit must bind the rendered guest port"
            );
            let path_unit = std::fs::read_to_string(units.join("katban-reload.path"))?;
            assert!(
                path_unit.contains(&format!(
                    "PathChanged={}",
                    caddy_dir.join("katban.conf").display()
                )),
                "reloader must watch the managed file in --caddy-dir"
            );
            // No runner project -> no board unit.
            assert!(!units.join("katban-board.service").exists());
            Ok(())
        })();
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
        result.unwrap();
    }

    #[test]
    fn systemd_units_render_board_unit_when_runner_project_set() {
        // Serialize CLAWDE_HOME mutation on the binary-wide lock (repo rule).
        let _guard = crate::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().unwrap();
        let previous = std::env::var("CLAWDE_HOME").ok();
        std::env::set_var("CLAWDE_HOME", tmp.path());
        let result = (|| -> anyhow::Result<()> {
            let caddy_dir = tmp.path().join("caddy");
            // Single project -> clean description + ExecStart.
            let units = write_systemd_units(&caddy_dir, 9000, (&[String::from("demo")], 8790))?;
            let board = std::fs::read_to_string(units.join("katban-board.service"))?;
            assert!(
                board.contains("ExecStart=clawde katban board serve --port 8790 --run demo")
                    || board.contains("board serve --port 8790 --run demo"),
                "board unit must serve the board port and run the project: {board}"
            );
            assert!(board.contains("Description=Katban admin board + runner (demo)"));
            // The guest + reloader units are still written.
            assert!(units.join("katban.service").exists());
            assert!(units.join("katban-reload.path").exists());
            Ok(())
        })();
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
        result.unwrap();
    }

    #[test]
    fn systemd_units_render_board_unit_for_multiple_projects() {
        // Serialize CLAWDE_HOME mutation on the binary-wide lock (repo rule).
        let _guard = crate::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().unwrap();
        let previous = std::env::var("CLAWDE_HOME").ok();
        std::env::set_var("CLAWDE_HOME", tmp.path());
        let result = (|| -> anyhow::Result<()> {
            let caddy_dir = tmp.path().join("caddy");
            let units = write_systemd_units(
                &caddy_dir,
                9000,
                (&[String::from("app"), String::from("api")], 8790),
            )?;
            let board = std::fs::read_to_string(units.join("katban-board.service"))?;
            assert!(
                board.contains("board serve --port 8790 --run app,api"),
                "board unit must schedule both projects: {board}"
            );
            assert!(board.contains("Description=Katban admin board + runner (2 projects)"));
            Ok(())
        })();
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
        result.unwrap();
    }

    #[test]
    fn validate_runner_project_rejects_execstart_injection() {
        assert!(validate_runner_project("demo").is_ok());
        assert!(validate_runner_project("my-board").is_ok());
        assert!(validate_runner_project("").is_err());
        assert!(validate_runner_project("   ").is_err());
        assert!(validate_runner_project("demo extra").is_err());
        assert!(validate_runner_project("demo;rm -rf /").is_err());
        assert!(validate_runner_project("demo`id`").is_err());
        assert!(validate_runner_project("$(x)").is_err());
        // A comma now belongs to the list separator, never inside a name.
        assert!(validate_runner_project("de,mo").is_err());
    }

    #[test]
    fn resolve_runner_projects_parses_lists_and_all() {
        // Comma-separated list: split and validated individually.
        let projects = resolve_runner_projects("app, api,batch").unwrap();
        assert_eq!(projects, vec!["app", "api", "batch"]);
        // A name carrying a shell metacharacter is rejected before it ever
        // reaches an ExecStart slot (commas split the list, so they cannot
        // appear inside a name).
        assert!(resolve_runner_projects("app,de;mo").is_err());
        // Empty components / empty input are rejected.
        assert!(resolve_runner_projects("app,,api").is_err());
        assert!(resolve_runner_projects("  ").is_err());
        // `all` resolves via the registry (empty here -> error).
        let _guard = crate::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().unwrap();
        let previous = std::env::var("CLAWDE_HOME").ok();
        std::env::set_var("CLAWDE_HOME", tmp.path());
        assert!(resolve_runner_projects("all").is_err()); // no registered projects
        let repo = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        clawde_katban::projects::set_repo_root("app", repo.path()).unwrap();
        clawde_katban::projects::set_repo_root("api", repo.path()).unwrap();
        let all = resolve_runner_projects("all").unwrap();
        assert!(all.contains(&"app".to_string()));
        assert!(all.contains(&"api".to_string()));
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
    }

    #[test]
    fn board_expose_run_all_persists_the_refresh_sentinel() {
        // `board expose --run all` must store the `all` sentinel (not a baked
        // project list) so a re-expose keeps the unit on the live-join path,
        // which resolves every registered project at serve time.
        // Serialize CLAWDE_HOME mutation on the binary-wide lock (repo rule).
        let _guard = crate::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().unwrap();
        let previous = std::env::var("CLAWDE_HOME").ok();
        std::env::set_var("CLAWDE_HOME", tmp.path());

        // A sync current-thread runtime so the async expose call runs without
        // an active tokio runtime for the env-guard's blocking_lock above.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let args = vec![
            "--run=all".to_string(),
            "--subdomain=board.example.com".to_string(),
            format!("--caddy-dir={}", tmp.path().join("caddy").display()),
        ];
        let exposed = rt.block_on(board_expose(&args));
        // Assert while CLAWDE_HOME still points at the sandbox -- the load()
        // and unit path resolve via the env at call time.
        let store = clawde_katban::board_admin::load().unwrap();
        let unit_path = clawde_katban::config::katban_data_dir().join("caddy/katban-board.service");
        let unit = std::fs::read_to_string(unit_path).unwrap();
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
        drop(_guard);
        assert!(exposed.is_ok(), "board expose should succeed: {exposed:?}");

        // The persisted store holds the `all` sentinel, so a later re-expose
        // keeps the live-join path.
        assert_eq!(store.runner_projects, vec![RUN_ALL.to_string()]);
        // The rendered always-on unit schedules `--run all` (re-resolved at
        // serve time), not a baked project list.
        assert!(
            unit.contains("--run all"),
            "unit must keep --run all, got: {unit}"
        );
    }

    #[test]
    fn admin_board_block_reads_exposed_subdomain_and_port() {
        // Serialize CLAWDE_HOME mutation on the binary-wide lock (repo rule).
        let _guard = crate::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().unwrap();
        let previous = std::env::var("CLAWDE_HOME").ok();
        std::env::set_var("CLAWDE_HOME", tmp.path());
        let result = (|| -> anyhow::Result<()> {
            // Not exposed yet -> None.
            let store = clawde_katban::board_admin::load()?;
            clawde_katban::board_admin::save(&store)?;
            assert!(admin_board_block()?.is_none());
            // Expose with a custom port -> Some((subdomain, port)).
            let mut store = clawde_katban::board_admin::load()?;
            store.public_subdomain = Some("board.example.com".to_string());
            store.board_port = Some(8891);
            clawde_katban::board_admin::save(&store)?;
            let block = admin_board_block()?.expect("exposed block");
            assert_eq!(block, ("board.example.com".to_string(), 8891));
            Ok(())
        })();
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
        result.unwrap();
    }

    #[test]
    fn parse_project_flag_extracts_project_and_leaves_positionals() {
        let args: Vec<String> = vec![
            "card".into(),
            "add".into(),
            "--project".into(),
            "my-repo".into(),
            "build the thing".into(),
        ];
        let (project, positionals) = parse_project_flag(&args);
        assert_eq!(project.as_deref(), Some("my-repo"));
        assert_eq!(positionals, vec!["card", "add", "build the thing"]);

        let (project, positionals) = parse_project_flag(&["--project=other".to_string()]);
        assert_eq!(project.as_deref(), Some("other"));
        assert!(positionals.is_empty());
    }
}
