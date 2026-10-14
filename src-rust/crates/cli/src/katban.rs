// `clawde katban` — the production Kanban development surface for Clawde.
//
// Katban owns development boards, project/repository registration, agent
// execution, and hosted development sites. Cat Chat is a separate sandbox
// command group implemented below as `clawde catchat`.
//
// Hosted-sites slice (spec §10): add/list/show/remove sites, serve a folder
// (or a registered site by name) on loopback with live reload, and `expose`
// a site through caddy via the owned include file + one-time bootstrap.
// Board, guest tiers, and auth arrive in later slices.

use anyhow::Context;
use clawde_katban::doctor::Finding;
use std::path::{Path, PathBuf};

pub(crate) const USAGE: &str = r#"Usage: clawde katban <command> [OPTIONS]

Katban development workspace: boards, agent execution, project repos,
and hosted development sites.

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
  status                                      Overview of sites, boards, caddy config
  doctor [--caddy-dir DIR] [--fix] [--strict] Diagnose drift vs the installed host
  caddy reload [--caddy-dir DIR]              Validate the whole Caddyfile, then reload caddy
  help                                        Show this help

Cat Chat is a separate public-facing sandbox:
  clawde catchat serve|expose|unblock|links ...

Katban state lives in ~/.clawde/katban/ and Cat Chat state lives in
~/.clawde/catchat/ (CLAWDE_HOME overrides). Both servers default to
127.0.0.1; binding beyond loopback requires --allow-non-loopback.
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
        "status" => run_status(),
        "doctor" => run_doctor(&args[1..]),
        "caddy" => run_caddy(&args[1..]),
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        other => anyhow::bail!("unknown katban command: {other}\n\n{USAGE}"),
    }
}

/// `clawde catchat` — the Cat Chat (guest chat) command group. Owns the
/// chat-facing surface: the guest server, its public exposure, lockout
/// unblocks, and link management. `clawde katban` keeps only the Kanban
/// board/project/site surface (its `link`/`guest` entries are aliases).
pub(crate) const CATCHAT_USAGE: &str = r#"Usage: clawde catchat <command> [OPTIONS]

Cat Chat: the password-protected guest chat you share with friends.

Commands:
  serve [--port N] [--host IP] [--allow-non-loopback]
        [--search-endpoint URL] [--commands SPEC] [--models LIST]
                                              Run the Cat Chat server
                                              (guest search defaults to the
                                              local SearXNG at 127.0.0.1:8080;
                                              override with the flag or
                                              CLAWDE_CATCHAT_SEARCH_ENDPOINT;
                                              --commands picks the guest slash
                                              vocabulary (all|none|names) and
                                              --models the models a guest may
                                              pick — both also read from
                                              CLAWDE_CATCHAT_COMMANDS /
                                              CLAWDE_CATCHAT_MODELS)
  expose [--subdomain HOST] [--port N] [--dry-run] [--caddy-dir DIR]
         [--duckdns-token TOKEN]              Put Cat Chat behind caddy
                                              (writes the managed katban.conf)
  links create <NAME> [--expires-in DAYS|never] [--max-concurrent N]
                                              Create a guest link (prints the URL + one-time password)
  links list                                  List guest links
  links show <ID>                             Show one link (URL, expiry, devices)
  links revoke <ID>                           Revoke a link (kicks its devices)
  links password <ID> [--set "PW"]            Rotate the password (generated, or --set your own)
  unblock <IP|loopback>                       Clear a lockout / 24h block. The
                                              argument is the bucket `status`
                                              names; a local client is 'loopback'
  status                                      Overview of links, exposure, lockouts
  help                                        Show this help

Friends open the URL, type the shared password, and chat with Clawde (chat
+ web search only — no files, no shell, nothing else). Cat Chat rides the
host's free/limited providers; if none are configured the chat explains
that politely.
"#;

pub async fn run_catchat_command(args: &[String]) -> anyhow::Result<()> {
    let Some(command) = args.first().map(|s| s.as_str()) else {
        print!("{CATCHAT_USAGE}");
        return Ok(());
    };
    match command {
        "serve" => catchat_serve(&args[1..]).await,
        "expose" => run_catchat_expose(&args[1..]).await,
        "links" => run_link(&args[1..]),
        "unblock" => run_catchat_unblock(&args[1..]),
        "status" => catchat_status(),
        "help" | "--help" | "-h" => {
            print!("{CATCHAT_USAGE}");
            Ok(())
        }
        other => anyhow::bail!("unknown catchat command: {other}\n\n{CATCHAT_USAGE}"),
    }
}

/// Cat Chat status overview: links, public exposure, lockout state.
fn catchat_status() -> anyhow::Result<()> {
    use clawde_katban::catchat::links::{link_active, load};
    let store = load()?;
    let now = now_secs();
    let active = store.links.iter().filter(|l| link_active(l, now)).count();
    let total = store.links.len();
    let devices: usize = store.devices.values().map(|d| d.len()).sum();
    println!("links:      {active} active / {total} total");
    println!("devices:    {devices} authenticated");
    match &store.public_subdomain {
        Some(sub) => println!("public:     https://{sub} (via caddy)"),
        None => println!("public:     (local only — run: clawde catchat expose)"),
    }
    // Name the buckets, not a count: a locked-out loopback client is filed
    // under 'loopback', which the operator cannot guess from an address, and
    // that is exactly what `unblock` has to be given.
    let lockouts = store.active_lockouts(now);
    if lockouts.is_empty() {
        println!("lockouts:   none");
    } else {
        println!(
            "lockouts:   {}",
            clawde_katban::http_security::describe_lockouts(&lockouts, now)
        );
        println!("            clear with: clawde catchat unblock <BUCKET>");
    }
    Ok(())
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
    } else if clawde_katban::catchat::links::load()
        .is_ok_and(|store| store.public_subdomain.is_some())
    {
        println!("note: run 'clawde catchat expose' to regenerate the managed caddy config without this site");
    }
    Ok(())
}

/// Resolve `serve`'s argument: a registered site name, else a directory.
/// Returns (root, port, live).
/// Where `site serve` should host, and which port the site is *registered*
/// with when it was named rather than given as a directory.
///
/// The two ports differ only when a `--port` override is used, and that
/// difference is how a site answers directly but 502s through caddy: the
/// managed block `site expose` renders proxies the registered port, never the
/// override. Keeping the registered port in the answer lets the command warn
/// instead of leaving the operator to find out from a broken page.
struct ServeTarget {
    root: PathBuf,
    port: u16,
    live: bool,
    registered_port: Option<u16>,
}

