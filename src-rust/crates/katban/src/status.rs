//! `clawde katban status` overview: where state lives, what's hosted, which
//! boards exist, and whether the managed caddy include is in place.

use crate::board;
use crate::caddy::DEFAULT_INCLUDE_NAME;
use crate::config;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KatbanStatus {
    pub data_dir: PathBuf,
    pub site_count: usize,
    pub exposed_count: usize,
    pub board_projects: Vec<String>,
    /// Board projects that have a registered git repo (so the runner can
    /// execute their cards). Empty means boards exist but cannot run until
    /// `clawde katban project set` maps them.
    pub runnable_projects: Vec<String>,
    pub managed_caddy_path: PathBuf,
    pub managed_caddy_exists: bool,
}

/// Aggregate status from disk. `managed_caddy_path` defaults to the standard
/// bare-metal location (`/etc/caddy/katban.conf`); a custom `--caddy-dir` used
/// with `site expose` is shown by that command.
pub fn status() -> KatbanStatus {
    let config = config::load().unwrap_or_default();
    let exposed_count = config
        .sites
        .iter()
        .filter(|site| site.public_subdomain.is_some())
        .count();
    let managed_caddy_path = PathBuf::from("/etc/caddy").join(DEFAULT_INCLUDE_NAME);
    KatbanStatus {
        data_dir: config::katban_data_dir(),
        site_count: config.sites.len(),
        exposed_count,
        board_projects: board::existing_projects(),
        runnable_projects: crate::projects::registered_projects(),
        managed_caddy_exists: managed_caddy_path.exists(),
        managed_caddy_path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{save, KatbanConfig, SiteConfig};

    #[test]
    fn status_counts_sites_boards_and_caddy_path() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("CLAWDE_HOME").ok();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", tmp.path());

        save(&KatbanConfig {
            version: config::CONFIG_VERSION,
            sites: vec![
                SiteConfig {
                    name: "local".to_string(),
                    root: tmp.path().join("local").to_path_buf(),
                    port: 8788,
                    public_subdomain: None,
                    locked: false,
                },
                SiteConfig {
                    name: "public".to_string(),
                    root: tmp.path().join("public").to_path_buf(),
                    port: 8788,
                    public_subdomain: Some("demo.example.com".to_string()),
                    locked: false,
                },
            ],
        })
        .unwrap();

        let mut board = board::Board::new();
        board.add_card("a task");
        board::save_board(&board, "my-repo").unwrap();

        let status = status();
        assert_eq!(status.site_count, 2);
        assert_eq!(status.exposed_count, 1);
        assert_eq!(status.board_projects, vec!["my-repo"]);
        assert_eq!(
            status.managed_caddy_path,
            PathBuf::from("/etc/caddy").join(DEFAULT_INCLUDE_NAME)
        );
        // managed_caddy_exists is environment-dependent; assert the value is a bool.

        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
    }
}