fn resolve_serve_target(
    config: &clawde_katban::config::KatbanConfig,
    arg: &str,
    port_override: Option<u16>,
    no_reload: bool,
    locked_override: bool,
) -> anyhow::Result<ServeTarget> {
    if let Some(site) = find_site(config, arg) {
        let live = !(site.locked || no_reload || locked_override);
        return Ok(ServeTarget {
            root: site.root.clone(),
            port: port_override.unwrap_or(site.port),
            live,
            registered_port: Some(site.port),
        });
    }
    let root = clawde_katban::config::canonical_site_root(Path::new(arg))?;
    Ok(ServeTarget {
        root,
        port: port_override.unwrap_or(clawde_katban::config::DEFAULT_SITE_PORT),
        live: !no_reload && !locked_override,
        // A bare directory is not a registered site, so there is no registered
        // port for an override to disagree with.
        registered_port: None,
    })
}
async fn site_serve(opts: SiteOpts) -> anyhow::Result<()> {
    use clawde_katban::config::load;
    use clawde_katban::host;

    let arg = opts
        .dir
        .clone()
        .context("site serve needs a site name or directory")?;
    let config = load()?;
    let ServeTarget {
        root,
        port,
        live,
        registered_port,
    } = resolve_serve_target(&config, &arg, opts.port, opts.no_reload, opts.locked)?;
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
    if let Some(registered) = registered_port {
        if port != registered {
            println!(
                "WARNING: '{name}' is registered on port {registered} but serving on {port} — the \
                 caddy block from `site expose` proxies the registered port, so the public URL \
                 will return 502. Serve the registered port, or re-register the site on this one \
                 with: clawde katban site remove {name} && clawde katban site add <DIR> --name \
                 {name} --port {port}"
            );
        }
    }
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

/// The complete managed-config text for a `site expose`: every exposed dev
/// site, the Cat Chat guest block when Cat Chat is exposed, and the admin-board
/// block when the board is exposed.
///
/// `apply_managed_include` replaces the entire file, so every expose path must
/// render the *complete* set — a role omitted here is a role this command
/// silently deletes from caddy. `site expose` used to pass `None` for the
/// guest block, which took `chat.example.com` offline (with a reload) on any
/// host that also exposed Cat Chat.
fn managed_config_text(
    exposed: &[(
        clawde_katban::config::SiteConfig,
        clawde_katban::caddy::SiteKind,
    )],
) -> anyhow::Result<String> {
    let guest = guest_block_for_expose()?;
    let board = admin_board_block()?;
    Ok(clawde_katban::caddy::render_config(
        exposed,
        guest.as_ref().map(|(host, port)| (host.as_str(), *port)),
        board.as_ref().map(|(host, port)| (host.as_str(), *port)),
    ))
}

/// The Cat Chat guest block `(host, port)` when Cat Chat has been exposed.
fn guest_block_for_expose() -> anyhow::Result<Option<(String, u16)>> {
    let store = clawde_katban::catchat::links::load()?;
    Ok(guest_block_from(&store))
}

async fn site_expose(opts: SiteOpts) -> anyhow::Result<()> {
    use clawde_katban::caddy::{katban_bootstrap_instructions, site_kind, DEFAULT_INCLUDE_NAME};
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

    let text = managed_config_text(&exposed)?;
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

    for line in apply_managed_include(&managed_path, &text)? {
        println!("{line}");
    }

    // Katban site exposure only writes the shared Caddy reload units. Cat
    // Chat's service is created exclusively by `clawde catchat expose`.
    let units_dir = write_systemd_units(&caddy_dir, None, (&[], 0))?;

    println!();
    println!("{}", katban_bootstrap_instructions(&units_dir, &caddy_dir));

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

--project NAME picks the board each command below reads or writes; it defaults
to "default". Three commands are board-wide and are the exception: `serve` and
`expose` cover every registered board and REJECT the flag, and `password` sets
the one admin password and IGNORES it.

  serve [--port N] [--host IP] [--allow-non-loopback] [--run NAME,...|all]
                                         Run the admin board web UI (loopback,
                                         every board: no --project);
                                         --run also schedules the listed projects'
                                         cards ('all' = every registered project)
  expose [--subdomain HOST] [--port N] [--dry-run] [--caddy-dir DIR]
                [--duckdns-token TOKEN] [--run NAME,...|all]
                                         Publish the board behind caddy (https,
                                         every board: no --project);
                                         --run renders the always-on board unit
                                         with one scheduler per listed project
  password <PASSWORD>                    Set/rotate the admin write password
                                         (board-wide; --project is ignored)
  init                                   Create/ensure the board file
  card add <PROMPT>                      Add a card
  card list                              List cards with status
  card show <ID>                         One card: status, result, commit, diff
  card set <ID> <STATUS>                 backlog|queued|running|blocked|review|failed|done
  card edit <ID> <PROMPT>                Replace a card's prompt (keeps id/status/reviews)
  card scope <ID> [PATH...]              Set the card's scope allowlist (empty = clear)
  card comment <ID> [--line N] <TEXT>    Append a diff-review comment
  card feedback <ID>                     Send the card's review comments back to
                                         its agent as a follow-up run
  card merge <ID>                        Merge a review card into the project
  card remove <ID>                       Archive (discards its pinned branch)
  link <A> <B>                           B must finish before A starts (cycle-checked)
  unlink <A> <B>
  ready [--cap N]                        Cards that can start now (queue order)
  auto-review on|off                     Toggle the auto-review pass for cards
  verify on|off                          Toggle the verification gate for cards
  attempts <N>                           Run each card up to N times (1-5) on
                                         different free-catalog model families;
                                         first attempt to pass the verify gate wins
  attempts-upstreams [ID...]             Pin the attempts rotation to catalog
                                         upstream ids (empty = auto-derive)
  runtime host|docker                    Where attempts execute: the host
                                         worktree lane (default) or an
                                         ephemeral Docker container per attempt
                                         (in-container verify, FS-manifest
                                         scope gate; requires a docker daemon)
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
/// registered — the always-on unit's command). Mirrors `catchat serve`'s flag
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
    clawde_katban::admin_auth::validate_password(password)
        .map_err(|message| anyhow::anyhow!(message))?;
    let mut store = clawde_katban::admin_auth::load().context("load admin store")?;
    store.set_password(password);
    clawde_katban::admin_auth::save(&store).context("save admin store")?;

    println!(
        "admin board password set at {}",
        clawde_katban::admin_auth::admin_path().display()
    );
    Ok(())
}

/// The admin board's caddy block target: `(subdomain, port)` when the board
/// has been exposed (`board expose`), else `None`. Reads the AdminStore so
/// every expose path (`site expose` / `catchat expose` / `board expose`)
/// regenerates the managed config with the board block intact.
fn admin_board_block() -> anyhow::Result<Option<(String, u16)>> {
    let store = clawde_katban::admin_auth::load().context("load admin store")?;
    Ok(board_block_from(&store))
}

/// The admin board's caddy block `(subdomain, port)` of a loaded store.
fn board_block_from(store: &clawde_katban::admin_auth::AdminStore) -> Option<(String, u16)> {
    let subdomain = store.public_subdomain.clone()?;
    let port = store
        .board_port
        .unwrap_or(clawde_katban::board_server::DEFAULT_BOARD_PORT);
    Some((subdomain, port))
}

/// Publish the admin board behind caddy at an https subdomain, mirroring
/// `catchat expose`. Renders the board block into the managed caddy config,
/// writes the reloader units, prints the one-time bootstrap instructions, and
/// optionally points DuckDNS at the subdomain.
async fn board_expose(args: &[String]) -> anyhow::Result<()> {
    use clawde_katban::board_server::DEFAULT_BOARD_PORT;
    use clawde_katban::caddy::{
        katban_bootstrap_instructions, render_config, DEFAULT_INCLUDE_NAME,
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
    let mut store = clawde_katban::admin_auth::load().context("load admin store")?;
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
        clawde_katban::admin_auth::save(&store)?;
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
    let guest = clawde_katban::catchat::links::load()?
        .public_subdomain
        .map(|host| {
            let port = clawde_katban::catchat::links::load()
                .ok()
                .and_then(|s| s.guest_port)
                .unwrap_or(clawde_katban::catchat::server::DEFAULT_GUEST_PORT);
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

    for line in apply_managed_include(&managed_path, &text)? {
        println!("{line}");
    }

    // Reloader units watch the managed file; the board unit is rendered when
    // runner projects are configured (`--run <NAME,...>` / `--run all`).
    let units_dir = write_systemd_units(&caddy_dir, None, (&runner_projects, port))?;
    println!();
    println!("{}", katban_bootstrap_instructions(&units_dir, &caddy_dir));
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
        "auto-review" => {
            if rest.len() != 1 || !matches!(rest[0].as_str(), "on" | "off") {
                anyhow::bail!("board auto-review needs on or off");
            }
            let enabled = rest[0] == "on";
            let _guard = clawde_katban::board::BoardLock::acquire(project)?;
            let mut board = load_board(project)?.unwrap_or_default();
            board.auto_review = enabled;
            save_board(&board, project)?;
            println!(
                "auto-review {} for board '{project}'",
                if enabled { "enabled" } else { "disabled" }
            );
            Ok(())
        }
        "verify" => {
            if rest.len() != 1 || !matches!(rest[0].as_str(), "on" | "off") {
                anyhow::bail!("board verify needs on or off");
            }
            let enabled = rest[0] == "on";
            let _guard = clawde_katban::board::BoardLock::acquire(project)?;
            let mut board = load_board(project)?.unwrap_or_default();
            board.verify = enabled;
            save_board(&board, project)?;
            println!(
                "verify {} for board '{project}'",
                if enabled { "enabled" } else { "disabled" }
            );
            Ok(())
        }
        "attempts" => {
            // attempts:N ladder (spec §7): run each card up to N times on
            // different free-catalog model families; first verify pass wins.
            if rest.len() != 1 {
                anyhow::bail!(
                    "board attempts needs a number 1-{}",
                    clawde_katban::board::MAX_ATTEMPTS
                );
            }
            let n: u32 = rest[0].parse().context("attempts must be a number")?;
            if !(1..=clawde_katban::board::MAX_ATTEMPTS).contains(&n) {
                anyhow::bail!(
                    "attempts must be 1-{} (got {n})",
                    clawde_katban::board::MAX_ATTEMPTS
                );
            }
            let _guard = clawde_katban::board::BoardLock::acquire(project)?;
            let mut board = load_board(project)?.unwrap_or_default();
            let stored = board.set_attempts(n);
            save_board(&board, project)?;
            println!("attempts {stored} for board '{project}'");
            Ok(())
        }
        "runtime" => {
            // Container tier switch (spec §8): where card attempts execute.
            if rest.len() != 1 {
                anyhow::bail!("board runtime needs 'host' or 'docker'");
            }
            // The lane was built on Incus and moved to Docker (2026-10-13).
            // A stored `"incus"` board migrates on load, but a typed token is
            // user intent — name the new value instead of a bare "unknown".
            if rest[0].eq_ignore_ascii_case("incus") {
                anyhow::bail!(
                    "the container tier moved from Incus to Docker — use `board runtime docker` \
                     (a board already storing 'incus' loads as the docker lane)"
                );
            }
            let Some(runtime) = clawde_katban::board::ContainerRuntime::parse(&rest[0]) else {
                anyhow::bail!("runtime needs 'host' or 'docker' (got '{}')", rest[0]);
            };
            if runtime == clawde_katban::board::ContainerRuntime::Docker
                && !clawde_katban::container::available()
            {
                anyhow::bail!(
                    "docker is not reachable (`docker info` failed) — is dockerd running?"
                );
            }
            let _guard = clawde_katban::board::BoardLock::acquire(project)?;
            let mut board = load_board(project)?.unwrap_or_default();
            // The docker tier IS the attempts ladder, containerized (spec §8),
            // and `attempts` defaults to 1: with no rungs there is nothing to
            // containerize, so every card would fail on an empty ladder with
            // only the generic "all attempts failed". Refuse at the switch,
            // where the fix is obvious, instead of per card.
            if runtime == clawde_katban::board::ContainerRuntime::Docker && board.attempts < 2 {
                anyhow::bail!(
                    "the docker tier runs the attempts ladder in containers, so it needs at \
                     least 2 attempts (this board has {}) — run: `clawde katban board attempts 2`, \
                     then either `clawde katban board attempts-upstreams <ID>` or configure free keys",
                    board.attempts
                );
            }
            let stored = board.set_runtime(runtime);
            save_board(&board, project)?;
            println!("runtime {} for board '{project}'", stored.as_str());
            Ok(())
        }
        "attempts-upstreams" => {
            // Pin the ladder's rotation explicitly; empty = auto-derive from
            // the keyed free catalog (spec §6).
            let _guard = clawde_katban::board::BoardLock::acquire(project)?;
            let mut board = load_board(project)?.unwrap_or_default();
            if rest.is_empty() {
                board.attempt_upstreams.clear();
                save_board(&board, project)?;
                println!(
                    "attempts-upstreams auto (derived from the free catalog) for board '{project}'"
                );
            } else {
                for id in &rest {
                    if clawde_api::providers::free::catalog_entry(id.as_str()).is_none() {
                        anyhow::bail!("unknown upstream '{id}' — not in the free catalog");
                    }
                }
                board.attempt_upstreams = rest.to_vec();
                save_board(&board, project)?;
                println!(
                    "attempts-upstreams {} for board '{project}'",
                    board.attempt_upstreams.join(", ")
                );
            }
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
        anyhow::bail!(
            "board card needs an action: add|list|set|edit|scope|merge|remove|comment|feedback|show"
        );
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
        "scope" => {
            // Scope allowlist (spec §5/§8): enforced by the FS-manifest scope
            // gate on the container tier. Empty = clear (no scope opinion).
            let Some(id) = rest.first() else {
                anyhow::bail!("board card scope needs an id: card scope <ID> [PATH...]");
            };
            let paths: Vec<String> = rest[1..].to_vec();
            let _guard = clawde_katban::board::BoardLock::acquire(project)?;
            let mut board = load_board(project)?.unwrap_or_default();
            if !board.set_card_scope(id, paths) {
                anyhow::bail!("no such card: {id}");
            }
            let stored = board
                .card(id)
                .map(|c| c.scope_paths.clone())
                .unwrap_or_default();
            save_board(&board, project)?;
            if stored.is_empty() {
                println!("scope cleared for card {id} (no scope opinion)");
            } else {
                println!("scope for card {id}: {}", stored.join(", "));
            }
            Ok(())
        }
        "set" => {
            if rest.len() < 2 {
                anyhow::bail!("board card set needs an id and a status");
            }
            let status = CardStatus::parse(&rest[1])
                .with_context(|| format!("unknown status '{}'", rest[1]))?;
            // Setting a card done without merging is an explicit discard: if the
            // card has a pinned commit (a reviewed card), its `katban/<id>`
            // branch must be cleaned up too — a bare Done would leak the branch
            // forever. `discard_card` locks, deletes the branch (if any), and
            // marks the card done.
            if status == CardStatus::Done {
                clawde_katban::commit::discard_card(project, &rest[0])
                    .map_err(|e| anyhow::anyhow!(e))?;
                println!("'{}' -> done (branch cleaned up)", rest[0]);
                return Ok(());
            }
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
        "edit" => {
            // `board card edit <ID> <PROMPT>` — replace the prompt, mirroring
            // the web UI's edit action. The card keeps its id, status,
            // reviews, dependencies and pinned branch.
            if rest.len() < 2 {
                anyhow::bail!("board card edit needs an id and a new prompt");
            }
            let prompt = rest[1..].join(" ").trim().to_string();
            if prompt.is_empty() {
                anyhow::bail!("board card edit prompt must not be empty");
            }
            let _guard = clawde_katban::board::BoardLock::acquire(project)?;
            let mut board = load_board(project)?.unwrap_or_default();
            if board.update_prompt(&rest[0], &prompt) {
                save_board(&board, project)?;
                println!("'{}' prompt updated", rest[0]);
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
        "comment" => {
            // `board card comment <ID> [--line N] <TEXT>` — append a diff-review
            // comment (spec §16a E5). The optional line anchor is advisory; text
            // is everything after the id and optional --line.
            if rest.is_empty() {
                anyhow::bail!(
                    "board card comment needs an id: board card comment <ID> [--line N] <TEXT>"
                );
            }
            let id = rest[0].clone();
            let mut index = 1;
            let mut location = None;
            let mut text_parts: Vec<&str> = Vec::new();
            while index < rest.len() {
                if rest[index] == "--line" {
                    location = Some(
                        rest.get(index + 1)
                            .cloned()
                            .context("--line needs a value")?,
                    );
                    index += 2;
                } else if let Some(value) = rest[index].strip_prefix("--line=") {
                    location = Some(value.to_string());
                    index += 1;
                } else {
                    text_parts.push(&rest[index]);
                    index += 1;
                }
            }
            let text = text_parts.join(" ").trim().to_string();
            if text.is_empty() {
                anyhow::bail!("board card comment needs comment text");
            }
            let new_id = clawde_katban::board::add_review(project, &id, location, &text)
                .map_err(|e| anyhow::anyhow!(e))?;
            println!("comment {new_id} added to '{id}'");
            Ok(())
        }
        "feedback" => {
            // `board card feedback <ID>` — send the card's review comments back
            // to its agent as a follow-up run: requeues + appends them to the
            // prompt (spec §16a E5).
            if rest.is_empty() {
                anyhow::bail!("board card feedback needs an id");
            }
            let count = clawde_katban::board::send_feedback_to_agent(project, &rest[0])
                .map_err(|e| anyhow::anyhow!(e))?;
            println!(
                "sent {} comment(s) back to '{}' — card requeued for a follow-up run",
                count, rest[0]
            );
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
    } else if card.diff.is_some() {
        println!("merge:    unavailable (diff-only review; no pinned commit)");
    }
    if let Some(kind) = card.failure_kind {
        println!("failure:  {}", failure_kind_name(kind));
    }
    if let Some(result) = &card.result {
        println!("result:");
        println!("{result}");
    }
    if let Some(summary) = &card.diff_summary {
        println!(
            "diff summary: {} file(s), +{}, -{}",
            summary.files_changed, summary.additions, summary.deletions
        );
    }
    if let Some(diff) = &card.diff {
        println!("diff ({} ch):", diff.len());
        println!("{diff}");
    }
    if !card.reviews.is_empty() {
        println!("reviews:");
        for r in &card.reviews {
            match &r.location {
                Some(loc) => println!("  [L{loc}] {}", r.text),
                None => println!("  {}", r.text),
            }
        }
    }
    if let Some(fb) = &card.followup_feedback {
        println!("pending feedback (sent to agent on the next run):");
        println!("{fb}");
    }
    Ok(())
}

fn failure_kind_name(kind: clawde_katban::board::FailureKind) -> &'static str {
    match kind {
        clawde_katban::board::FailureKind::Agent => "agent",
        clawde_katban::board::FailureKind::Verification => "verification",
        clawde_katban::board::FailureKind::Worktree => "worktree",
        clawde_katban::board::FailureKind::Commit => "commit",
    }
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
pub use clawde_katban::admin_auth::RUN_ALL;

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

/// Resolve the binary an always-on unit should pin. Whatever build invoked
/// the expose is `current_exe()` — if that is a debug build, prefer the
/// sibling release binary, because a debug path is enormous and is removed by
/// `cargo clean`, leaving a pinned unit that can no longer start.
fn resolve_unit_binary() -> (PathBuf, bool) {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok())
        .unwrap_or_else(|| PathBuf::from("clawde"));
    prefer_release_binary(exe)
}

/// Prefer `.../target/release/clawde` over `.../target/debug/clawde` when the
/// former exists. Returns the chosen path and whether it is still a debug
/// build, so the caller can warn instead of silently pinning one.
fn prefer_release_binary(exe: PathBuf) -> (PathBuf, bool) {
    let is_debug = exe
        .parent()
        .and_then(|parent| parent.file_name())
        .is_some_and(|name| name == "debug");
    if is_debug {
        let release = exe.file_name().and_then(|name| {
            exe.parent()
                .and_then(Path::parent)
                .map(|target| target.join("release").join(name))
        });
        if let Some(release) = release {
            if release.is_file() {
                return (release, false);
            }
        }
    }
    (exe, is_debug)
}

/// Render + write `catchat.service` (Cat Chat's always-on sandbox server)
/// and the shared caddy reloader units. Returns the directory the files were
/// written to. The binary path is resolved from the currently running clawde,
/// so the unit always starts the exact build the admin is using; rebuild in
/// place and restart `catchat.service` to update. `caddy_dir` is where the
/// managed include lives (the reloader must watch that exact file) and
/// `guest_port` is the Cat Chat port. `board` is the independent Katban runner
/// project list + board port when `katban-board.service` should be always-on;
/// an empty list means no board unit is rendered.
fn write_systemd_units(
    caddy_dir: &Path,
    catchat_port: Option<u16>,
    board: (&[String], u16),
) -> anyhow::Result<PathBuf> {
    let dir = units_dir();
    std::fs::create_dir_all(&dir)?;
    let (binary, debug_pinned) = resolve_unit_binary();
    if debug_pinned {
        eprintln!(
            "warning: rendering an always-on unit pinned to a debug build ({}); \
             `cargo clean` or a rebuild removes it — build with `cargo build --release` \
             and re-run this command",
            binary.display()
        );
    }
    let user = unit_user()?;
    if let Some(port) = catchat_port {
        std::fs::write(
            dir.join("catchat.service"),
            clawde_katban::caddy::render_service_unit(&binary.display().to_string(), &user, port),
        )?;
    }
    std::fs::write(
        dir.join("katban-reload.path"),
        clawde_katban::caddy::render_reloader_path_unit(
            &caddy_dir.join(clawde_katban::caddy::DEFAULT_INCLUDE_NAME),
        ),
    )?;
    // The watcher runs the *checked* reload (`clawde katban caddy reload`), so
    // the reload flavour is re-detected when it fires instead of being baked in
    // here — switching to a socket-scoped admin endpoint later needs no
    // re-render. It does mean the unit needs this binary to exist, which
    // `doctor` reports.
    std::fs::write(
        dir.join("katban-reload.service"),
        clawde_katban::caddy::render_reloader_service_unit(
            &binary.display().to_string(),
            caddy_dir,
        ),
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

const DOCTOR_USAGE: &str = r#"Usage: clawde katban doctor [OPTIONS]

Diagnose drift between the persisted Katban/Cat Chat config and what the host
actually has installed: the managed caddy include, the Caddyfile import, every
exposed route's loopback backend, the always-on systemd units (including the
port each serves and the caddy reload watcher), and whether caddy is running.
A store that will not parse is reported as a finding.

  --caddy-dir DIR   Managed caddy include dir (default /etc/caddy)
  --fix             Rewrite the managed include from desired state and reload caddy
  --strict          Exit non-zero when any error is found
  help              Show this help
"#;

const CADDY_USAGE: &str = r#"Usage: clawde katban caddy <command> [OPTIONS]

Katban's view of the host caddy install: the managed include it writes and the
reload that makes it live.

Commands:
  reload [--caddy-dir DIR]   Validate the whole Caddyfile with `caddy adapt`,
                             then reload caddy — refusing to reload a config
                             that no longer adapts
  help                       Show this help

This is what the installed `katban-reload.path` watcher runs, so a fragment
written by anything (a second tool, a hand edit, a partial write) is reported
instead of being applied and left to fail every later reload. For the read-only
comparison of the include against desired state, use `clawde katban doctor`.
"#;

/// `clawde katban caddy` — the reload path, kept separate from `doctor`
/// (which only reports).
fn run_caddy(args: &[String]) -> anyhow::Result<()> {
    let Some(subcommand) = args.first().map(|s| s.as_str()) else {
        print!("{CADDY_USAGE}");
        return Ok(());
    };
    match subcommand {
        "reload" => run_caddy_reload(&args[1..]),
        "help" | "--help" | "-h" => {
            print!("{CADDY_USAGE}");
            Ok(())
        }
        other => anyhow::bail!("unknown katban caddy command: {other}\n\n{CADDY_USAGE}"),
    }
}

/// `clawde katban caddy reload` — validate the whole Caddyfile, then reload.
///
/// The order is the point, and it is why the installed `katban-reload.service`
/// runs *this* rather than a bare `systemctl reload caddy`: caddy keeps the
/// last-good config when a reload fails, so a fragment that no longer adapts
/// cannot take the proxy down — but nothing stops that fragment from sitting on
/// disk, where it keeps every later reload failing until a human notices.
/// Refusing here means an unhealthy config is never applied, and the journal
/// says why. A hand edit and a second tool writing the include are covered too;
/// `expose` is not the only writer.
///
/// Exits non-zero on a refusal or a failed reload so the systemd unit records a
/// failure instead of reporting success.
fn run_caddy_reload(args: &[String]) -> anyhow::Result<()> {
    let mut caddy_dir = PathBuf::from("/etc/caddy");
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--caddy-dir" => {
                index += 1;
                caddy_dir = PathBuf::from(args.get(index).context("--caddy-dir needs a value")?);
            }
            flag if flag.starts_with("--caddy-dir=") => {
                caddy_dir = PathBuf::from(&flag["--caddy-dir=".len()..]);
            }
            "help" | "--help" | "-h" => {
                print!("{CADDY_USAGE}");
                return Ok(());
            }
            flag => anyhow::bail!("unknown option: {flag}\n\n{CADDY_USAGE}"),
        }
        index += 1;
    }

    let managed_path = caddy_dir.join(clawde_katban::caddy::DEFAULT_INCLUDE_NAME);
    let caddyfile = caddyfile_for(&managed_path);
    if !caddyfile.is_file() {
        anyhow::bail!(
            "nothing to reload: no Caddyfile at {} — the managed include in {} is not loaded \
             from here",
            caddyfile.display(),
            caddy_dir.display()
        );
    }

    match validate_caddyfile(&caddyfile) {
        Validation::Valid => println!("validated {} with `caddy adapt`", caddyfile.display()),
        Validation::Skipped => println!(
            "note: `caddy` is not on PATH — skipped validation; the reload below still decides"
        ),
        Validation::Invalid(error) => anyhow::bail!(
            "refused to reload: {} no longer adapts — caddy keeps serving the last-good config, \
             and this file will keep every later reload failing until it is fixed\n{error}",
            caddyfile.display()
        ),
    }

    let endpoint = std::fs::read_to_string(&caddyfile)
        .map(|text| admin_endpoint(&text))
        .unwrap_or(AdminEndpoint::Unset);
    let plan = reload_plan(&caddyfile, &endpoint);
    if let Some(note) = plan.note {
        println!("{note}");
    }
    let (ok, line) = run_reload(&plan.argv);
    println!("{line}");
    if !ok {
        anyhow::bail!("caddy reload failed");
    }
    Ok(())
}

/// Where the bootstrap installs the always-on units. `diagnose` takes the
/// directory as an argument so the checks stay testable against a scratch one.
const SYSTEMD_UNITS_DIR: &str = "/etc/systemd/system";

/// Everything `doctor` compares, gathered up front so a store that will not
/// parse is a finding rather than an abort.
struct Diagnosis {
    managed_path: PathBuf,
    /// Desired managed-include text, or `None` when the persisted state that
    /// produces it could not be read.
    desired: Option<String>,
    /// The managed include as it is on disk, or `None` when it is missing.
    actual: Option<String>,
    /// The directory holds a Caddyfile or the managed include. `--fix` refuses
    /// to write anywhere else, so a typo'd `--caddy-dir` cannot leave a stray
    /// `katban.conf` behind.
    can_repair: bool,
    findings: Vec<Finding>,
}

impl Diagnosis {
    fn include_matches(&self) -> bool {
        matches!((&self.desired, &self.actual), (Some(desired), Some(actual)) if desired == actual)
    }
}

/// `clawde katban doctor` — compare the persisted desired state (exposed
/// sites, Cat Chat links, admin board) against the host: the managed caddy
/// include, the Caddyfile `import`, every exposed route's loopback backend,
/// the always-on units, and the caddy reload watcher. Every failure this
/// session hit (stale include, unit pinned to a deleted binary, unguarded
/// import, dead port) surfaces here as one line.
fn run_doctor(args: &[String]) -> anyhow::Result<()> {
    let mut caddy_dir = PathBuf::from("/etc/caddy");
    let mut fix = false;
    let mut strict = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--caddy-dir" => {
                index += 1;
                caddy_dir = PathBuf::from(args.get(index).context("--caddy-dir needs a value")?);
            }
            flag if flag.starts_with("--caddy-dir=") => {
                caddy_dir = PathBuf::from(&flag["--caddy-dir=".len()..]);
            }
            "--fix" => fix = true,
            "--strict" => strict = true,
            "help" | "--help" | "-h" => {
                print!("{DOCTOR_USAGE}");
                return Ok(());
            }
            flag => anyhow::bail!("unknown option: {flag}\n\n{DOCTOR_USAGE}"),
        }
        index += 1;
    }

    use clawde_katban::doctor::Level;

    let units_dir = Path::new(SYSTEMD_UNITS_DIR);
    let mut diagnosis = diagnose(&caddy_dir, units_dir, caddy_states());
    if fix {
        match (&diagnosis.desired, &diagnosis.actual) {
            (None, _) => println!(
                "cannot rewrite the managed include: the persisted state could not be read \
                 (see the findings below)"
            ),
            (Some(_), _) if diagnosis.include_matches() => {
                println!("managed include already matches desired state — nothing to fix")
            }
            (Some(_), _) if !diagnosis.can_repair => println!(
                "refusing to write {}: no Caddyfile and no managed include in {} — check \
                 --caddy-dir",
                diagnosis.managed_path.display(),
                caddy_dir.display()
            ),
            (Some(desired), _) => {
                // A repair that cannot adapt is rolled back, so report the
                // failure and keep the findings instead of aborting the run.
                match apply_managed_include(&diagnosis.managed_path, desired) {
                    Ok(lines) => {
                        for line in lines {
                            println!("{line}");
                        }
                    }
                    Err(error) => println!("fix failed: {error}"),
                }
                // Re-diagnose so the report — and the `--strict` decision —
                // describe the repaired host, not the one just fixed.
                diagnosis = diagnose(&caddy_dir, units_dir, caddy_states());
            }
        }
    }

    println!("Katban / Cat Chat doctor");
    println!("managed include: {}", diagnosis.managed_path.display());
    println!();
    for finding in &diagnosis.findings {
        let tag = match finding.level {
            Level::Ok => "ok   ",
            Level::Warn => "warn ",
            Level::Error => "ERROR",
        };
        println!("[{tag}] {:<22} {}", finding.subject, finding.message);
    }
    let errors = diagnosis
        .findings
        .iter()
        .filter(|f| f.level == Level::Error)
        .count();
    let warnings = diagnosis
        .findings
        .iter()
        .filter(|f| f.level == Level::Warn)
        .count();
    println!();
    println!("{errors} error(s), {warnings} warning(s)");

    if strict && errors > 0 {
        anyhow::bail!("{errors} doctor error(s) (--strict)");
    }
    Ok(())
}

/// The Caddyfile that imports the managed include in `caddy_dir`.
fn caddyfile_for(managed_path: &Path) -> PathBuf {
    managed_path
        .parent()
        .unwrap_or_else(|| Path::new("/etc/caddy"))
        .join("Caddyfile")
}

/// What the Caddyfile's global block says about caddy's admin endpoint.
///
/// This is the switch that decides whether a reload can be done without root:
/// the admin API is unauthenticated (any local process may POST a full config),
/// so the trusted default stays `systemctl reload caddy` and the rootless path
/// is used only when the operator scoped the endpoint to a unix socket, where
/// socket permissions do the access control (spec §10.1a's documented fallback).
#[derive(Debug, Clone, PartialEq, Eq)]
enum AdminEndpoint {
    /// No `admin` directive: caddy's default (TCP `localhost:2019`).
    Unset,
    /// `admin off` — no admin API at all; only a service reload works.
    Off,
    /// `admin unix/...` — rootless reloads are safe here.
    Unix(String),
    /// `admin <host:port>` — the exposed-by-default TCP endpoint.
    Tcp(String),
}

/// Parse the global `admin` directive out of a Caddyfile. Pure, so the
/// decision is unit-testable; `admin` is a global option, so scanning every
/// line is enough (a site block cannot set it).
fn admin_endpoint(caddyfile: &str) -> AdminEndpoint {
    for line in caddyfile.lines() {
        let code = line.split('#').next().unwrap_or("").trim();
        let Some(rest) = code.strip_prefix("admin") else {
            continue;
        };
        // `admin` must be its own token — `administration` is not the directive.
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            continue;
        }
        let Some(value) = rest.split_whitespace().next() else {
            continue;
        };
        return match value {
            "off" => AdminEndpoint::Off,
            token if token.starts_with("unix/") => AdminEndpoint::Unix(token.to_string()),
            token => AdminEndpoint::Tcp(token.to_string()),
        };
    }
    AdminEndpoint::Unset
}

/// How this host's reload will be performed, plus why.
struct ReloadPlan {
    argv: Vec<String>,
    /// Printed before the outcome when the choice is not the plain default.
    note: Option<String>,
}

/// The reload command for this host.
///
/// The admin API is used **only** through a unix socket named by this Caddyfile,
/// because the default TCP endpoint is unauthenticated and host-global: posting
/// *another* directory's Caddyfile to `localhost:2019` would replace the running
/// proxy's whole config. A socket path, by contrast, belongs to the install that
/// names it, so a scratch `--caddy-dir` can only ever fail to connect.
///
/// `CLAWDE_CADDY_RELOAD` tunes the choice: `systemd` forces the service reload,
/// `caddy` asks for the rootless admin-API path (refused, with a note, unless
/// this Caddyfile scopes the endpoint to a socket), unset auto-detects.
fn reload_plan(caddyfile: &Path, endpoint: &AdminEndpoint) -> ReloadPlan {
    let requested = std::env::var("CLAWDE_CADDY_RELOAD").unwrap_or_default();
    let service = || ReloadPlan {
        argv: vec!["systemctl".into(), "reload".into(), "caddy".into()],
        note: None,
    };
    match endpoint {
        AdminEndpoint::Unix(address) => {
            let mut argv = vec![
                "caddy".into(),
                "reload".into(),
                "--config".into(),
                caddyfile.display().to_string(),
            ];
            argv.push("--address".into());
            argv.push(address.clone());
            ReloadPlan { argv, note: None }
        }
        other => {
            let mut plan = service();
            if requested == "caddy" {
                let found = match other {
                    AdminEndpoint::Off => "`admin off`".to_string(),
                    AdminEndpoint::Tcp(address) => format!("a TCP admin endpoint ({address})"),
                    _ => "no `admin` directive".to_string(),
                };
                plan.note = Some(format!(
                    "note: CLAWDE_CADDY_RELOAD=caddy needs a socket-scoped admin endpoint \
                     (`admin unix//...`); this Caddyfile has {found} — the unauthenticated TCP \
                     admin API is never used, reloading via the service instead"
                ));
            }
            plan
        }
    }
}

/// The outcome of `caddy adapt` over the whole Caddyfile.
enum Validation {
    /// The file adapts: the config on disk is loadable.
    Valid,
    /// caddy refused it; the string is caddy's own diagnostic.
    Invalid(String),
    /// No `caddy` binary to ask — validation is best-effort.
    Skipped,
}

/// `caddy adapt` parses and adapts the Caddyfile without starting anything and
/// without root (measured: ~50 ms). This is what makes a bad fragment
/// detectable *before* it is persisted.
fn validate_caddyfile(caddyfile: &Path) -> Validation {
    match std::process::Command::new("caddy")
        .arg("adapt")
        .arg("--config")
        .arg(caddyfile)
        .output()
    {
        Ok(output) if output.status.success() => Validation::Valid,
        Ok(output) => {
            Validation::Invalid(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
        Err(_) => Validation::Skipped,
    }
}

/// The most useful line of a failed reload's stderr. caddy logs JSON, where
/// every line looks alike and only the error is worth showing; systemctl prints
/// plain text, where the last line carries the verdict.
fn reload_error_line(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let chosen = lines
        .iter()
        .rev()
        .find(|line| line.contains("\"level\":\"error\"") || line.starts_with("Error"))
        .or_else(|| lines.last())
        .copied()
        .unwrap_or("(no output)");
    chosen.trim().chars().take(300).collect()
}

/// Execute a reload command, returning whether it succeeded plus a printable
/// status line. Output is captured rather than inherited so a reload failure
/// reads as one line instead of spraying caddy's JSON over the command's own
/// report. The flag is what lets `caddy reload` exit non-zero for systemd while
/// `expose` still reports the failure inline.
fn run_reload(argv: &[String]) -> (bool, String) {
    let Some((program, args)) = argv.split_first() else {
        return (false, "warning: no reload command".to_string());
    };
    let shown = argv.join(" ");
    match std::process::Command::new(program).args(args).output() {
        Ok(output) if output.status.success() => (true, format!("reloaded caddy via `{shown}`")),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let hint = if program == "systemctl" {
                " — that needs root: the installed katban-reload.path watcher runs it as root, or \
                 scope caddy's admin endpoint to a unix socket and set CLAWDE_CADDY_RELOAD=caddy"
            } else {
                ""
            };
            (
                false,
                format!(
                    "warning: `{shown}` failed ({}){hint}: {}",
                    output.status,
                    reload_error_line(&stderr)
                ),
            )
        }
        Err(error) => (false, format!("warning: could not run `{shown}`: {error}")),
    }
}

/// Persist the managed include, then make it live: write, validate the whole
/// Caddyfile, reload — in that order, and roll the write back if caddy can no
/// longer adapt the result.
///
/// The rollback is the point. Caddy itself keeps the last-good config when a
/// reload fails, so a bad fragment cannot take the proxy down (spec §10.1a) —
/// but nothing today stops that fragment from being *persisted*, where it keeps
/// every later reload failing until a human notices. Validating before the
/// reload and restoring the previous bytes means an unadaptable state is never
/// left on disk.
fn apply_managed_include(managed_path: &Path, desired: &str) -> anyhow::Result<Vec<String>> {
    let previous = std::fs::read_to_string(managed_path).ok();
    clawde_katban::caddy::write_atomic(managed_path, desired)?;
    let mut report = vec![format!(
        "wrote {} from desired state",
        managed_path.display()
    )];

    let caddyfile = caddyfile_for(managed_path);
    if !caddyfile.is_file() {
        report.push(format!(
            "note: no Caddyfile at {} — nothing validates or reloads this include yet",
            caddyfile.display()
        ));
        return Ok(report);
    }

    match validate_caddyfile(&caddyfile) {
        Validation::Valid => report.push("validated (caddy adapt)".to_string()),
        Validation::Skipped => report.push(
            "note: `caddy` is not on PATH — skipped validation (the reload below still decides)"
                .to_string(),
        ),
        Validation::Invalid(error) => {
            match &previous {
                Some(text) => {
                    clawde_katban::caddy::write_atomic(managed_path, text)?;
                }
                None => {
                    std::fs::remove_file(managed_path).ok();
                }
            }
            anyhow::bail!(
                "refused to keep this change: the Caddyfile no longer adapts\n{error}\n\
                 (the managed include was restored to its previous contents)"
            );
        }
    }

    let endpoint = std::fs::read_to_string(&caddyfile)
        .map(|text| admin_endpoint(&text))
        .unwrap_or(AdminEndpoint::Unset);
    let plan = reload_plan(&caddyfile, &endpoint);
    if let Some(note) = plan.note {
        report.push(note);
    }
    let (_, line) = run_reload(&plan.argv);
    report.push(line);
    Ok(report)
}

/// caddy's `LoadState`/`ActiveState`, from one `systemctl show`. `None` when
/// systemd is absent or has no caddy unit: the reverse proxy may be run some
/// other way (a container, a snap, another unit name), and guessing would turn
/// a working host into a false alarm.
fn caddy_states() -> Option<(String, String)> {
    let output = std::process::Command::new("systemctl")
        .args([
            "show",
            "caddy.service",
            "-p",
            "LoadState",
            "-p",
            "ActiveState",
            "--value",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let load_state = lines.next()?.to_string();
    let active_state = lines.next()?.to_string();
    Some((load_state, active_state))
}

/// Gather the facts and grade them. Never fails: unreadable state, a missing
/// include and an absent Caddyfile are findings, not errors that abort the
/// report — a corrupted `katban.json` is exactly what doctor must name.
/// `caddy` is the `(LoadState, ActiveState)` pair of the caddy unit, gathered
/// by the caller so the checks stay hermetic in tests.
fn diagnose(caddy_dir: &Path, units_dir: &Path, caddy: Option<(String, String)>) -> Diagnosis {
    use clawde_katban::board_server::DEFAULT_BOARD_PORT;
    use clawde_katban::caddy::{site_kind, SiteKind, DEFAULT_INCLUDE_NAME};
    use clawde_katban::catchat::server::DEFAULT_GUEST_PORT;
    use clawde_katban::doctor::{
        check_caddy_service, check_imports, check_listener, check_managed_include,
        check_reloader_path_unit, check_reloader_service_unit, check_service_unit, check_site,
        check_unit_port, unit_port_flag, Level, SiteTarget,
    };

    let managed_path = caddy_dir.join(DEFAULT_INCLUDE_NAME);
    let actual = std::fs::read_to_string(&managed_path).ok();
    let mut findings = Vec::new();

    // Persisted state. Outer `None` = the store is unreadable; inner `None` =
    // the role is not exposed.
    let sites = match clawde_katban::config::load() {
        Ok(config) => Some(config.sites),
        Err(error) => {
            findings.push(Finding::new(
                Level::Error,
                "katban.json",
                format!("unreadable: {error} — exposed sites cannot be compared until it parses"),
            ));
            None
        }
    };
    let guest = match clawde_katban::catchat::links::load() {
        Ok(store) => Some(guest_block_from(&store)),
        Err(error) => {
            findings.push(Finding::new(
                Level::Error,
                "catchat links.json",
                format!("unreadable: {error}"),
            ));
            None
        }
    };
    let board = match clawde_katban::admin_auth::load() {
        Ok(store) => Some(board_block_from(&store)),
        Err(error) => {
            findings.push(Finding::new(
                Level::Error,
                "admin.json",
                format!("unreadable: {error}"),
            ));
            None
        }
    };

    // The desired include needs every part of the state, so one unreadable
    // store skips the comparison instead of inventing a diff (its own finding
    // is already reported above).
    let desired = match (&sites, &guest, &board) {
        (Some(sites), Some(guest), Some(board)) => {
            let exposed: Vec<_> = sites
                .iter()
                .filter(|site| site.public_subdomain.is_some())
                .map(|site| (site.clone(), site_kind(site)))
                .collect();
            Some(clawde_katban::caddy::render_config(
                &exposed,
                guest.as_ref().map(|(host, port)| (host.as_str(), *port)),
                board.as_ref().map(|(host, port)| (host.as_str(), *port)),
            ))
        }
        _ => None,
    };
    if let Some(desired) = &desired {
        findings.push(check_managed_include(actual.as_deref(), desired));
    }

    // Whether caddy ever loads the include at all.
    match std::fs::read_to_string(caddy_dir.join("Caddyfile")) {
        Ok(caddyfile) => findings.extend(check_imports(&caddyfile, &managed_path)),
        Err(_) => findings.push(Finding::new(
            Level::Warn,
            "caddyfile",
            format!(
                "no Caddyfile at {} — the managed include is never loaded",
                caddy_dir.join("Caddyfile").display()
            ),
        )),
    }

    // Every exposed route's backend: a live site's proxy port, a locked site's
    // folder, then the two role services.
    if let Some(sites) = &sites {
        for site in sites.iter().filter(|site| site.public_subdomain.is_some()) {
            findings.push(match site_kind(site) {
                SiteKind::Live { port } => {
                    check_site(&site.name, SiteTarget::Proxy { port }, port_listening(port))
                }
                SiteKind::Static { root } => check_site(
                    &site.name,
                    SiteTarget::Static { root: &root },
                    root.is_dir(),
                ),
            });
        }
    }
    if let Some(Some((_, port))) = guest.as_ref() {
        findings.push(check_listener("cat chat", *port, port_listening(*port)));
    }
    if let Some(Some((_, port))) = board.as_ref() {
        findings.push(check_listener("admin board", *port, port_listening(*port)));
    }

    // Only expect units for roles that are actually configured: a missing
    // `katban-board.service` is normal when no board is exposed, and a missing
    // `catchat.service` is normal when Cat Chat is not. A role whose store is
    // unreadable is unknown, so it is left alone.
    let mut unit_names = vec!["katban-reload"];
    if guest.as_ref().is_some_and(Option::is_some) {
        unit_names.push("catchat");
    }
    if board.as_ref().is_some_and(Option::is_some) {
        unit_names.push("katban-board");
    }
    for name in unit_names {
        let unit = units_dir.join(format!("{name}.service"));
        let contents = std::fs::read_to_string(&unit).ok();
        findings.extend(check_service_unit(name, contents.as_deref()));
    }

    // The installed unit and its caddy block are rendered at different times
    // and installed by hand, so they can disagree about the port — a 502 that
    // nothing else reports. `unit_port_flag` returning `None` means the unit
    // takes the server's own default.
    for (role, unit, proxied, default_port) in [
        (
            "catchat.service",
            "catchat",
            guest
                .as_ref()
                .and_then(|guest| guest.as_ref())
                .map(|(_, port)| *port),
            DEFAULT_GUEST_PORT,
        ),
        (
            "katban-board.service",
            "katban-board",
            board
                .as_ref()
                .and_then(|board| board.as_ref())
                .map(|(_, port)| *port),
            DEFAULT_BOARD_PORT,
        ),
    ] {
        let Some(proxied) = proxied else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(units_dir.join(format!("{unit}.service"))) else {
            continue;
        };
        findings.push(check_unit_port(
            role,
            unit_port_flag(&text).unwrap_or(default_port),
            proxied,
        ));
    }

    // The reload watcher is checked even with nothing exposed: a stale
    // `PathChanged=` is what makes the next `expose` silently do nothing, and a
    // watcher that reloads without validating applies whatever is on disk.
    let reloader = std::fs::read_to_string(units_dir.join("katban-reload.path")).ok();
    findings.extend(check_reloader_path_unit(reloader.as_deref(), &managed_path));
    let reloader_service = std::fs::read_to_string(units_dir.join("katban-reload.service")).ok();
    findings.extend(check_reloader_service_unit(
        reloader_service.as_deref(),
        caddy_dir,
    ));

    // And whether caddy is up at all, when the host has a caddy unit to ask.
    if let Some((load_state, active_state)) = caddy {
        findings.extend(check_caddy_service(&load_state, &active_state));
    }

    // A directory with neither a Caddyfile nor the managed include is not a
    // caddy dir, and `--fix` must not leave a stray file in it.
    let can_repair = caddy_dir.join("Caddyfile").is_file() || managed_path.is_file();

    Diagnosis {
        managed_path,
        desired,
        actual,
        can_repair,
        findings,
    }
}

/// The Cat Chat block `(host, port)` of a loaded store, when Cat Chat has been
/// exposed.
fn guest_block_from(store: &clawde_katban::catchat::links::GuestStore) -> Option<(String, u16)> {
    let host = store.public_subdomain.clone()?;
    let port = store
        .guest_port
        .unwrap_or(clawde_katban::catchat::server::DEFAULT_GUEST_PORT);
    Some((host, port))
}

/// Is something accepting connections on loopback at `port`?
fn port_listening(port: u16) -> bool {
    use std::net::{SocketAddr, TcpStream};
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(250)).is_ok()
}

const LINK_USAGE: &str = r#"Usage: clawde catchat links <command>

  create <NAME> [--expires-in DAYS|never] [--max-concurrent N]
                                             Create a guest link (prints the URL + one-time password)
  list                                       List guest links
  show <ID>                                  Show one link (URL, expiry, devices)
  revoke <ID>                                Revoke a link (kicks its devices)
  password <ID> [--set "YOUR PASSWORD"]      Rotate the password (generated, or your own
                                             choice — the chosen one is not printed)
"#;

const GUEST_USAGE: &str = r#"Usage: clawde catchat <command> [OPTIONS]
Public-facing Cat Chat sandbox commands.

  serve [--port N] [--host IP] [--allow-non-loopback]
        [--search-endpoint URL] [--commands SPEC] [--models LIST]
                                              Run the Cat Chat sandbox server
                                              (guest web search; defaults to a
                                              local SearXNG on 127.0.0.1:8080,
                                              overridable via the flag or the
                                              CLAWDE_CATCHAT_SEARCH_ENDPOINT env var;
                                              --commands picks the guest slash
                                              vocabulary and --models the models
                                              a guest may pick)
  expose [--subdomain HOST] [--port N] [--dry-run] [--caddy-dir DIR]
         [--duckdns-token TOKEN]              Put the guest chat behind caddy
                                              (writes the managed katban.conf)
  unblock <IP|loopback>                       Clear a lockout / 24h block (the
                                              bucket `status` names)

Runs the Cat Chat sandbox server: friends open the URL, type the shared password,
and chat with Clawde (chat + web search only — no files, no shell, nothing
else). Cat Chat rides the host's free/limited providers; if none are
configured the chat explains that politely.
"#;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn run_link(args: &[String]) -> anyhow::Result<()> {
    use clawde_katban::catchat::links::{generate_password, load, save};

    let Some(subcommand) = args.first().map(|s| s.as_str()) else {
        print!("{LINK_USAGE}");
        return Ok(());
    };
    match subcommand {
        "create" => {
            // The name is everything before the first flag, so multi-word
            // names work without shell quoting (mirrors `/chat create`).
            let mut name_parts = Vec::new();
            let mut index = 1;
            while index < args.len() && !args[index].starts_with("--") {
                name_parts.push(args[index].as_str());
                index += 1;
            }
            let name = name_parts.join(" ");
            if name.trim().is_empty() {
                anyhow::bail!("link create needs a name: clawde catchat links create <NAME>");
            }
            let mut expires_at = None;
            let mut max_concurrent = clawde_katban::catchat::links::DEFAULT_MAX_CONCURRENT;
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
                clawde_katban::catchat::server::DEFAULT_GUEST_PORT
            );
            println!("password: {password}");
            println!("expires:  {expiry_text}");
            println!("max chat: {max_concurrent} at once");
            println!();
            println!("share the URL + password with friends. The password is shown once — ");
            println!("keep it safe. Serve the link with: clawde catchat serve");
            Ok(())
        }
        "list" => {
            let store = load()?;
            if store.links.is_empty() {
                println!("no guest links — create one with: clawde catchat links create <NAME>");
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
                .context("link show needs an id: clawde catchat links show <ID>")?;
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
                clawde_katban::catchat::server::DEFAULT_GUEST_PORT
            );
            Ok(())
        }
        "revoke" => {
            let id = args
                .get(1)
                .context("link revoke needs an id: clawde catchat links revoke <ID>")?;
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
                .context("link password needs an id: clawde catchat links password <ID>")?;
            // `--set "..."` (or `--set=...`) chooses your own password;
            // otherwise a random one is generated.
            let mut chosen: Option<String> = None;
            let mut index = 2;
            while index < args.len() {
                let flag = &args[index];
                if let Some(value) = flag.strip_prefix("--set=") {
                    chosen = Some(value.to_string());
                    index += 1;
                } else if flag == "--set" {
                    chosen = Some(
                        args.get(index + 1)
                            .context("--set needs a value: --set \"YOUR PASSWORD\"")?
                            .clone(),
                    );
                    index += 2;
                } else {
                    anyhow::bail!("unknown option: {flag}\n\n{LINK_USAGE}");
                }
            }
            let password = match chosen {
                Some(ref pw) => {
                    if let Err(message) = clawde_katban::catchat::links::validate_set_password(pw) {
                        anyhow::bail!("{message}");
                    }
                    pw.trim().to_string()
                }
                None => generate_password(),
            };
            let mut store = load()?;
            if !store.set_password(id, &password) {
                anyhow::bail!("no guest link '{id}'");
            }
            save(&store)?;
            println!("rotated password for guest link '{id}'");
            if chosen.is_some() {
                println!("password: (your chosen password — not printed)");
            } else {
                println!("password: {password}");
                println!();
                println!(
                    "The old password no longer works. The new one is shown once — keep it safe."
                );
            }
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print!("{LINK_USAGE}");
            Ok(())
        }
        other => anyhow::bail!("unknown link command: {other}\n\n{LINK_USAGE}"),
    }
}

async fn catchat_serve(args: &[String]) -> anyhow::Result<()> {
    use clawde_katban::catchat::server::{GuestServer, DEFAULT_GUEST_PORT};
    use std::sync::Arc;

    let mut port = DEFAULT_GUEST_PORT;
    let mut host = "127.0.0.1".to_string();
    let mut allow_non_loopback = false;
    let mut search_endpoint: Option<String> = None;
    let mut commands: Option<String> = None;
    let mut models: Option<String> = None;
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
            "--search-endpoint" => {
                search_endpoint = Some(
                    args.get(index + 1)
                        .context("--search-endpoint needs a URL")?
                        .to_string(),
                );
                index += 1;
            }
            "--commands" => {
                commands = Some(
                    args.get(index + 1)
                        .context("--commands needs a spec (all, none, or a comma-separated list)")?
                        .to_string(),
                );
                index += 1;
            }
            "--models" => {
                models = Some(
                    args.get(index + 1)
                        .context("--models needs a comma-separated model list")?
                        .to_string(),
                );
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
            flag if flag.starts_with("--search-endpoint=") => {
                search_endpoint = Some(flag["--search-endpoint=".len()..].to_string());
            }
            flag if flag.starts_with("--commands=") => {
                commands = Some(flag["--commands=".len()..].to_string());
            }
            flag if flag.starts_with("--models=") => {
                models = Some(flag["--models=".len()..].to_string());
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

    let mut store = clawde_katban::catchat::links::load()?;
    if store.links.is_empty() {
        println!(
            "WARNING: no guest links yet — create one with: clawde catchat links create <NAME>"
        );
    }
    let public_url = store.public_subdomain.clone();
    store.prune(now_secs());
    let store = Arc::new(std::sync::Mutex::new(store));
    let search_endpoint =
        clawde_katban::catchat::search::resolve_endpoint(search_endpoint.as_deref());
    let search: Arc<dyn clawde_katban::catchat::search::GuestSearch> = Arc::new(
        clawde_katban::catchat::search::SearxClient::new(search_endpoint.clone()),
    );
    let backend: Arc<dyn clawde_katban::catchat::engine::GuestBackend> =
        Arc::new(clawde_katban::catchat::engine::FreeBackend::new());
    let engine = Arc::new(clawde_katban::catchat::engine::ChatEngine::new(
        backend, search,
    ));
    // Guest policy: which commands and models a friend may use. A bad spec is
    // fatal — an always-on unit with a typo must not start with a surface the
    // host did not ask for.
    let options = clawde_katban::catchat::commands::GuestOptions::resolve(
        commands.as_deref(),
        models.as_deref(),
    )
    .map_err(anyhow::Error::msg)?;
    let commands_list = options.commands.names().join(", ");
    let server = GuestServer::new(engine, store, options);
    println!("Cat Chat serving at http://{addr}/  (Ctrl-C to stop)");
    if !clawde_katban::host::is_loopback(addr) {
        println!("WARNING: bound to {addr} — reachable by other machines; guests can chat but have no access to your system.");
    }
    if let Some(subdomain) = &public_url {
        println!("public:      https://{subdomain} (via caddy — see: clawde catchat expose)");
    }
    // Ask the endpoint what it is instead of asserting it is a SearXNG: a
    // host whose port 8080 belongs to something else (or whose SearXNG has the
    // JSON API off) otherwise advertises a guest feature that can only fail.
    match clawde_katban::catchat::search::probe_endpoint(&search_endpoint).await {
        Ok(()) => println!("guest search: {search_endpoint} (SearXNG JSON API answered)"),
        Err(reason) => {
            println!("guest search: {search_endpoint}");
            println!("              WARNING: unavailable — {reason}");
            println!(
                "              guests will be told search is unavailable. Point it somewhere else with:\n                clawde catchat serve --search-endpoint http://127.0.0.1:<port>   (or set {})",
                clawde_katban::catchat::search::ENDPOINT_ENV
            );
        }
    }
    if commands_list.is_empty() {
        println!("guest commands: none (guests just chat)");
    } else {
        println!("guest commands: {commands_list}   (a guest types /help)");
    }
    println!(
        "guests ride free/limited providers; chat degrades gracefully if none are configured."
    );
    server.run(addr).await?;
    Ok(())
}

/// Put the guest chat behind caddy: writes the managed `katban.conf` (all
/// exposed sites + the guest block), emits the reloader units, prints the
/// one-time bootstrap, and best-effort updates the DuckDNS subdomain.
async fn run_catchat_expose(args: &[String]) -> anyhow::Result<()> {
    use clawde_katban::caddy::{bootstrap_instructions, render_config, DEFAULT_INCLUDE_NAME};
    use clawde_katban::catchat::links::{load, save};
    use clawde_katban::config::load as load_sites;

    let mut subdomain: Option<String> = None;
    let mut port = clawde_katban::catchat::server::DEFAULT_GUEST_PORT;
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
    // running `catchat expose` never drops previously exposed sites.
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

    println!("exposing Cat Chat at https://{subdomain}");
    println!(
        "Managed caddy block (written to {}):",
        managed_path.display()
    );
    println!("{text}");

    if dry_run {
        println!("[dry-run] no files were written.");
        return Ok(());
    }

    for line in apply_managed_include(&managed_path, &text)? {
        println!("{line}");
    }

    // Write Cat Chat's own systemd unit plus the shared Caddy reloader. The
    // service is only emitted from the Cat Chat expose path.
    let units_dir = write_systemd_units(&caddy_dir, Some(port), (&[], 0))?;
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

fn run_catchat_unblock(args: &[String]) -> anyhow::Result<()> {
    use clawde_katban::catchat::links::{load, save};

    let input = args
        .first()
        .context("unblock needs an IP: clawde catchat unblock <IP>")?;
    // The bucket a client is filed under is not always the address the
    // operator typed: a loopback client with no `X-Forwarded-For` is filed
    // under `loopback`. Resolving first is what makes `unblock 127.0.0.1`
    // actually clear that client instead of reporting success after removing
    // a key nothing was stored under.
    let bucket = clawde_katban::http_security::lockout_bucket(input);
    let mut store = load()?;
    if !store.reset_failed_attempts(&bucket) {
        let active = store.active_lockouts(now_secs());
        if active.is_empty() {
            anyhow::bail!("no lockout recorded for '{bucket}' — nothing to clear");
        }
        anyhow::bail!(
            "no lockout recorded for '{bucket}' — active bucket(s): {}. A local client (curl, or a proxy that sets no X-Forwarded-For) is filed under 'loopback'.",
            clawde_katban::http_security::describe_lockouts(&active, now_secs())
        );
    }
    save(&store)?;
    if bucket == input.trim() {
        println!("cleared lockouts and 24h blocks for '{bucket}'");
    } else {
        println!(
            "cleared lockouts and 24h blocks for '{input}' (bucket '{bucket}' — every local \
             client without X-Forwarded-For shares it)"
        );
    }
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
        let target = resolve_serve_target(&config, "demo", None, false, false).unwrap();
        assert_eq!(target.root, PathBuf::from("/srv/demo"));
        assert_eq!(target.port, 8799);
        assert!(target.live);
        assert_eq!(target.registered_port, Some(8799));

        // Locked site -> live reload off unless overridden.
        assert!(
            !resolve_serve_target(&config, "blog", None, false, false)
                .unwrap()
                .live
        );
        assert!(
            !resolve_serve_target(&config, "blog", None, false, true)
                .unwrap()
                .live
        );
    }

    /// A `--port` override moves where the site is served but not what
    /// `site expose` proxies; the target has to carry both numbers so the
    /// command can warn about the mismatch instead of serving a 502 silently.
    #[test]
    fn resolve_serve_target_reports_a_port_override_against_the_registered_port() {
        let config = config_with(vec![site("demo", false, None)]);
        let target = resolve_serve_target(&config, "demo", Some(9123), false, false).unwrap();
        assert_eq!(target.port, 9123, "the override is what gets served");
        assert_eq!(
            target.registered_port,
            Some(8799),
            "the registered port is what caddy proxies"
        );
        // Overriding with the registered port: the two agree, so there is
        // nothing to warn about.
        let target = resolve_serve_target(&config, "demo", Some(8799), false, false).unwrap();
        assert_eq!(target.port, 8799);
        assert_eq!(target.registered_port, Some(8799));
    }

    #[test]
    fn resolve_serve_target_falls_back_to_directory() {
        let config = config_with(vec![]);
        let tmp = tempfile::tempdir().unwrap();
        let target =
            resolve_serve_target(&config, tmp.path().to_str().unwrap(), None, false, false)
                .unwrap();
        assert!(target.root.is_dir());
        assert_eq!(target.port, clawde_katban::config::DEFAULT_SITE_PORT);
        assert!(target.live);
        // A directory is not a registered site, so no port is on record for an
        // override to disagree with.
        assert_eq!(target.registered_port, None);
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

    // systemd exposure is a Linux-only surface: the unit renderer checks the
    // invoking user's identity and the unit exec paths, none of which exist
    // on Windows runners.
    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_units_follow_caddy_dir_and_guest_port() {
        // Serialize CLAWDE_HOME mutation on the binary-wide lock (repo rule).
        let _guard = crate::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().unwrap();
        let previous = std::env::var("CLAWDE_HOME").ok();
        std::env::set_var("CLAWDE_HOME", tmp.path());
        let result = (|| -> anyhow::Result<()> {
            let caddy_dir = tmp.path().join("custom-caddy");
            let units = write_systemd_units(&caddy_dir, Some(9000), (&[], 0))?;
            let service = std::fs::read_to_string(units.join("catchat.service"))?;
            assert!(
                service.contains("catchat serve --port 9000"),
                "service unit must bind the rendered Cat Chat port"
            );
            let path_unit = std::fs::read_to_string(units.join("katban-reload.path"))?;
            assert!(
                path_unit.contains(&format!(
                    "PathChanged={}",
                    caddy_dir.join("katban.conf").display()
                )),
                "reloader must watch the managed file in --caddy-dir"
            );
            // The watcher runs the checked reload, not a bare service reload,
            // and validates the include dir it was rendered for.
            let reload_service = std::fs::read_to_string(units.join("katban-reload.service"))?;
            assert!(
                reload_service.contains("katban caddy reload --caddy-dir"),
                "the watcher must gate the reload: {reload_service}"
            );
            assert!(
                reload_service.contains(&caddy_dir.display().to_string()),
                "the watcher must validate the include in --caddy-dir: {reload_service}"
            );
            assert!(
                !reload_service.contains("ExecStart=systemctl reload caddy"),
                "the watcher must not apply an unvalidated config: {reload_service}"
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

    #[cfg(target_os = "linux")]
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
            let units = write_systemd_units(&caddy_dir, None, (&[String::from("demo")], 8790))?;
            let board = std::fs::read_to_string(units.join("katban-board.service"))?;
            assert!(
                board.contains("ExecStart=clawde katban board serve --port 8790 --run demo")
                    || board.contains("board serve --port 8790 --run demo"),
                "board unit must serve the board port and run the project: {board}"
            );
            assert!(board.contains("Description=Katban admin board + runner (demo)"));
            // Katban expose writes only Katban/reloader artifacts; Cat Chat's
            // service is emitted exclusively by `catchat expose`.
            assert!(!units.join("catchat.service").exists());
            assert!(units.join("katban-reload.path").exists());
            Ok(())
        })();
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
        result.unwrap();
    }

    #[cfg(target_os = "linux")]
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
                None,
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

    // Persists a systemd refresh sentinel via the expose path — same Linux
    // surface as the systemd unit tests.
    #[cfg(target_os = "linux")]
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
        let store = clawde_katban::admin_auth::load().unwrap();
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
            let store = clawde_katban::admin_auth::load()?;
            clawde_katban::admin_auth::save(&store)?;
            assert!(admin_board_block()?.is_none());
            // Expose with a custom port -> Some((subdomain, port)).
            let mut store = clawde_katban::admin_auth::load()?;
            store.public_subdomain = Some("board.example.com".to_string());
            store.board_port = Some(8891);
            clawde_katban::admin_auth::save(&store)?;
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

    #[test]
    fn board_card_set_done_discards_pinned_branch() {
        // `board card set <id> done` on a reviewed card is an explicit discard:
        // it must delete the pinned `katban/<id>` branch, not leave it to leak
        // forever in the repo (advance/set on the web routes the same way).
        let _guard = crate::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let previous = std::env::var("CLAWDE_HOME").ok();
        std::env::set_var("CLAWDE_HOME", tmp.path());
        let result = (|| -> anyhow::Result<()> {
            std::process::Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .current_dir(repo.path())
                .output()
                .unwrap();
            // Repo-local identity: CI runners have no global git user, so the
            // init commit below would fail and leave the fixture without HEAD
            // (create_worktree then panics on "invalid reference: HEAD").
            for (k, v) in [("user.email", "test@example.com"), ("user.name", "Test")] {
                let cfg = std::process::Command::new("git")
                    .args(["config", k, v])
                    .current_dir(repo.path())
                    .output()
                    .unwrap();
                assert!(cfg.status.success(), "git config {k} failed: {cfg:?}");
            }
            std::fs::write(repo.path().join("README.md"), "# demo\n").unwrap();
            let add = std::process::Command::new("git")
                .args(["add", "."])
                .current_dir(repo.path())
                .output()
                .unwrap();
            assert!(add.status.success(), "git add failed: {add:?}");
            let commit = std::process::Command::new("git")
                .args(["commit", "-q", "-m", "init"])
                .current_dir(repo.path())
                .output()
                .unwrap();
            assert!(commit.status.success(), "git commit failed: {commit:?}");

            clawde_katban::projects::set_repo_root("default", repo.path())?;
            let mut board = clawde_katban::board::Board::new();
            let id = board.add_card("wire the db");
            board.set_status(&id, clawde_katban::board::CardStatus::Review);
            board.cards.iter_mut().find(|c| c.id == id).unwrap().branch =
                Some(format!("katban/{id}"));
            clawde_katban::board::save_board(&board, "default")?;

            // Pin a commit on the card's branch like the runner's finalize, then
            // remove the worktree the way finalize always does.
            let wt = clawde_katban::git::card_worktree_dir("default", &id);
            std::fs::create_dir_all(&wt)?;
            clawde_katban::git::create_worktree(repo.path(), &wt, None)
                .map_err(|e| anyhow::anyhow!(e))?;
            std::fs::write(wt.join("feature.txt"), "x\n")?;
            let sha = clawde_katban::git::commit_card(
                repo.path(),
                &wt,
                &format!("katban/{id}"),
                "katban: wire",
            )
            .map_err(|e| anyhow::anyhow!(e))?;
            let mut board = clawde_katban::board::load_board("default")?.unwrap();
            board.cards.iter_mut().find(|c| c.id == id).unwrap().commit = Some(sha);
            clawde_katban::board::save_board(&board, "default")?;
            clawde_katban::git::remove_worktree(repo.path(), &wt);

            run_card("default", &["set".into(), id.clone(), "done".into()])?;
            let board = clawde_katban::board::load_board("default")?.unwrap();
            assert_eq!(
                board.card(&id).unwrap().status,
                clawde_katban::board::CardStatus::Done
            );
            assert!(
                clawde_katban::git::rev_parse(repo.path(), &format!("katban/{id}")).is_none(),
                "pinned branch leaked after `card set done`"
            );
            Ok(())
        })();
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
        result.unwrap();
    }

    #[test]
    fn board_card_comment_and_feedback_flow() {
        // Serialize CLAWDE_HOME mutation on the binary-wide lock (repo rule).
        let _guard = crate::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().unwrap();
        let previous = std::env::var("CLAWDE_HOME").ok();
        std::env::set_var("CLAWDE_HOME", tmp.path());
        let result = (|| -> anyhow::Result<()> {
            // Seed a board with one card in review (the runner marks success
            // review).
            let mut board = clawde_katban::board::Board::new();
            let id = board.add_card("wire up the db");
            board.set_status(&id, clawde_katban::board::CardStatus::Review);
            clawde_katban::board::save_board(&board, "default")?;

            // `board card comment <id> --line 12 <text>` appends a comment.
            run_card(
                "default",
                &[
                    "comment".into(),
                    id.clone(),
                    "--line".into(),
                    "12".into(),
                    "add an index".into(),
                ],
            )?;
            let loaded = clawde_katban::board::load_board("default")?.unwrap();
            let card = loaded.card(&id).unwrap();
            assert_eq!(card.reviews.len(), 1);
            assert_eq!(card.reviews[0].location.as_deref(), Some("12"));
            assert_eq!(card.reviews[0].text, "add an index");

            // `board card feedback <id>` requeues it with the composed feedback.
            run_card("default", &["feedback".into(), id.clone()])?;
            let loaded = clawde_katban::board::load_board("default")?.unwrap();
            let card = loaded.card(&id).unwrap();
            assert_eq!(card.status, clawde_katban::board::CardStatus::Queued);
            assert!(card
                .followup_feedback
                .as_deref()
                .unwrap()
                .contains("add an index"));
            Ok(())
        })();
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
        result.unwrap();
    }

    #[tokio::test]
    async fn board_verify_toggle_round_trips() {
        // `board verify on|off` is the per-board master switch for the
        // verification gate: it must round-trip through board.json, reject
        // anything but on/off, and default to on.
        let _guard = crate::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let previous = std::env::var("CLAWDE_HOME").ok();
        std::env::set_var("CLAWDE_HOME", tmp.path());
        let result = async {
            let board = clawde_katban::board::Board::new();
            assert!(board.verify, "gate must default to on");
            clawde_katban::board::save_board(&board, "default")?;

            run_board(&["verify".into(), "off".into()]).await?;
            assert!(
                !clawde_katban::board::load_board("default")?.unwrap().verify,
                "verify off must persist"
            );
            run_board(&["verify".into(), "on".into()]).await?;
            assert!(clawde_katban::board::load_board("default")?.unwrap().verify);
            assert!(
                run_board(&["verify".into(), "maybe".into()]).await.is_err(),
                "board verify rejects anything but on/off"
            );
            Ok::<(), anyhow::Error>(())
        }
        .await;
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
        result.unwrap();
    }

    #[tokio::test]
    async fn catchat_serve_flags_reach_the_serve_parser() {
        // Regression: `run_catchat_command` once handed `serve` to a second
        // dispatcher that stripped the subcommand token and then matched the
        // next argument (`--port`) as if it were a subcommand, so every
        // `clawde catchat serve --port N` died with "unknown guest command".
        // The flag must now reach `catchat_serve`'s option parser instead.
        let error = run_catchat_command(&["serve".to_string(), "--not-a-real-flag".to_string()])
            .await
            .expect_err("an unknown serve flag must be rejected");
        let message = error.to_string();
        assert!(
            message.contains("unknown option"),
            "serve flags must reach the serve parser, got: {message}"
        );
        assert!(
            !message.contains("unknown guest command"),
            "the removed double-dispatch must not come back: {message}"
        );
    }

    #[tokio::test]
    async fn catchat_command_dispatch_routes_each_verb() {
        // Each verb must reach its own parser and surface that parser's
        // message — never a neighbour's. One table row per verb, so adding a
        // verb without a routing case shows up in the diff. The regression
        // this guards is `serve` being handed to a second dispatcher
        // (`unknown guest command`), so every row names the expected owner.
        let _guard = crate::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let previous = std::env::var("CLAWDE_HOME").ok();
        std::env::set_var("CLAWDE_HOME", tmp.path());
        let result = async {
            // (arguments, fragment only that verb's own parser produces)
            let error_cases: &[(&[&str], &str)] = &[
                (&["frobnicate"], "unknown catchat command"),
                (&["serve", "--bogus"], "unknown option"),
                (&["expose", "--bogus"], "unknown option"),
                (&["unblock"], "unblock needs an IP"),
            ];
            for (args, expected) in error_cases {
                let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
                let message = run_catchat_command(&args)
                    .await
                    .expect_err("this verb must reject the arguments")
                    .to_string();
                assert!(
                    message.contains(expected),
                    "`{}` must be reported by its own parser, got: {message}",
                    args.join(" ")
                );
                assert!(
                    !message.contains("unknown guest command"),
                    "the removed double-dispatch came back: {message}"
                );
            }

            // Verbs that print their usage and succeed.
            let ok_cases: &[&[&str]] = &[&[], &["links"], &["help"], &["--help"], &["-h"]];
            for args in ok_cases {
                let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
                assert!(
                    run_catchat_command(&args).await.is_ok(),
                    "`{}` prints usage and succeeds",
                    args.join(" ")
                );
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
        result.unwrap();
    }

    /// The bucket fix, end to end through the CLI. A loopback guest is filed
    /// under `loopback`, so `unblock 127.0.0.1` must clear *that* bucket — and a
    /// clear that matches nothing must fail loudly, because the old behaviour
    /// (remove a key nothing was stored under, print success) is exactly how a
    /// locked-out guest stayed locked out while the operator was told it was
    /// handled.
    #[tokio::test]
    async fn catchat_unblock_resolves_a_loopback_spelling_and_reports_a_miss() {
        let _guard = crate::ENV_LOCK.lock().await;
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", home.path());
        use clawde_katban::catchat::links::{load, save, GuestStore};
        use clawde_katban::http_security::{lockout_bucket, LOOPBACK_BUCKET};

        let mut store = GuestStore::default();
        for _ in 0..5 {
            store.record_failed_attempt(LOOPBACK_BUCKET);
        }
        save(&store).unwrap();

        assert_eq!(lockout_bucket("127.0.0.1"), LOOPBACK_BUCKET);
        run_catchat_unblock(&["127.0.0.1".to_string()])
            .expect("127.0.0.1 must clear the bucket a loopback client is filed under");
        assert!(load().unwrap().failed_attempts.is_empty());

        // Nothing is locked out now, so a repeat has nothing to clear.
        let miss = run_catchat_unblock(&["127.0.0.1".to_string()])
            .expect_err("a clear that matched nothing must not report success");
        assert!(miss.to_string().contains("nothing to clear"), "{miss}");

        // With someone *else* locked out, the miss names the bucket to pass.
        // Four attempts is what arms Cat Chat's 3-minute lockout window, so the
        // store has to record that many before the bucket is "active".
        let mut store = GuestStore::default();
        for _ in 0..4 {
            store.record_failed_attempt("1.2.3.4");
        }
        save(&store).unwrap();
        let miss = run_catchat_unblock(&["10.9.9.9".to_string()])
            .expect_err("an address with no lockout must not report success");
        let message = miss.to_string();
        assert!(
            message.contains("1.2.3.4 (lockout"),
            "the miss must name the active bucket: {message}"
        );
    }

    /// A caddy dir and a systemd units dir for `doctor` to read, so its checks
    /// never touch the developer's host (the caller pins `CLAWDE_HOME`).
    fn doctor_scratch() -> (tempfile::TempDir, tempfile::TempDir) {
        let caddy = tempfile::tempdir().unwrap();
        let units = tempfile::tempdir().unwrap();
        (caddy, units)
    }

    fn write_sites(home: &Path, sites: serde_json::Value) {
        std::fs::create_dir_all(home.join("katban")).unwrap();
        let config = serde_json::json!({ "version": 1, "sites": sites });
        std::fs::write(home.join("katban/katban.json"), config.to_string()).unwrap();
    }

    /// Regression: `site expose` renders the managed include itself, and
    /// `apply_managed_include` replaces the whole file — so a block this path
    /// forgets is a block it deletes. It used to pass `None` for the Cat Chat
    /// guest block, taking `chat.example.com` offline (with a reload) on any
    /// host that also exposed Cat Chat. Every expose path must render the full
    /// set: sites + guest + board.
    #[test]
    fn site_expose_config_keeps_the_cat_chat_and_board_blocks() {
        let _guard = crate::ENV_LOCK.blocking_lock();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var("CLAWDE_HOME").ok();
        std::env::set_var("CLAWDE_HOME", home.path());

        write_sites(
            home.path(),
            serde_json::json!([
                { "name": "demo", "root": "/tmp", "port": 8788,
                  "publicSubdomain": "demo.example.com", "locked": false }
            ]),
        );
        let guests = clawde_katban::catchat::links::GuestStore {
            public_subdomain: Some("chat.example.com".to_string()),
            guest_port: Some(8789),
            ..Default::default()
        };
        clawde_katban::catchat::links::save(&guests).unwrap();
        let admin = clawde_katban::admin_auth::AdminStore {
            public_subdomain: Some("board.example.com".to_string()),
            board_port: Some(8790),
            ..Default::default()
        };
        clawde_katban::admin_auth::save(&admin).unwrap();

        let exposed: Vec<_> = clawde_katban::config::load()
            .unwrap()
            .sites
            .iter()
            .filter(|site| site.public_subdomain.is_some())
            .map(|site| (site.clone(), clawde_katban::caddy::site_kind(site)))
            .collect();
        let text = managed_config_text(&exposed).unwrap();

        assert!(text.contains("demo.example.com"), "site block: {text}");
        assert!(
            text.contains("chat.example.com"),
            "a site expose must not drop the Cat Chat block: {text}"
        );
        assert!(
            text.contains("reverse_proxy 127.0.0.1:8789"),
            "the guest block still proxies its registered port: {text}"
        );
        assert!(text.contains("board.example.com"), "board block: {text}");

        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
    }

    fn finding<'a>(findings: &'a [Finding], subject: &str) -> &'a Finding {
        findings
            .iter()
            .find(|f| f.subject == subject)
            .unwrap_or_else(|| panic!("no `{subject}` finding in {findings:#?}"))
    }

    #[tokio::test]
    async fn doctor_flags_dead_site_routes_and_a_missing_import() {
        let _guard = crate::ENV_LOCK.lock().await;
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", home.path());
        let (caddy, units) = doctor_scratch();
        use clawde_katban::doctor::Level;

        // A live site whose backend we own (so it is up) and a locked site
        // whose folder is gone.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live_port = listener.local_addr().unwrap().port();
        let missing_root = home.path().join("gone");
        write_sites(
            home.path(),
            serde_json::json!([
                { "name": "live", "root": "/tmp", "port": live_port,
                  "publicSubdomain": "live.example.com", "locked": false },
                { "name": "locked", "root": missing_root.display().to_string(), "port": 8788,
                  "publicSubdomain": "locked.example.com", "locked": true },
            ]),
        );
        // The one-time bootstrap line was never added.
        std::fs::write(caddy.path().join("Caddyfile"), "import sites-enabled/*\n").unwrap();

        let diagnosis = diagnose(caddy.path(), units.path(), None);
        assert!(
            diagnosis.desired.is_some(),
            "readable state must render a desired include"
        );
        assert_eq!(
            finding(&diagnosis.findings, "managed caddy include").level,
            Level::Warn
        );
        assert!(finding(&diagnosis.findings, "caddy import")
            .message
            .contains("never serves these routes"));
        assert_eq!(finding(&diagnosis.findings, "site live").level, Level::Ok);
        let locked = finding(&diagnosis.findings, "site locked");
        assert_eq!(locked.level, Level::Warn);
        assert!(locked.message.contains("404"));
        // Roles that are not configured get no unit expected of them.
        assert!(!diagnosis
            .findings
            .iter()
            .any(|f| f.subject == "catchat.service"));
        assert!(!diagnosis
            .findings
            .iter()
            .any(|f| f.subject == "katban-board.service"));
        assert_eq!(
            finding(&diagnosis.findings, "katban-reload.service").level,
            Level::Warn
        );
        assert_eq!(
            finding(&diagnosis.findings, "katban-reload.path").level,
            Level::Warn
        );
    }

    #[tokio::test]
    async fn doctor_flags_a_watcher_that_reloads_without_validating() {
        let _guard = crate::ENV_LOCK.lock().await;
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", home.path());
        let (caddy, units) = doctor_scratch();
        use clawde_katban::doctor::Level;

        write_sites(home.path(), serde_json::json!([]));
        std::fs::write(caddy.path().join("Caddyfile"), "import katban*.conf\n").unwrap();
        // A watcher installed before the gate existed: it applies whatever is
        // on disk, so a fragment that stopped adapting fails every later
        // reload with nothing naming the cause.
        std::fs::write(
            units.path().join("katban-reload.service"),
            "[Service]\nType=oneshot\nExecStart=systemctl reload caddy\n",
        )
        .unwrap();
        let stale = diagnose(caddy.path(), units.path(), None);
        let gate = finding(&stale.findings, "katban-reload gate");
        assert_eq!(gate.level, Level::Warn);
        assert!(gate.message.contains("without validating"), "{gate:?}");

        // A watcher rendered from the current code passes, for this dir.
        std::fs::write(
            units.path().join("katban-reload.service"),
            clawde_katban::caddy::render_reloader_service_unit(
                "/usr/local/bin/clawde",
                caddy.path(),
            ),
        )
        .unwrap();
        let fresh = diagnose(caddy.path(), units.path(), None);
        assert_eq!(
            finding(&fresh.findings, "katban-reload gate").level,
            Level::Ok
        );
    }

    #[tokio::test]
    async fn doctor_flags_a_reloader_watching_another_include() {
        let _guard = crate::ENV_LOCK.lock().await;
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", home.path());
        let (caddy, units) = doctor_scratch();
        use clawde_katban::doctor::Level;

        write_sites(home.path(), serde_json::json!([]));
        std::fs::write(caddy.path().join("Caddyfile"), "import katban*.conf\n").unwrap();
        // Cat Chat is exposed and its backend is up (a listener we own), but
        // the reload watcher points at the default caddy dir, not this one.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let guest_port = listener.local_addr().unwrap().port();
        std::fs::create_dir_all(home.path().join("catchat")).unwrap();
        std::fs::write(
            home.path().join("catchat/links.json"),
            serde_json::json!({
                "version": 1,
                "publicSubdomain": "chat.example.com",
                "guestPort": guest_port,
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            units.path().join("katban-reload.path"),
            "[Path]\nPathChanged=/etc/caddy/katban.conf\n",
        )
        .unwrap();

        let diagnosis = diagnose(caddy.path(), units.path(), None);
        let reloader = finding(&diagnosis.findings, "katban-reload.path");
        assert_eq!(reloader.level, Level::Error);
        assert!(reloader.message.contains("/etc/caddy/katban.conf"));
        assert!(reloader
            .message
            .contains(&caddy.path().join("katban.conf").display().to_string()));
        // Cat Chat is exposed, so its unit is expected and its port is checked.
        assert!(diagnosis
            .findings
            .iter()
            .any(|f| f.subject == "catchat.service"));
        assert_eq!(
            finding(&diagnosis.findings, "cat chat listener").level,
            Level::Ok
        );
        // No board is exposed, so no board unit is expected of the host.
        assert!(!diagnosis
            .findings
            .iter()
            .any(|f| f.subject == "katban-board.service"));
    }

    #[tokio::test]
    async fn doctor_reports_unreadable_state_instead_of_aborting() {
        let _guard = crate::ENV_LOCK.lock().await;
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", home.path());
        let (caddy, units) = doctor_scratch();
        use clawde_katban::doctor::Level;

        std::fs::create_dir_all(home.path().join("katban")).unwrap();
        std::fs::write(home.path().join("katban/katban.json"), "{ nope").unwrap();
        std::fs::write(caddy.path().join("Caddyfile"), "import katban*.conf\n").unwrap();

        let diagnosis = diagnose(caddy.path(), units.path(), None);
        let state = finding(&diagnosis.findings, "katban.json");
        assert_eq!(state.level, Level::Error);
        assert!(state.message.contains("unreadable"));
        assert!(
            diagnosis.desired.is_none(),
            "an unreadable store must not invent a desired include"
        );
        assert!(!diagnosis
            .findings
            .iter()
            .any(|f| f.subject == "managed caddy include"));
        // The rest of the host is still reported.
        assert!(diagnosis
            .findings
            .iter()
            .any(|f| f.subject == "katban-reload.path"));
    }

    #[tokio::test]
    async fn doctor_repairs_the_managed_include_then_reports_it_clean() {
        let _guard = crate::ENV_LOCK.lock().await;
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", home.path());
        let (caddy, units) = doctor_scratch();
        use clawde_katban::doctor::Level;

        write_sites(home.path(), serde_json::json!([]));
        std::fs::write(caddy.path().join("Caddyfile"), "import katban*.conf\n").unwrap();

        let before = diagnose(caddy.path(), units.path(), None);
        assert_eq!(
            finding(&before.findings, "managed caddy include").level,
            Level::Warn
        );
        assert!(!before.include_matches());
        assert!(
            before.can_repair,
            "this dir has a Caddyfile to write beside"
        );

        // What `--fix` writes, minus the systemctl hop (which needs root).
        clawde_katban::caddy::write_atomic(
            &before.managed_path,
            before.desired.as_deref().unwrap(),
        )
        .unwrap();

        // A fresh diagnosis — what the report and `--strict` now see — must be
        // clean instead of repeating the repaired drift.
        let after = diagnose(caddy.path(), units.path(), None);
        assert!(after.include_matches());
        assert_eq!(
            finding(&after.findings, "managed caddy include").level,
            Level::Ok
        );
        assert_eq!(
            after
                .findings
                .iter()
                .filter(|f| f.level == Level::Error)
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn doctor_flags_a_role_unit_serving_another_port_than_caddy_proxies() {
        let _guard = crate::ENV_LOCK.lock().await;
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", home.path());
        let (caddy, units) = doctor_scratch();
        use clawde_katban::doctor::Level;

        write_sites(home.path(), serde_json::json!([]));
        std::fs::write(caddy.path().join("Caddyfile"), "import katban*.conf\n").unwrap();
        // Cat Chat's caddy block proxies to 8790, its unit serves 9999: the
        // unit and the block were rendered at different times.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let guest_port = listener.local_addr().unwrap().port();
        std::fs::create_dir_all(home.path().join("catchat")).unwrap();
        std::fs::write(
            home.path().join("catchat/links.json"),
            serde_json::json!({
                "version": 1,
                "publicSubdomain": "chat.example.com",
                "guestPort": guest_port,
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            units.path().join("catchat.service"),
            "[Service]\nExecStart=clawde catchat serve --port 9999\n",
        )
        .unwrap();

        let diagnosis = diagnose(caddy.path(), units.path(), None);
        let drift = finding(&diagnosis.findings, "catchat.service port");
        assert_eq!(drift.level, Level::Error);
        assert!(drift.message.contains("9999"));
        assert!(drift.message.contains(&guest_port.to_string()));
        // The unit itself is fine, so only the port disagrees.
        assert_eq!(
            finding(&diagnosis.findings, "catchat.service").level,
            Level::Ok
        );
    }

    #[tokio::test]
    async fn doctor_refuses_to_repair_a_directory_that_is_not_caddy() {
        let _guard = crate::ENV_LOCK.lock().await;
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", home.path());
        let (caddy, units) = doctor_scratch();
        use clawde_katban::doctor::Level;

        write_sites(home.path(), serde_json::json!([]));
        // A typo'd --caddy-dir: neither a Caddyfile nor a managed include.
        let diagnosis = diagnose(caddy.path(), units.path(), None);
        assert!(!diagnosis.can_repair, "--fix must not write here");
        assert!(diagnosis.desired.is_some(), "state itself is readable");
        assert_eq!(finding(&diagnosis.findings, "caddyfile").level, Level::Warn);
        // A directory that has the include but no Caddyfile is still ours.
        std::fs::write(
            diagnosis.managed_path.clone(),
            diagnosis.desired.as_deref().unwrap(),
        )
        .unwrap();
        assert!(diagnose(caddy.path(), units.path(), None).can_repair);
    }

    #[tokio::test]
    async fn doctor_reports_the_caddy_unit_it_can_see() {
        let _guard = crate::ENV_LOCK.lock().await;
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", home.path());
        let (caddy, units) = doctor_scratch();
        use clawde_katban::doctor::Level;

        write_sites(home.path(), serde_json::json!([]));
        std::fs::write(caddy.path().join("Caddyfile"), "import katban*.conf\n").unwrap();

        // A failed caddy unit is the worst case: every route is down.
        let failed = diagnose(
            caddy.path(),
            units.path(),
            Some(("loaded".into(), "failed".into())),
        );
        assert_eq!(
            finding(&failed.findings, "caddy.service").level,
            Level::Error
        );

        // No caddy unit to ask about (another proxy) -> no finding at all.
        let unknown = diagnose(caddy.path(), units.path(), None);
        assert!(!unknown
            .findings
            .iter()
            .any(|f| f.subject == "caddy.service"));
    }

    /// Fake `caddy` and `systemctl` on PATH so the apply/reload path is
    /// exercised without touching the host: each shim appends its argv to
    /// `$SHIM_LOG` and exits with `$SHIM_ADAPT_EXIT` (for `adapt`) or
    /// `$SHIM_RUN_EXIT` (anything else).
    fn install_shims(dir: &Path) {
        for program in ["caddy", "systemctl"] {
            let shim = dir.join(program);
            std::fs::write(
                &shim,
                "#!/bin/sh\n\
                 printf '%s %s\\n' \"$0\" \"$*\" >> \"$SHIM_LOG\"\n\
                 if [ \"$1\" = adapt ]; then\n\
                   if [ \"${SHIM_ADAPT_EXIT:-0}\" != 0 ]; then\n\
                     echo 'shim: unexpected token, expecting }' >&2\n\
                   fi\n\
                   exit \"${SHIM_ADAPT_EXIT:-0}\"\n\
                 fi\n\
                 exit \"${SHIM_RUN_EXIT:-0}\"\n",
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
    }

    /// The `PATH` and `SHIM_LOG` values that put the shims in front of the real
    /// binaries. The caller sets them: the env audit requires a test's
    /// mutations to be visible in the `ENV_LOCK`-holding test fn, not hidden in
    /// a helper. The previous `PATH` stays after the shim dir, so commands the
    /// shims do not stand in for (`id`, `git`, ...) still resolve.
    fn shim_env(shim_dir: &Path, log: &Path) -> (String, String) {
        let path = std::env::var("PATH").unwrap_or_default();
        (
            format!("{}:{path}", shim_dir.display()),
            log.display().to_string(),
        )
    }

    fn shim_log(log: &Path) -> String {
        std::fs::read_to_string(log).unwrap_or_default()
    }

    /// What the installed watcher runs: an unadaptable Caddyfile is refused and
    /// nothing reaches caddy at all.
    #[tokio::test]
    async fn caddy_reload_refuses_a_caddyfile_that_stops_adapting() {
        let _guard = crate::ENV_LOCK.lock().await;
        let (caddy, shims) = doctor_scratch();
        let previous_path = std::env::var("PATH").ok();
        let previous_log = std::env::var("SHIM_LOG").ok();
        install_shims(shims.path());
        let (path, log_path) = shim_env(shims.path(), &shims.path().join("log"));
        std::env::set_var("PATH", path);
        std::env::set_var("SHIM_LOG", log_path);
        std::env::set_var("SHIM_ADAPT_EXIT", "1");
        std::env::set_var("SHIM_RUN_EXIT", "0");
        std::fs::write(caddy.path().join("Caddyfile"), "import katban*.conf\n").unwrap();

        let error = run_caddy_reload(&[
            "--caddy-dir".to_string(),
            caddy.path().display().to_string(),
        ])
        .expect_err("an unadaptable Caddyfile must not be applied");
        let message = error.to_string();
        assert!(message.contains("refused to reload"), "{message}");
        assert!(message.contains("no longer adapts"), "{message}");
        // caddy's own diagnostic reaches the journal, not just a status code.
        assert!(message.contains("unexpected token"), "{message}");

        // The gate is the point: validation ran, the reload never did.
        let log = shim_log(&shims.path().join("log"));
        assert!(log.contains("adapt"), "validation must run: {log}");
        assert!(!log.contains("reload"), "nothing may reach caddy: {log}");

        std::env::remove_var("SHIM_ADAPT_EXIT");
        std::env::remove_var("SHIM_RUN_EXIT");
        match previous_log {
            Some(value) => std::env::set_var("SHIM_LOG", value),
            None => std::env::remove_var("SHIM_LOG"),
        }
        match previous_path {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
    }

    /// The healthy path: validate first, then reload — in that order.
    #[tokio::test]
    async fn caddy_reload_validates_before_it_reloads() {
        let _guard = crate::ENV_LOCK.lock().await;
        let (caddy, shims) = doctor_scratch();
        let previous_path = std::env::var("PATH").ok();
        let previous_log = std::env::var("SHIM_LOG").ok();
        install_shims(shims.path());
        let (path, log_path) = shim_env(shims.path(), &shims.path().join("log"));
        std::env::set_var("PATH", path);
        std::env::set_var("SHIM_LOG", log_path);
        std::env::set_var("SHIM_ADAPT_EXIT", "0");
        std::env::set_var("SHIM_RUN_EXIT", "0");
        std::fs::write(caddy.path().join("Caddyfile"), "import katban*.conf\n").unwrap();

        run_caddy_reload(&[
            "--caddy-dir".to_string(),
            caddy.path().display().to_string(),
        ])
        .expect("an adapting Caddyfile is applied");

        let log = shim_log(&shims.path().join("log"));
        let validated = log.find("adapt").expect("validation must run");
        let reloaded = log.find("reload").expect("the reload must follow: {log}");
        assert!(validated < reloaded, "validate before reload: {log}");

        std::env::remove_var("SHIM_ADAPT_EXIT");
        std::env::remove_var("SHIM_RUN_EXIT");
        match previous_log {
            Some(value) => std::env::set_var("SHIM_LOG", value),
            None => std::env::remove_var("SHIM_LOG"),
        }
        match previous_path {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
    }

    #[test]
    fn caddy_reload_errors_when_there_is_no_caddyfile() {
        let (caddy, _units) = doctor_scratch();
        let error = run_caddy_reload(&[
            "--caddy-dir".to_string(),
            caddy.path().display().to_string(),
        ])
        .expect_err("a dir with no Caddyfile has nothing to reload");
        assert!(error.to_string().contains("no Caddyfile"), "{error}");
    }

    #[test]
    fn caddy_subcommands_reject_unknown_input() {
        let unknown = run_caddy_reload(&["--bogus".to_string()]).unwrap_err();
        assert!(unknown.to_string().contains("unknown option"));
        let incomplete = run_caddy_reload(&["--caddy-dir".to_string()]).unwrap_err();
        assert!(incomplete.to_string().contains("--caddy-dir needs a value"));
        assert!(run_caddy_reload(&["--help".to_string()]).is_ok());

        assert!(run_caddy(&[]).is_ok());
        assert!(run_caddy(&["help".to_string()]).is_ok());
        let bad = run_caddy(&["rewind".to_string()]).unwrap_err();
        assert!(bad.to_string().contains("unknown katban caddy command"));
    }

    #[tokio::test]
    async fn apply_managed_include_rolls_back_a_config_that_stops_adapting() {
        let _guard = crate::ENV_LOCK.lock().await;
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", home.path());
        let (caddy, shims) = doctor_scratch();
        let (path, log_path) = shim_env(shims.path(), &shims.path().join("log"));
        install_shims(shims.path());
        std::env::set_var("PATH", path);
        std::env::set_var("SHIM_LOG", log_path);
        std::env::set_var("SHIM_ADAPT_EXIT", "1");
        std::env::set_var("SHIM_RUN_EXIT", "0");

        // The include as it is now; the fragment below would be the change.
        std::fs::write(caddy.path().join("Caddyfile"), "import katban*.conf\n").unwrap();
        let managed = caddy.path().join("katban.conf");
        std::fs::write(&managed, "previous-good\n").unwrap();

        let error = apply_managed_include(&managed, "demo.example.com {\n\trespond \"x\"\n")
            .expect_err("a config that no longer adapts must not be kept");
        let message = error.to_string();
        assert!(message.contains("no longer adapts"), "{message}");
        // caddy's own diagnostic is surfaced, not swallowed.
        assert!(message.contains("unexpected token"), "{message}");
        // The previous bytes are back on disk...
        assert_eq!(
            std::fs::read_to_string(&managed).unwrap(),
            "previous-good\n"
        );
        // ...and nothing ever tried to reload the proxy.
        let log = shim_log(&shims.path().join("log"));
        assert!(log.contains("adapt"), "validation must run: {log}");
        assert!(
            !log.contains("reload"),
            "no reload after a failed adapt: {log}"
        );

        std::env::remove_var("SHIM_ADAPT_EXIT");
        std::env::remove_var("SHIM_RUN_EXIT");
    }

    #[tokio::test]
    async fn apply_managed_include_reloads_rootlessly_for_a_unix_socket_admin() {
        let _guard = crate::ENV_LOCK.lock().await;
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", home.path());
        let (caddy, shims) = doctor_scratch();
        let log = shims.path().join("log");
        let (path, log_path) = shim_env(shims.path(), &log);
        install_shims(shims.path());
        std::env::set_var("PATH", path);
        std::env::set_var("SHIM_LOG", log_path);
        std::env::remove_var("CLAWDE_CADDY_RELOAD");

        let socket = "unix//run/caddy/admin.sock";
        std::fs::write(
            caddy.path().join("Caddyfile"),
            format!("{{\n\tadmin {socket}\n}}\n\nimport katban*.conf\n"),
        )
        .unwrap();
        let managed = caddy.path().join("katban.conf");

        let report = apply_managed_include(&managed, "demo.example.com {\n}\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&managed).unwrap(),
            "demo.example.com {\n}\n"
        );
        assert!(
            report.iter().any(|line| line.contains("validated")),
            "{report:?}"
        );
        let log = shim_log(&log);
        assert!(
            log.contains("caddy reload --config") && log.contains(socket),
            "a socket-scoped admin endpoint reloads root-free through it: {log}"
        );
        assert!(!log.contains("systemctl"), "no service reload here: {log}");
    }

    #[tokio::test]
    async fn apply_managed_include_keeps_the_trusted_service_reload_by_default() {
        let _guard = crate::ENV_LOCK.lock().await;
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", home.path());
        let (caddy, shims) = doctor_scratch();
        let log = shims.path().join("log");
        let (path, log_path) = shim_env(shims.path(), &log);
        install_shims(shims.path());
        std::env::set_var("PATH", path);
        std::env::set_var("SHIM_LOG", log_path);
        std::env::remove_var("CLAWDE_CADDY_RELOAD");

        // A plain host: caddy's default TCP admin API is reachable without
        // root, but the trusted default must stay the service reload.
        std::fs::write(caddy.path().join("Caddyfile"), "import katban*.conf\n").unwrap();
        let managed = caddy.path().join("katban.conf");
        apply_managed_include(&managed, "demo.example.com {\n}\n").unwrap();
        let default_path = shim_log(&log);
        assert!(
            default_path.contains("systemctl reload caddy"),
            "{default_path}"
        );
        assert!(!default_path.contains("caddy reload"), "{default_path}");

        // Asking for the admin API without a socket endpoint is refused, not
        // obeyed: the TCP admin API would have loaded *this* directory's
        // Caddyfile into the running proxy.
        std::fs::write(&log, "").unwrap();
        std::env::set_var("CLAWDE_CADDY_RELOAD", "caddy");
        let report = apply_managed_include(&managed, "demo.example.com {\n  # v2\n}\n").unwrap();
        let forced = shim_log(&log);
        assert!(forced.contains("systemctl reload caddy"), "{forced}");
        assert!(!forced.contains("caddy reload"), "{forced}");
        assert!(
            report
                .iter()
                .any(|line| line.contains("needs a socket-scoped admin endpoint")),
            "the refusal is explained: {report:?}"
        );
        std::env::remove_var("CLAWDE_CADDY_RELOAD");
    }

    #[test]
    fn admin_endpoint_reads_the_global_directive() {
        assert_eq!(
            admin_endpoint("import katban*.conf\n"),
            AdminEndpoint::Unset
        );
        assert_eq!(admin_endpoint("admin off\n"), AdminEndpoint::Off);
        assert_eq!(
            admin_endpoint("{\n\tadmin unix//run/caddy/admin.sock\n}\n"),
            AdminEndpoint::Unix("unix//run/caddy/admin.sock".into())
        );
        assert_eq!(
            admin_endpoint("admin localhost:2019\n"),
            AdminEndpoint::Tcp("localhost:2019".into())
        );
        // Not the directive, and not uncommented.
        assert_eq!(admin_endpoint("administration off\n"), AdminEndpoint::Unset);
        assert_eq!(admin_endpoint("# admin off\n"), AdminEndpoint::Unset);
    }

    #[test]
    fn reload_error_line_prefers_the_error_over_log_noise() {
        // caddy's stderr is JSON logs; only the error line is useful.
        let caddy = concat!(
            "{\"level\":\"info\",\"msg\":\"using config from file\"}\n",
            "{\"level\":\"error\",\"msg\":\"dial unix /run/x.sock: connection refused\"}\n"
        );
        let picked = reload_error_line(caddy);
        assert!(picked.contains("connection refused"), "{picked}");
        assert!(!picked.contains("using config"), "{picked}");

        // systemctl prints prose; the verdict is the last line.
        let systemd = "Failed to reload caddy.service: Interactive authentication required.\nSee system logs and 'systemctl status caddy.service' for details.\n";
        assert!(reload_error_line(systemd).contains("See system logs"));
        assert_eq!(reload_error_line("   \n"), "(no output)");
    }

    #[test]
    fn reload_plan_picks_the_rootless_path_only_for_a_socket() {
        let file = Path::new("/etc/caddy/Caddyfile");
        let service = vec!["systemctl", "reload", "caddy"];
        for endpoint in [
            AdminEndpoint::Unset,
            AdminEndpoint::Off,
            AdminEndpoint::Tcp("localhost:2019".into()),
        ] {
            let plan = reload_plan(file, &endpoint);
            assert_eq!(plan.argv, service, "{endpoint:?} keeps the service reload");
            assert!(plan.note.is_none());
        }
        assert_eq!(
            reload_plan(file, &AdminEndpoint::Unix("unix//run/caddy/a.sock".into())).argv,
            vec![
                "caddy",
                "reload",
                "--config",
                "/etc/caddy/Caddyfile",
                "--address",
                "unix//run/caddy/a.sock"
            ]
        );
    }

    #[test]
    fn doctor_rejects_unknown_and_incomplete_options() {
        let unknown = run_doctor(&["--bogus".to_string()]).unwrap_err();
        assert!(unknown.to_string().contains("unknown option"));
        let incomplete = run_doctor(&["--caddy-dir".to_string()]).unwrap_err();
        assert!(incomplete.to_string().contains("--caddy-dir needs a value"));
        assert!(run_doctor(&["--help".to_string()]).is_ok());
    }

    #[test]
    fn prefer_release_binary_swaps_a_debug_path_when_release_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        let debug = target.join("debug");
        let release = target.join("release");
        std::fs::create_dir_all(&debug).unwrap();
        std::fs::create_dir_all(&release).unwrap();
        let debug_bin = debug.join("clawde");
        std::fs::write(&debug_bin, "x").unwrap();

        // No release sibling yet -> keep the debug path and flag it.
        assert_eq!(
            prefer_release_binary(debug_bin.clone()),
            (debug_bin.clone(), true)
        );

        // Release sibling present -> prefer it, no warning.
        let release_bin = release.join("clawde");
        std::fs::write(&release_bin, "x").unwrap();
        assert_eq!(prefer_release_binary(debug_bin), (release_bin, false));

        // A non-debug path is returned unchanged without a warning.
        let installed = PathBuf::from("/usr/local/bin/clawde");
        assert_eq!(prefer_release_binary(installed.clone()), (installed, false));
    }
}
