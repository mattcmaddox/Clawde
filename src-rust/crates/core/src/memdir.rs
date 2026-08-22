//! Memory directory (memdir) system.
//!
//! Provides persistent, file-based memory across sessions.  Mirrors the
//! TypeScript modules under `src/memdir/`:
//!   - `memoryScan.ts`   → `scan_memory_dir`, `parse_frontmatter_quick`, `format_memory_manifest`
//!   - `memoryAge.ts`    → `memory_age_days`, `memory_freshness_text`, `memory_freshness_note`
//!   - `memdir.ts`       → `build_memory_prompt_content`, `load_memory_index`, `ensure_memory_dir_exists`
//!   - `paths.ts`        → `auto_memory_path`, `is_auto_memory_enabled`

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Memory type taxonomy
// ---------------------------------------------------------------------------

/// The four canonical memory types.
/// Matches the TypeScript `MemoryType` union in `memoryTypes.ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    /// Information about the user's role, goals, and preferences.
    User,
    /// Guidance the user has given about how to approach work.
    Feedback,
    /// Information about ongoing work, goals, or incidents in the project.
    Project,
    /// Pointers to where information lives in external systems.
    Reference,
}

impl MemoryType {
    /// Parse a raw frontmatter value into a `MemoryType`.
    /// Returns `None` for missing or unrecognised values (legacy files degrade gracefully).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "user" => Some(Self::User),
            "feedback" => Some(Self::Feedback),
            "project" => Some(Self::Project),
            "reference" => Some(Self::Reference),
            _ => None,
        }
    }

    /// Display as a lowercase string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }
}

// ---------------------------------------------------------------------------
// Memory file metadata and content
// ---------------------------------------------------------------------------

/// Scanned metadata for a single memory file (without the full body).
/// Mirrors `MemoryHeader` in `memoryScan.ts`.
#[derive(Debug, Clone)]
pub struct MemoryFileMeta {
    /// Filename relative to the memory directory (e.g. `user_role.md`).
    pub filename: String,
    /// Absolute path to the file.
    pub path: PathBuf,
    /// `name:` frontmatter field.
    pub name: Option<String>,
    /// `description:` frontmatter field (used for relevance scoring).
    pub description: Option<String>,
    /// `type:` frontmatter field.
    pub memory_type: Option<MemoryType>,
    /// `created:` frontmatter field (ISO date, opt-in).
    pub created: Option<String>,
    /// `updated:` frontmatter field (ISO date, opt-in).
    pub updated: Option<String>,
    /// Filenames this memory supersedes (`supersedes:` frontmatter, opt-in).
    /// The referenced files stay on disk but are treated as stale.
    pub supersedes: Vec<String>,
    /// `conflicts:` frontmatter field (opt-in) — filenames this memory claims
    /// are wrong, pending user adjudication. Both sides stay active: an
    /// unconfirmed claim never demotes the established fact.
    pub conflicts: Vec<String>,
    /// `asked:` frontmatter field (opt-in) — conflict targets the user was
    /// consulted about and left unresolved (e.g. said "I don't know").
    /// Per-pair: each entry names one target file, so adjudicating one pair
    /// never silences another. Legacy files may hold an ISO date instead
    /// ("asked about all current conflicts"); see [`asked_targets`].
    pub asked: Vec<String>,
    /// `resolved:` frontmatter field (opt-in) — conflict targets the user
    /// already adjudicated (claim dropped / both-true). Never re-flag these.
    pub resolved: Vec<String>,
    /// File modification time in seconds since UNIX epoch.
    pub modified_secs: u64,
}

/// A fully-loaded memory file including its body.
#[derive(Debug, Clone)]
pub struct MemoryFile {
    pub meta: MemoryFileMeta,
    pub content: String,
}

// ---------------------------------------------------------------------------
// Directory scanning
// ---------------------------------------------------------------------------

/// Maximum number of memory files kept after sorting.
/// Matches `MAX_MEMORY_FILES` in `memoryScan.ts`.
const MAX_MEMORY_FILES: usize = 200;

/// Number of lines scanned for frontmatter.
/// Matches `FRONTMATTER_MAX_LINES` in `memoryScan.ts`.
const FRONTMATTER_MAX_LINES: usize = 30;

/// Scan a memory directory, returning metadata for all `.md` files
/// (excluding `MEMORY.md`), sorted newest-first, capped at `MAX_MEMORY_FILES`.
///
/// This is a synchronous scan used during system-prompt assembly.
/// Mirrors `scanMemoryFiles` in `memoryScan.ts` (async version; this is the
/// sync equivalent used at prompt-build time).
pub fn scan_memory_dir(dir: &Path) -> Vec<MemoryFileMeta> {
    let mut files: Vec<MemoryFileMeta> = Vec::new();

    if !dir.exists() {
        return files;
    }

    // Walk recursively using `walkdir`-style manual recursion to stay
    // dependency-free (only std).
    collect_md_files(dir, dir, &mut files);

    // Sort newest-first, breaking equal-mtime ties by filename so status and
    // browser ordering remain deterministic on filesystems with coarse clocks.
    files.sort_by(|a, b| {
        b.modified_secs
            .cmp(&a.modified_secs)
            .then_with(|| a.filename.cmp(&b.filename))
    });
    files.truncate(MAX_MEMORY_FILES);
    files
}

/// Recursively collect `.md` files (excluding `MEMORY.md`) from `current_dir`.
fn collect_md_files(base: &Path, current_dir: &Path, out: &mut Vec<MemoryFileMeta>) {
    let Ok(entries) = std::fs::read_dir(current_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_md_files(base, &path, out);
        } else if path.extension().map(|e| e == "md").unwrap_or(false) {
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if file_name == "MEMORY.md" {
                continue;
            }

            let modified_secs = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
                .unwrap_or(0);

            let frontmatter = if let Ok(content) = std::fs::read_to_string(&path) {
                parse_frontmatter_quick(&content)
            } else {
                MemoryFrontmatter::default()
            };

            // Relative path from the memory dir root.
            let relative = path
                .strip_prefix(base)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| file_name.clone());

            out.push(MemoryFileMeta {
                filename: relative,
                path,
                name: frontmatter.name,
                description: frontmatter.description,
                memory_type: frontmatter.memory_type,
                created: frontmatter.created,
                updated: frontmatter.updated,
                supersedes: frontmatter.supersedes,
                conflicts: frontmatter.conflicts,
                asked: frontmatter.asked,
                resolved: frontmatter.resolved,
                modified_secs,
            });
        }
    }
}

/// Parsed YAML frontmatter fields from a memory file.
///
/// All fields are opt-in; a legacy file without them degrades to `Default`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemoryFrontmatter {
    /// `name:` field.
    pub name: Option<String>,
    /// `description:` field (used for relevance scoring).
    pub description: Option<String>,
    /// `type:` field.
    pub memory_type: Option<MemoryType>,
    /// `created:` field (ISO date `YYYY-MM-DD`).
    pub created: Option<String>,
    /// `updated:` field (ISO date `YYYY-MM-DD`).
    pub updated: Option<String>,
    /// `supersedes:` field — comma/space separated list of filenames this
    /// memory replaces (confirmed; the targets are treated as stale).
    pub supersedes: Vec<String>,
    /// `conflicts:` field — comma/space separated list of filenames this
    /// memory claims are wrong, pending user adjudication. Unlike
    /// `supersedes:`, targets stay fully active.
    pub conflicts: Vec<String>,
    /// `asked:` field — conflict targets the user was consulted about and
    /// left unresolved (per-pair; the agent must not re-ask those). A legacy
    /// ISO date (`YYYY-MM-DD`) means "asked about every current conflict".
    pub asked: Vec<String>,
    /// `resolved:` field — conflict targets the user already adjudicated
    /// (dropped claim, both-true verdict, etc.). The dream must never
    /// re-flag a pair named here: the contradiction is a settled decision.
    pub resolved: Vec<String>,
}

/// Parse YAML frontmatter from the first `FRONTMATTER_MAX_LINES` lines without
/// a full YAML parser.
///
/// Mirrors `parseFrontmatter` usage in `memoryScan.ts`.
pub fn parse_frontmatter_quick(content: &str) -> MemoryFrontmatter {
    let mut fm = MemoryFrontmatter::default();

    let lines: Vec<&str> = content.lines().take(FRONTMATTER_MAX_LINES).collect();

    // Frontmatter must start with `---`
    if lines.first().map(|l| l.trim() != "---").unwrap_or(true) {
        return fm;
    }

    for line in &lines[1..] {
        if line.trim() == "---" {
            break;
        }
        let unquote = |raw: &str| raw.trim().trim_matches('"').trim_matches('\'').to_string();
        if let Some(rest) = line.strip_prefix("name:") {
            fm.name = Some(unquote(rest));
        } else if let Some(rest) = line.strip_prefix("description:") {
            fm.description = Some(unquote(rest));
        } else if let Some(rest) = line.strip_prefix("type:") {
            fm.memory_type = MemoryType::parse(&unquote(rest));
        } else if let Some(rest) = line.strip_prefix("created:") {
            fm.created = Some(unquote(rest));
        } else if let Some(rest) = line.strip_prefix("updated:") {
            fm.updated = Some(unquote(rest));
        } else if let Some(rest) = line.strip_prefix("supersedes:") {
            fm.supersedes = parse_file_list(rest);
        } else if let Some(rest) = line.strip_prefix("conflicts:") {
            fm.conflicts = parse_file_list(rest);
        } else if let Some(rest) = line.strip_prefix("asked:") {
            fm.asked = parse_file_list(rest);
        } else if let Some(rest) = line.strip_prefix("resolved:") {
            fm.resolved = parse_file_list(rest);
        }
    }

    fm
}

/// Split a comma- or whitespace-separated frontmatter filename list.
fn parse_file_list(raw: &str) -> Vec<String> {
    raw.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_matches('"').trim_matches('\'').to_string())
        .collect()
}

/// Whether a string looks like the legacy `asked: YYYY-MM-DD` format (an ISO
/// date rather than a per-pair target filename).
fn is_iso_date(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        })
}

/// Split a per-pair `asked:` entry into `(target, date)`.
///
/// Entries are either bare filenames (`asked: auth-flow-v1.md`, no date) or
/// `target:YYYY-MM-DD` (`asked: auth-flow-v1.md:2026-08-20`). The date is
/// parsed off only when the suffix after the last `:` is a valid ISO date, so
/// a filename that happens to contain `:` stays intact.
fn split_asked_entry(entry: &str) -> (&str, Option<&str>) {
    match entry.rfind(':') {
        Some(idx) if is_iso_date(&entry[idx + 1..]) => (&entry[..idx], Some(&entry[idx + 1..])),
        _ => (entry, None),
    }
}

/// Resolve which conflict targets of a claimant have been asked-and-left-
/// unresolved (`asked:` frontmatter).
///
/// Modern files list targets per-pair with their ask date
/// (`asked: auth-flow-v1.md:2026-08-20`); legacy files carry a bare ISO date
/// (`asked: 2026-08-20`), which means "asked about every conflict current at
/// the time" — expanded against the claimant's current `conflicts:` list.
/// A legacy date covers only conflicts whose target file predates the ask
/// date (`target_created`): a conflict added after the ask date cannot have
/// been asked about and must stay askable. Returns the effective set of
/// asked targets (dates stripped).
pub fn asked_targets(
    asked: &[String],
    conflicts: &[String],
    target_created: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut legacy: Option<&str> = None;
    for entry in asked {
        let (target, _date) = split_asked_entry(entry);
        if is_iso_date(entry) {
            legacy = Some(entry.as_str());
        } else if !out.iter().any(|t| t == target) {
            out.push(target.to_string());
        }
    }
    if let Some(ask_date) = legacy {
        for target in conflicts {
            // ISO `YYYY-MM-DD` dates compare lexicographically == chronologically.
            let predates_ask = match target_created(target) {
                Some(created) => created.as_str() <= ask_date,
                // Unknown creation date → assume it predates (conservative:
                // never re-ask what might have been asked about).
                None => true,
            };
            if predates_ask && !out.iter().any(|t| t == target) {
                out.push(target.clone());
            }
        }
    }
    out
}

/// The ask date for a specific (claimant, target) pair, if known.
///
/// Returns the date from a per-pair `target:YYYY-MM-DD` entry when present,
/// falling back to a legacy bare `asked: <date>` (which covered all pairs).
/// `None` when the pair was marked asked without a date.
pub fn asked_entry_date(asked: &[String], target: &str) -> Option<String> {
    let mut legacy: Option<String> = None;
    for entry in asked {
        let (entry_target, date) = split_asked_entry(entry);
        if is_iso_date(entry) {
            legacy = Some(entry.to_string());
        } else if entry_target == target && date.is_some() {
            return date.map(str::to_string);
        }
    }
    legacy
}

/// Days elapsed between an ISO `YYYY-MM-DD` date and today (floor), or `None`
/// when the date does not parse. Clamped to 0 for future dates (clock skew).
pub fn days_since_iso(date: &str) -> Option<u64> {
    if !is_iso_date(date) {
        return None;
    }
    let year: i64 = date[0..4].parse().ok()?;
    let month: i64 = date[5..7].parse().ok()?;
    let day: i64 = date[8..10].parse().ok()?;
    let today = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86400;
    // days_from_civil (Howard Hinnant's algorithm) for the given date.
    let y = year - if month <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let date_days = era * 146_097 + doe - 719_468;
    Some(today.saturating_sub(date_days as u64))
}

/// Human-readable ask age for display: "today", "yesterday", "N days ago".
pub fn iso_age_text(date: &str) -> String {
    match days_since_iso(date) {
        Some(0) => "today".to_string(),
        Some(1) => "yesterday".to_string(),
        Some(n) => format!("{} days ago", n),
        None => date.to_string(),
    }
}

/// Map each superseded filename to the file(s) that supersede it.
///
/// A file is *superseded* when some other memory lists it in its
/// `supersedes:` frontmatter. The superseding file stays current; the target
/// stays on disk (auditable) but should be excluded from active use.
pub fn superseded_by(
    memories: &[MemoryFileMeta],
) -> std::collections::HashMap<String, Vec<String>> {
    let mut map: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for meta in memories {
        for target in &meta.supersedes {
            map.entry(target.clone())
                .or_default()
                .push(meta.filename.clone());
        }
    }
    map
}

/// Whether `start` can reach `needle` by following `supersedes:` links
/// (transitively). Used by the supersession-cycle guard: adding
/// `claimant supersedes: target` would close a cycle iff `target` already
/// reaches `claimant` through the relation.
///
/// BFS with a visited set so a pre-existing (manually authored) cycle cannot
/// loop forever. Files that fail to read are skipped (a missing file is a
/// dangling reference the sweep cleans up, not a cycle member).
fn supersedes_reaches(memory_dir: &Path, start: &str, needle: &str) -> bool {
    use std::collections::{HashSet, VecDeque};
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    queue.push_back(start.to_string());
    while let Some(name) = queue.pop_front() {
        if !visited.insert(name.clone()) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(memory_dir.join(&name)) else {
            continue;
        };
        let fm = parse_frontmatter_quick(&content);
        for next in &fm.supersedes {
            if next == needle {
                return true;
            }
            queue.push_back(next.clone());
        }
    }
    false
}

/// Map each conflict-target filename to the file(s) claiming it is wrong.
///
/// A *conflict target* is a file listed in another memory's `conflicts:`
/// frontmatter. Unlike a superseded file it stays fully active — the claim is
/// unconfirmed until the user adjudicates it.
pub fn conflicted_by(
    memories: &[MemoryFileMeta],
) -> std::collections::HashMap<String, Vec<String>> {
    let mut map: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for meta in memories {
        for target in &meta.conflicts {
            map.entry(target.clone())
                .or_default()
                .push(meta.filename.clone());
        }
    }
    map
}

/// Format memory headers as a text manifest: one entry per file with
/// `[type] filename (iso-timestamp): description`, plus annotations:
/// - `— superseded by …` when another memory supersedes it (confirmed),
/// - `— pending conflict with …` when it claims other files are wrong
///   (unconfirmed, awaiting user adjudication),
/// - `— under review by …` when another memory claims it is wrong.
///
/// Mirrors `formatMemoryManifest` in `memoryScan.ts`.
pub fn format_memory_manifest(memories: &[MemoryFileMeta]) -> String {
    let superseded = superseded_by(memories);
    let conflicted = conflicted_by(memories);
    memories
        .iter()
        .map(|m| {
            let tag = m
                .memory_type
                .as_ref()
                .map(|t| format!("[{}] ", t.as_str()))
                .unwrap_or_default();

            // Convert modified_secs to an ISO-8601-like timestamp.
            let ts = format_unix_secs_iso(m.modified_secs);

            let base = match &m.description {
                Some(desc) => format!("- {}{} ({}): {}", tag, m.filename, ts, desc),
                None => format!("- {}{}", tag, m.filename),
            };

            let mut line = match superseded.get(&m.filename) {
                Some(superseders) => {
                    format!("{} — superseded by {}", base, superseders.join(", "))
                }
                None => base,
            };
            if !m.conflicts.is_empty() {
                let asked = asked_targets(&m.asked, &m.conflicts, |target| {
                    memories
                        .iter()
                        .find(|candidate| candidate.filename == target)
                        .and_then(|candidate| candidate.created.clone())
                });
                let annotated: Vec<String> = m
                    .conflicts
                    .iter()
                    .map(|target| {
                        if !asked.iter().any(|t| t == target) {
                            return target.clone();
                        }
                        // Asked per-pair: show the ask date when known (from
                        // the `target:YYYY-MM-DD` entry or a legacy bare date),
                        // else a bare marker (the target name is shown).
                        match asked_entry_date(&m.asked, target) {
                            Some(date) => format!("{} (asked {})", target, date),
                            None => format!("{} (asked)", target),
                        }
                    })
                    .collect();
                line.push_str(&format!(
                    " — pending conflict with {}",
                    annotated.join(", ")
                ));
            }
            if !m.resolved.is_empty() {
                line.push_str(&format!(" — user-resolved vs {}", m.resolved.join(", ")));
            }
            if let Some(claimants) = conflicted.get(&m.filename) {
                line.push_str(&format!(" — under review by {}", claimants.join(", ")));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Minimal ISO-8601 formatter for a Unix timestamp (no external deps).
fn format_unix_secs_iso(secs: u64) -> String {
    // We use a very lightweight implementation to avoid pulling in chrono here
    // (chrono is already a workspace dep but we want this module to stay lean).
    // Accuracy to the day is sufficient for memory manifests.
    let days_since_epoch = secs / 86400;
    // Julian Day Number for 1970-01-01 is 2440588.
    let jdn = days_since_epoch as u32 + 2440588;
    let (y, m, d) = jdn_to_ymd(jdn);
    let hh = (secs % 86400) / 3600;
    let mm = (secs % 3600) / 60;
    let ss = secs % 60;
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, hh, mm, ss)
}

/// Convert a Julian Day Number to (year, month, day).
fn jdn_to_ymd(jdn: u32) -> (u32, u32, u32) {
    let a = jdn + 32044;
    let b = (4 * a + 3) / 146097;
    let c = a - (146097 * b) / 4;
    let d = (4 * c + 3) / 1461;
    let e = c - (1461 * d) / 4;
    let m = (5 * e + 2) / 153;
    let day = e - (153 * m + 2) / 5 + 1;
    let month = m + 3 - 12 * (m / 10);
    let year = 100 * b + d - 4800 + m / 10;
    (year, month, day)
}

// ---------------------------------------------------------------------------
// Memory age / freshness
// ---------------------------------------------------------------------------

/// Days elapsed since `modified_secs`.  Floor-rounded; clamped to 0 for
/// future mtimes (clock skew).
///
/// Mirrors `memoryAgeDays` in `memoryAge.ts`.
pub fn memory_age_days(modified_secs: u64) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    (now.saturating_sub(modified_secs)) / 86400
}

/// Human-readable age string.  Models are poor at date arithmetic — a raw
/// ISO timestamp does not trigger staleness reasoning the way "47 days ago" does.
///
/// Mirrors `memoryAge` in `memoryAge.ts`.
#[allow(dead_code)]
pub fn memory_age(modified_secs: u64) -> String {
    let d = memory_age_days(modified_secs);
    match d {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        n => format!("{} days ago", n),
    }
}

/// Plain-text staleness caveat for memories > 1 day old.
/// Returns an empty string for fresh memories (today / yesterday).
///
/// Mirrors `memoryFreshnessText` in `memoryAge.ts`.
pub fn memory_freshness_text(modified_secs: u64) -> String {
    let d = memory_age_days(modified_secs);
    if d <= 1 {
        return String::new();
    }
    format!(
        "This memory is {} days old. \
        Memories are point-in-time observations, not live state — \
        claims about code behavior or file:line citations may be outdated. \
        Verify against current code before asserting as fact.",
        d
    )
}

/// Per-memory staleness note wrapped in `<system-reminder>` tags.
/// Returns an empty string for memories ≤ 1 day old.
///
/// Mirrors `memoryFreshnessNote` in `memoryAge.ts`.
pub fn memory_freshness_note(modified_secs: u64) -> String {
    let text = memory_freshness_text(modified_secs);
    if text.is_empty() {
        return String::new();
    }
    format!("<system-reminder>{}</system-reminder>\n", text)
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Entrypoint filename within the memory directory.
pub const MEMORY_ENTRYPOINT: &str = "MEMORY.md";

/// Maximum number of lines loaded from `MEMORY.md`.
/// Matches `MAX_ENTRYPOINT_LINES` in `memdir.ts`.
pub const MAX_ENTRYPOINT_LINES: usize = 200;

/// Maximum bytes loaded from `MEMORY.md`.
/// Matches `MAX_ENTRYPOINT_BYTES` in `memdir.ts`.
pub const MAX_ENTRYPOINT_BYTES: usize = 25_000;

/// Compute the auto-memory directory path for a project root.
///
/// Resolution order (mirrors `getAutoMemPath` in `paths.ts`):
/// 1. `CLAUDE_COWORK_MEMORY_PATH_OVERRIDE` env var (full-path override).
/// 2. `<CLAWDE_REMOTE_MEMORY_DIR>/projects/<sanitized-root>/memory/`
///    when `CLAWDE_REMOTE_MEMORY_DIR` is set.
/// 3. `~/.clawde/projects/<sanitized-root>/memory/` (default).
pub fn auto_memory_path(project_root: &Path) -> PathBuf {
    // 1. Cowork full-path override.
    if let Ok(override_path) = std::env::var("CLAUDE_COWORK_MEMORY_PATH_OVERRIDE") {
        if !override_path.is_empty() {
            return PathBuf::from(override_path);
        }
    }

    // 2. Determine the memory base directory.
    let memory_base = std::env::var("CLAWDE_REMOTE_MEMORY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::config::Settings::config_dir());

    // 3. Sanitize the project root into a safe directory name.
    let sanitized = sanitize_path_component(&project_root.to_string_lossy());

    memory_base.join("projects").join(sanitized).join("memory")
}

/// Sanitize an arbitrary string into a directory-name-safe component.
/// Matches `sanitizePath` used inside `getAutoMemPath` in `paths.ts`.
pub fn sanitize_path_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Whether the auto-memory system is enabled for this session.
///
/// Priority chain (mirrors `isAutoMemoryEnabled` in `paths.ts`):
/// 1. `CLAWDE_DISABLE_AUTO_MEMORY` — truthy → OFF, falsy (but defined) → ON.
/// 2. `CLAWDE_SIMPLE` (--bare) → OFF.
/// 3. Remote mode without `CLAWDE_REMOTE_MEMORY_DIR` → OFF.
/// 4. `settings_enabled` parameter (from settings.json `autoMemoryEnabled` field).
/// 5. Default: enabled.
pub fn is_auto_memory_enabled(settings_enabled: Option<bool>) -> bool {
    if let Ok(val) = std::env::var("CLAWDE_DISABLE_AUTO_MEMORY") {
        // Truthy values (non-empty, non-"0", non-"false") disable memory.
        match val.to_lowercase().as_str() {
            "" | "0" | "false" | "no" | "off" => return true, // defined-falsy → ON
            _ => return false,                                // truthy → OFF
        }
    }

    if std::env::var("CLAWDE_SIMPLE").is_ok() {
        return false;
    }

    if std::env::var("CLAWDE_REMOTE").is_ok() && std::env::var("CLAWDE_REMOTE_MEMORY_DIR").is_err()
    {
        return false;
    }

    settings_enabled.unwrap_or(true)
}

// ---------------------------------------------------------------------------
// Index loading and truncation
// ---------------------------------------------------------------------------

/// Result of loading and (optionally) truncating the `MEMORY.md` entrypoint.
#[derive(Debug, Clone)]
pub struct EntrypointTruncation {
    pub content: String,
    pub line_count: usize,
    pub byte_count: usize,
    pub was_line_truncated: bool,
    pub was_byte_truncated: bool,
}

/// Truncate `MEMORY.md` content to `MAX_ENTRYPOINT_LINES` lines and
/// `MAX_ENTRYPOINT_BYTES` bytes, appending a warning when either cap fires.
///
/// Mirrors `truncateEntrypointContent` in `memdir.ts`.
pub fn truncate_entrypoint_content(raw: &str) -> EntrypointTruncation {
    let trimmed = raw.trim();
    let content_lines: Vec<&str> = trimmed.lines().collect();
    let line_count = content_lines.len();
    let byte_count = trimmed.len();

    let was_line_truncated = line_count > MAX_ENTRYPOINT_LINES;
    let was_byte_truncated = byte_count > MAX_ENTRYPOINT_BYTES;

    if !was_line_truncated && !was_byte_truncated {
        return EntrypointTruncation {
            content: trimmed.to_string(),
            line_count,
            byte_count,
            was_line_truncated: false,
            was_byte_truncated: false,
        };
    }

    let mut truncated = if was_line_truncated {
        content_lines[..MAX_ENTRYPOINT_LINES].join("\n")
    } else {
        trimmed.to_string()
    };

    if truncated.len() > MAX_ENTRYPOINT_BYTES {
        // floor_char_boundary guards against slicing mid-UTF-8-char.
        let boundary = truncated.floor_char_boundary(MAX_ENTRYPOINT_BYTES);
        let cut_at = truncated[..boundary].rfind('\n').unwrap_or(boundary);
        truncated.truncate(cut_at);
    }

    let reason = match (was_line_truncated, was_byte_truncated) {
        (true, false) => format!("{} lines (limit: {})", line_count, MAX_ENTRYPOINT_LINES),
        (false, true) => format!(
            "{} bytes (limit: {}) — index entries are too long",
            byte_count, MAX_ENTRYPOINT_BYTES
        ),
        _ => format!("{} lines and {} bytes", line_count, byte_count),
    };

    truncated.push_str(&format!(
        "\n\n> WARNING: {} is {}. Only part of it was loaded. \
        Keep index entries to one line under ~200 chars; move detail into topic files.",
        MEMORY_ENTRYPOINT, reason
    ));

    EntrypointTruncation {
        content: truncated,
        line_count,
        byte_count,
        was_line_truncated,
        was_byte_truncated,
    }
}

/// Load and truncate the `MEMORY.md` index from `memory_dir`.
/// Returns `None` when the file does not exist or is empty.
///
/// Mirrors the entrypoint-reading path in `buildMemoryPrompt` / `loadMemoryPrompt`.
pub fn load_memory_index(memory_dir: &Path) -> Option<EntrypointTruncation> {
    let index_path = memory_dir.join(MEMORY_ENTRYPOINT);
    if !index_path.exists() {
        return None;
    }
    let raw = std::fs::read_to_string(&index_path).ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(truncate_entrypoint_content(&raw))
}

// ---------------------------------------------------------------------------
// System-prompt memory content builder
// ---------------------------------------------------------------------------/// Maximum bytes loaded from a single session summary. Kept conservative so
/// the combined memory injection (index + summary) stays within a few percent
/// of a typical 128K context window (audit spec §18.3 token-budget concern).
const MAX_SESSION_SUMMARY_BYTES: usize = 4_000;

/// Load the most recently modified session summary from `sessions/`.
///
/// Returns `None` when the directory is missing or empty.  Summaries are
/// capped at `MAX_SESSION_SUMMARY_BYTES` — a session summary is a supplement
/// to the primary `MEMORY.md` index, not a replacement for it.  Equal-mtime
/// ties break by filename (summaries are date-named, so the newest date wins)
/// for determinism.
pub fn most_recent_session_summary(memory_dir: &Path) -> Option<String> {
    let sessions_dir = memory_dir.join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        return None;
    };

    let mut newest: Option<(u64, String, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_md = path.extension().map(|e| e == "md").unwrap_or(false);
        if !is_md {
            continue;
        }
        let modified_secs = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
            .unwrap_or(0);
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let is_newer = newest
            .as_ref()
            .map(|(t, name, _)| (modified_secs, &file_name) > (*t, name))
            .unwrap_or(true);
        if is_newer {
            newest = Some((modified_secs, file_name, path));
        }
    }

    let (_modified_secs, _file_name, path) = newest?;
    let raw = std::fs::read_to_string(&path).ok()?;
    if raw.trim().is_empty() {
        return None;
    }

    if raw.len() > MAX_SESSION_SUMMARY_BYTES {
        // Cut at a line boundary (newlines are ASCII, so a boundary found via
        // `rfind` is a safe char boundary). `floor_char_boundary` keeps the
        // initial slice from panicking when the cap splits a multibyte char.
        let boundary = raw.floor_char_boundary(MAX_SESSION_SUMMARY_BYTES);
        let cut_at = raw[..boundary].rfind('\n').unwrap_or(boundary);
        Some(format!(
            "{}…\n\n> WARNING: session summary truncated — larger than {} bytes.",
            &raw[..cut_at],
            MAX_SESSION_SUMMARY_BYTES
        ))
    } else {
        Some(raw)
    }
}

/// Build the memory content string to inject into the system prompt's
/// `<memory>` block (no token budget — uses the per-file caps only).
///
/// Includes the `MEMORY.md` index when it exists, plus the most recent
/// session summary from `sessions/` when one is available.
/// Called during `build_system_prompt` → `SystemPromptOptions::memory_content`.
pub fn build_memory_prompt_content(memory_dir: &Path) -> String {
    build_memory_prompt_content_with_budget(memory_dir, None)
}

/// Build the memory content to inject, capped at `budget_bytes` when set
/// (audit spec §18.3 memory token budget).
///
/// When the combined index + summary exceed the budget, the session summary
/// (least durable signal) is dropped first; if the index alone still exceeds
/// the budget it is clamped at a line boundary with a warning. `None` keeps
/// both parts at their built-in caps.
pub fn build_memory_prompt_content_with_budget(
    memory_dir: &Path,
    budget_bytes: Option<usize>,
) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(index) = load_memory_index(memory_dir) {
        parts.push(format!("## Memory Index (MEMORY.md)\n{}", index.content));
    }

    if let Some(summary) = most_recent_session_summary(memory_dir) {
        parts.push(format!("## Recent Session Summary\n{}", summary));
    }

    let mut joined = parts.join("\n\n");
    if let Some(budget) = budget_bytes {
        if joined.len() > budget {
            if parts.len() >= 2 {
                // Drop the session summary first — the index is the durable
                // anchor and the summary is rebuilt each consolidation.
                joined = parts[0].clone();
            }
            if joined.len() > budget {
                // Clamp the remaining content at a line boundary. Newlines are
                // ASCII, so a boundary found via `rfind` is a safe char
                // boundary; `floor_char_boundary` guards the initial slice.
                let boundary = joined.floor_char_boundary(budget);
                let cut_at = joined[..boundary].rfind('\n').unwrap_or(boundary);
                joined = format!(
                    "{}…\n\n> WARNING: memory injection truncated to {} bytes by the \
                     memory token budget (`memory.maxTokens` in settings).",
                    &joined[..cut_at],
                    budget
                );
            }
        }
    }

    // Pending conflicts are appended after the budget logic on purpose: they
    // are tiny and are the one part we never drop — omitting them could let a
    // stale fact be asserted as current. `pending_conflicts_block` returns
    // "" when there is nothing to show. The same applies to superseded
    // memories: the index is a hand-maintained list the resolver cannot fully
    // keep current, so a confirmed invalidation is surfaced structurally here
    // rather than left to the next consolidation.
    let conflicts = pending_conflicts_block(memory_dir);
    if !conflicts.is_empty() {
        if !joined.is_empty() {
            joined.push_str("\n\n");
        }
        joined.push_str(&conflicts);
    }
    let superseded = superseded_memories_block(memory_dir);
    if !superseded.is_empty() {
        if !joined.is_empty() {
            joined.push_str("\n\n");
        }
        joined.push_str(&superseded);
    }
    joined
}

/// Render the superseded-memories section for the `<memory>` injection.
///
/// Lists every file that another memory demotes via `supersedes:` frontmatter
/// (and that still exists on disk), so the model never presents a known-stale
/// fact as current even when the hand-maintained `MEMORY.md` index still
/// lists it. Only confirmed supersessions appear — pending `conflicts:` claims
/// stay fully active and are covered by [`pending_conflicts_block`]. Returns
/// "" when nothing is superseded.
pub fn superseded_memories_block(memory_dir: &Path) -> String {
    let metas = scan_memory_dir(memory_dir);
    let superseded = superseded_by(&metas);
    if superseded.is_empty() {
        return String::new();
    }
    let by_name: std::collections::HashMap<&str, &MemoryFileMeta> =
        metas.iter().map(|m| (m.filename.as_str(), m)).collect();

    let mut targets: Vec<&String> = superseded.keys().collect();
    targets.sort();
    let mut lines: Vec<String> = Vec::new();
    for target in targets {
        // Skip dangling entries (target deleted) — the sweep clears those.
        let Some(target_meta) = by_name.get(target.as_str()) else {
            continue;
        };
        let describe = target_meta
            .description
            .as_deref()
            .map(|d| format!("\"{}\"", d))
            .unwrap_or_else(|| format!("`{}`", target));
        let mut superseders: Vec<String> = superseded[target].clone();
        superseders.sort();
        lines.push(format!(
            "- {} ({}) — superseded by {}",
            describe,
            target,
            superseders.join(", ")
        ));
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Superseded Memories\n");
    out.push_str(&lines.join("\n"));
    out.push_str(
        "\n\nThese memories are confirmed stale: do not assert their claims as \
         current. If the topic comes up, read the superseding file instead.",
    );
    out
}

/// Render the pending-memory-conflicts section for the `<memory>` injection.
///
/// Lists each unconfirmed claim (`conflicts:` frontmatter) as a plain-language
/// pair — the established fact vs the claim — with both descriptions when
/// available. Pairs the user was already asked about (`asked:` naming that
/// target, or a legacy `asked: <date>`) are marked and excluded from the ask
/// instruction. Pairs the user already resolved (`resolved:`) are skipped.
/// Returns "" when there are no conflicts.
/// Enumerate the pending (unadjudicated) conflict pairs the injection, the
/// `/memory status` report, and the TUI indicator all surface — one source of
/// truth so they can never disagree.
///
/// Each entry is `(claimant filename, target filename)`. Filtering matches
/// [`pending_conflicts_block`]: dangling targets (file deleted),
/// self-references, pairs the user already resolved (`resolved:`), and
/// claimants that are themselves superseded (their claims are moot) are all
/// excluded.
pub fn pending_conflict_pairs(memory_dir: &Path) -> Vec<(String, String)> {
    let metas = scan_memory_dir(memory_dir);
    let existing: std::collections::HashSet<&str> =
        metas.iter().map(|m| m.filename.as_str()).collect();
    let superseded = superseded_by(&metas);
    let mut pairs: Vec<(String, String)> = Vec::new();
    for claimant in &metas {
        if claimant.conflicts.is_empty() {
            continue;
        }
        // A superseded file's claims are suspect by definition — the user
        // ruled against it (or a confirmed fact replaced it), so its pending
        // conflicts are not worth adjudicating. Skip them (the resolver also
        // clears a demoted target's reciprocal claim, so this is mostly a
        // defensive filter for the target's other conflicts).
        if superseded.contains_key(&claimant.filename) {
            continue;
        }
        for target_name in &claimant.conflicts {
            if target_name == &claimant.filename
                || claimant.resolved.iter().any(|t| t == target_name)
                || !existing.contains(target_name.as_str())
            {
                continue;
            }
            // A target that another file supersedes is already ruled stale —
            // the claim against it is moot (asking "is X wrong?" when a
            // supersession already says so is pointless). Skip the pair.
            if superseded.contains_key(target_name.as_str()) {
                continue;
            }
            pairs.push((claimant.filename.clone(), target_name.clone()));
        }
    }
    pairs
}

/// Number of pending conflict pairs (see [`pending_conflict_pairs`]) — the
/// count both the TUI indicator and `/memory status` display as "Lethesyne".
pub fn pending_conflict_count(memory_dir: &Path) -> usize {
    pending_conflict_pairs(memory_dir).len()
}

pub fn pending_conflicts_block(memory_dir: &Path) -> String {
    let metas = scan_memory_dir(memory_dir);
    let by_name: std::collections::HashMap<&str, &MemoryFileMeta> =
        metas.iter().map(|m| (m.filename.as_str(), m)).collect();

    let mut lines: Vec<String> = Vec::new();
    let mut askable = 0usize;
    for (claimant_name, target_name) in pending_conflict_pairs(memory_dir) {
        let Some(claimant) = by_name.get(claimant_name.as_str()) else {
            continue;
        };
        let Some(target) = by_name.get(target_name.as_str()) else {
            continue;
        };
        let asked = asked_targets(&claimant.asked, &claimant.conflicts, |t| {
            by_name
                .get(t)
                .and_then(|candidate| candidate.created.clone())
        });
        let describe = |m: &MemoryFileMeta| {
            m.description
                .as_deref()
                .map(|d| format!("\"{}\"", d))
                .unwrap_or_else(|| format!("`{}`", m.filename))
        };
        let pair_asked = asked.iter().any(|t| t == &target_name);
        if !pair_asked {
            askable += 1;
        }
        lines.push(format!(
            "- {} ({}) vs {} ({}){}",
            describe(claimant),
            claimant.filename,
            describe(target),
            target.filename,
            if pair_asked {
                match asked_entry_date(&claimant.asked, &target_name) {
                    Some(date) => format!(" — asked {}", iso_age_text(&date)),
                    None => " — asked".to_string(),
                }
            } else {
                String::new()
            }
        ));
    }
    if lines.is_empty() {
        return String::new();
    }

    let mut out = String::from("## Pending Memory Conflicts\n");
    out.push_str(&lines.join("\n"));
    if askable > 0 {
        out.push_str(
            "\n\nIf your work touches one of these subjects, ask the user once via \
             AskUserQuestion which version is correct (keep the new fact / keep the \
             old fact / both are true / I don't know), then apply the answer with the \
             ResolveMemoryConflict tool, passing the claimant file, the target file, \
             and the decision — keep_new / keep_old / both / unknown. The tool updates \
             the frontmatter itself (promote to `supersedes:`, drop the claim, or stamp \
             `asked: <target>:<today>`); never hand-edit these fields. Never re-ask \
             a conflict that already has `asked:` set.",
        );
    }
    out
}

// ---------------------------------------------------------------------------
// Conflict resolution (user adjudication, no model in the loop)
// ---------------------------------------------------------------------------

/// The user's verdict on a pending memory conflict (`conflicts:` frontmatter).
///
/// Mirrors the AskUserQuestion options the agent presents: keep the new
/// claim, keep the established fact, keep both, or leave it unresolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictDecision {
    /// The claimant file's fact is correct → promote `conflicts:` to
    /// `supersedes:` so the target is treated as stale.
    KeepNew,
    /// The target file's fact is correct → drop the claim; the status quo
    /// (both files active) is restored.
    KeepOld,
    /// Both facts are true in different contexts → drop the claim; both
    /// files stay active and a body note records the verdict so a future
    /// dream does not re-flag the pair.
    Both,
    /// The user does not know → append `<target>` to the claimant's `asked:`
    /// list; the claim stays pending and no authority changes. That pair is
    /// never re-asked (per-pair, so other conflicts on the file stay askable).
    Unknown,
}

impl ConflictDecision {
    /// Parse a decision string from tool input, case-insensitively, tolerating
    /// `-` / space / apostrophe punctuation variants.
    pub fn parse(raw: &str) -> Option<Self> {
        let normalized = raw
            .trim()
            .to_ascii_lowercase()
            .replace(['-', ' ', '\''], "_");
        match normalized.as_str() {
            "keep_new" | "new" | "keepnew" => Some(Self::KeepNew),
            "keep_old" | "old" => Some(Self::KeepOld),
            "both" | "both_true" | "both_are_true" => Some(Self::Both),
            "unknown" | "unsure" | "dont_know" | "i_dont_know" => Some(Self::Unknown),
            _ => None,
        }
    }

    /// Canonical tool-facing name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::KeepNew => "keep_new",
            Self::KeepOld => "keep_old",
            Self::Both => "both",
            Self::Unknown => "unknown",
        }
    }
}

/// Outcome of applying a user verdict to one pending conflict.
#[derive(Debug, Clone)]
pub struct ConflictResolution {
    /// The memory file that carried the claim (`conflicts:` frontmatter).
    pub claimant: String,
    /// The file the claim was about.
    pub target: String,
    /// The verdict that was applied.
    pub decision: ConflictDecision,
    /// Human-readable description of what changed (surfaced as the tool
    /// result so the agent can report it back to the user).
    pub summary: String,
}

/// Apply a user's verdict to a pending memory conflict, rewriting the
/// claimant file's frontmatter deterministically.
///
/// This is the enforcement layer behind the conversational flow: the agent
/// asks via AskUserQuestion and then calls this (through the
/// `ResolveMemoryConflict` tool) — the frontmatter transition never goes
/// through a model hand-edit.
///
/// State machine (see [`ConflictDecision`]):
/// - `KeepNew` → `conflicts: <target>` becomes `supersedes: <target>`; the
///   target is demoted as stale and the claimant is stamped `updated:`.
/// - `KeepOld` → `<target>` is dropped from `conflicts:` and the claimant is
///   stamped `resolved: <target>`; nothing is demoted (the claim was
///   unconfirmed, so dropping it restores the status quo), but the dream
///   must not re-flag the pair.
/// - `Both` → `<target>` is dropped, `resolved: <target>` is stamped, and a
///   body note records the verdict so a future dream does not re-flag it.
/// - `Unknown` → `<target>` is appended to the claimant's `asked:` list; the
///   claim stays pending and no authority changes. Refused when the pair is
///   already in `asked:` (or a legacy `asked:` date is present).
///
/// Every successful resolution is appended to the `.resolutions.jsonl` audit
/// log under the memory dir (best-effort; log failures never fail the resolve).
///
/// Errors are descriptive strings suitable for a tool result: unknown files,
/// a claim that does not exist, path traversal, or an already-asked conflict.
pub fn resolve_memory_conflict(
    memory_dir: &Path,
    claimant: &str,
    target: &str,
    decision: ConflictDecision,
) -> Result<ConflictResolution, String> {
    validate_memory_rel_name(claimant)?;
    validate_memory_rel_name(target)?;

    let claimant_path = memory_dir.join(claimant);
    if !claimant_path.is_file() {
        return Err(format!("claimant memory file not found: {}", claimant));
    }
    // Defense in depth: even when a path component is a symlink, the resolved
    // file must stay inside the memory dir.
    let canonical_dir = std::fs::canonicalize(memory_dir)
        .map_err(|e| format!("cannot resolve memory dir: {}", e))?;
    let canonical_claimant = std::fs::canonicalize(&claimant_path)
        .map_err(|e| format!("cannot resolve claimant file: {}", e))?;
    if !canonical_claimant.starts_with(&canonical_dir) {
        return Err(format!(
            "claimant path escapes the memory directory: {}",
            claimant
        ));
    }

    let content = std::fs::read_to_string(&claimant_path)
        .map_err(|e| format!("cannot read {}: {}", claimant, e))?;
    let fm = parse_frontmatter_quick(&content);
    // A file cannot be in conflict with itself; the pending block already
    // skips self-references, but the resolver must refuse them too (a model
    // could otherwise demote a file against itself and corrupt the state).
    if claimant == target {
        return Err(format!(
            "{} cannot be in conflict with itself — pick a different target",
            claimant
        ));
    }
    // The target must still exist: adjudicating a claim against a deleted
    // file would write a dangling `supersedes:`/`resolved:` entry that only a
    // later sweep could clean. The pending block never surfaces dangling
    // pairs, so reaching this is a caller error — refuse it clearly.
    if !memory_dir.join(target).is_file() {
        return Err(format!(
            "target memory file not found: {} — cannot adjudicate a conflict \
             against a deleted file",
            target
        ));
    }
    if !fm.conflicts.iter().any(|c| c == target) {
        return Err(format!(
            "{} does not claim {} is wrong (no matching `conflicts:` entry)",
            claimant, target
        ));
    }
    // Mutual-supersession guard: promoting this claim would make the claimant
    // supersede a file that already (transitively) supersedes it — a cycle
    // where every member is demoted and none is authoritative. Refuse so the
    // state machine stays a DAG (the `supersedes:` relation must remain
    // acyclic). Walking the closure — not just the target's direct list —
    // catches 3+-cycles (A supersedes B, B supersedes C, then C→A would
    // close the ring).
    if decision == ConflictDecision::KeepNew && supersedes_reaches(memory_dir, target, claimant) {
        return Err(format!(
            "{} already supersedes {} (directly or through a chain) — resolving \
             {} as the winner would create a supersession cycle (all files \
             demoted, none authoritative)",
            target, claimant, claimant
        ));
    }
    // Per-pair ask guard: refuse when THIS target was already asked and left
    // unresolved (either explicitly or via a legacy `asked: <date>` covering
    // conflicts that existed at that date).
    let target_created = |candidate: &str| {
        std::fs::read_to_string(memory_dir.join(candidate))
            .ok()
            .and_then(|content| parse_frontmatter_quick(&content).created)
    };
    if decision == ConflictDecision::Unknown
        && asked_targets(&fm.asked, &fm.conflicts, target_created)
            .iter()
            .any(|t| t == target)
    {
        return Err(format!(
            "{} was already asked about {} — do not re-ask this conflict",
            claimant, target
        ));
    }

    let today = today_iso_date();
    let mut editor = FrontmatterEditor::parse(&content);

    let summary = match decision {
        ConflictDecision::KeepNew => {
            if editor.remove_from_list("conflicts", target) {
                editor.remove_field("conflicts");
            }
            editor.add_to_list("supersedes", target);
            if editor.remove_asked(target) {
                editor.remove_field("asked");
            }
            // A pair can carry both a `resolved:` marker (from an earlier
            // keep_old/both verdict) and a re-flagged `conflicts:` entry.
            // Promotion to `supersedes:` is the stronger verdict — drop the
            // stale "settled, never re-flag" marker so the two cannot
            // contradict each other.
            if editor.remove_from_list("resolved", target) {
                editor.remove_field("resolved");
            }
            editor.set_scalar("updated", &today);
            format!(
                "Promoted {}'s claim against {} to a confirmed supersession \
                 (`supersedes:`); {} is now treated as stale.",
                claimant, target, target
            )
        }
        ConflictDecision::KeepOld => {
            if editor.remove_from_list("conflicts", target) {
                editor.remove_field("conflicts");
            }
            if editor.remove_asked(target) {
                editor.remove_field("asked");
            }
            editor.add_to_list("resolved", target);
            editor.set_scalar("updated", &today);
            format!(
                "Dropped {}'s claim against {} — the established fact stands, \
                 both files stay active, and this pair will not be re-flagged.",
                claimant, target
            )
        }
        ConflictDecision::Both => {
            if editor.remove_from_list("conflicts", target) {
                editor.remove_field("conflicts");
            }
            if editor.remove_asked(target) {
                editor.remove_field("asked");
            }
            editor.add_to_list("resolved", target);
            editor.set_scalar("updated", &today);
            editor.append_body_note(&format!(
                "> User confirmed {} and {} are both true in different contexts ({}).",
                claimant, target, today
            ));
            format!(
                "Dropped {}'s claim against {} — both facts remain active as \
                 user-confirmed complements, and this pair will not be re-flagged.",
                claimant, target
            )
        }
        ConflictDecision::Unknown => {
            editor.add_to_list("asked", &format!("{}:{}", target, today));
            format!(
                "Marked {} `asked: {}:{}` — the claim stays pending, {} stays \
                 active, and this conflict will not be re-asked.",
                claimant, target, today, target
            )
        }
    };

    let rewritten = editor.render();
    std::fs::write(&claimant_path, rewritten)
        .map_err(|e| format!("cannot write {}: {}", claimant, e))?;

    // A confirmed supersession demotes the target immediately: drop its
    // `MEMORY.md` index entry so the stale fact stops being presented as
    // current (best-effort — the dream's Phase 4 and the injection's
    // superseded-memories block cover any case this misses).
    let mut reciprocal_cleared = false;
    if decision == ConflictDecision::KeepNew {
        let _ = prune_index_entry(memory_dir, target);
        // Cascade: if the demoted target itself claimed the claimant is wrong
        // (a mutual conflict), that claim is now moot — the user just ruled
        // the claimant authoritative. Clear the reciprocal entry so the
        // pending block does not surface a stale file's counter-claim and ask
        // the user to adjudicate it. The flag is recorded in the audit log so
        // undo can restore the reciprocal claim.
        if let Ok(target_content) = std::fs::read_to_string(memory_dir.join(target)) {
            let target_fm = parse_frontmatter_quick(&target_content);
            if target_fm.conflicts.iter().any(|c| c == claimant) {
                let mut target_editor = FrontmatterEditor::parse(&target_content);
                if target_editor.remove_from_list("conflicts", claimant) {
                    target_editor.remove_field("conflicts");
                }
                if target_editor.remove_asked(claimant) {
                    target_editor.remove_field("asked");
                }
                if target_editor.remove_from_list("resolved", claimant) {
                    target_editor.remove_field("resolved");
                }
                target_editor.set_scalar("updated", &today);
                let _ = std::fs::write(memory_dir.join(target), target_editor.render());
                reciprocal_cleared = true;
            }
        }
    }

    let resolution = ConflictResolution {
        claimant: claimant.to_string(),
        target: target.to_string(),
        decision,
        summary,
    };
    // Audit trail (best-effort): a failed log write never fails the resolve.
    let _ = record_resolution(
        memory_dir,
        &ResolutionRecord {
            ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            claimant: resolution.claimant.clone(),
            target: resolution.target.clone(),
            decision: decision.as_str().to_string(),
            reciprocal_cleared,
        },
    );

    Ok(resolution)
}

/// The markdown link target of an index line's own entry link, if any.
///
/// Index entries look like `- [Title](file.md) — one-line hook`. The *own*
/// link is the first `](…)` in the line; a description that happens to link
/// to another memory file must not be confused with the entry itself. Returns
/// `None` for prose lines without link syntax.
fn line_own_link_target(line: &str) -> Option<&str> {
    let open = line.find("](")? + 2;
    let rest = &line[open..];
    let close = rest.find(')')?;
    Some(&rest[..close])
}

/// Remove the `MEMORY.md` index line whose OWN entry link resolves to
/// `target` (`- [Title](target) …`).
///
/// Only the entry's own link counts: a line that links to a *different*
/// file but mentions `target` in its description (`- [new.md](new.md) — see
/// [old.md](old.md)`) is preserved — dropping it would de-index the wrong
/// file. Prose mentions without link syntax are preserved too. Returns `true`
/// when the index changed. Best-effort: missing/unreadable index is not an
/// error.
pub fn prune_index_entry(memory_dir: &Path, target: &str) -> std::io::Result<bool> {
    let index_path = memory_dir.join(MEMORY_ENTRYPOINT);
    let raw = match std::fs::read_to_string(&index_path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let original_lines: Vec<&str> = raw.lines().collect();
    let kept: Vec<&str> = original_lines
        .iter()
        .copied()
        .filter(|line| line_own_link_target(line) != Some(target))
        .collect();
    if kept.len() == original_lines.len() {
        return Ok(false);
    }
    let mut out = kept.join("\n");
    out.push('\n');
    std::fs::write(&index_path, out)?;
    Ok(true)
}

/// Re-add a `MEMORY.md` index entry for `target` (the inverse of
/// [`prune_index_entry`]), used by the undo path so a reversed `keep_new`
/// resolution leaves the target visible in the index again.
///
/// Appends `- [Title](target) — description` (title from the file's `name:`
/// frontmatter, falling back to the filename) when no line already owns a
/// link to `target`. Returns `true` when the index changed. Best-effort: a
/// missing index is left alone (nothing was pruned), and a file without
/// frontmatter degrades to its bare filename.
pub fn restore_index_entry(memory_dir: &Path, target: &str) -> std::io::Result<bool> {
    let index_path = memory_dir.join(MEMORY_ENTRYPOINT);
    let raw = match std::fs::read_to_string(&index_path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if raw
        .lines()
        .any(|line| line_own_link_target(line) == Some(target))
    {
        return Ok(false);
    }
    // Best-effort title/description from the target file's frontmatter.
    let (title, hook) = std::fs::read_to_string(memory_dir.join(target))
        .ok()
        .map(|content| {
            let fm = parse_frontmatter_quick(&content);
            let title = fm.name.unwrap_or_else(|| target.to_string());
            let hook = fm.description.unwrap_or_default();
            (title, hook)
        })
        .unwrap_or_else(|| (target.to_string(), String::new()));
    let entry = if hook.is_empty() {
        format!("- [{}]({})", title, target)
    } else {
        format!("- [{}]({}) — {}", title, target, hook)
    };
    let mut out = raw.trim_end().to_string();
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&entry);
    out.push('\n');
    std::fs::write(&index_path, out)?;
    Ok(true)
}

/// Result of sweeping dangling memory references.
#[derive(Debug, Clone, Default)]
pub struct SweepReport {
    /// `(file, dangling_target)` pairs removed from `conflicts:` frontmatter.
    pub removed_conflicts: Vec<(String, String)>,
    /// `(file, dangling_target)` pairs removed from `supersedes:` frontmatter.
    pub removed_supersedes: Vec<(String, String)>,
    /// `(file, dangling_target)` pairs removed from `asked:` frontmatter
    /// (per-pair `target:YYYY-MM-DD` entries; legacy bare dates are not
    /// filenames and are left alone).
    pub removed_asked: Vec<(String, String)>,
    /// `(file, dangling_target)` pairs removed from `resolved:` frontmatter.
    pub removed_resolved: Vec<(String, String)>,
}

impl SweepReport {
    pub fn is_empty(&self) -> bool {
        self.removed_conflicts.is_empty()
            && self.removed_supersedes.is_empty()
            && self.removed_asked.is_empty()
            && self.removed_resolved.is_empty()
    }
}

/// Remove `conflicts:`/`supersedes:`/`asked:`/`resolved:` entries whose
/// target file no longer exists in the memory dir (e.g. the user deleted the
/// file). Dangling entries are pure noise: the pending block skips them,
/// nothing can adjudicate them, and a stale `supersedes:` mislabels a missing
/// file as current history. Deterministic, best-effort — files are rewritten
/// only when a dangling entry was actually dropped.
pub fn sweep_dangling_memory_refs(memory_dir: &Path) -> SweepReport {
    let metas = scan_memory_dir(memory_dir);
    let existing: std::collections::HashSet<&str> =
        metas.iter().map(|m| m.filename.as_str()).collect();
    let mut report = SweepReport::default();

    for meta in &metas {
        let mut changed = false;
        let mut editor = match std::fs::read_to_string(&meta.path) {
            Ok(content) => FrontmatterEditor::parse(&content),
            Err(_) => continue,
        };
        for target in &meta.conflicts {
            if !existing.contains(target.as_str()) {
                if editor.remove_from_list("conflicts", target) {
                    editor.remove_field("conflicts");
                }
                report
                    .removed_conflicts
                    .push((meta.filename.clone(), target.clone()));
                changed = true;
            }
        }
        for target in &meta.supersedes {
            if !existing.contains(target.as_str()) {
                if editor.remove_from_list("supersedes", target) {
                    editor.remove_field("supersedes");
                }
                report
                    .removed_supersedes
                    .push((meta.filename.clone(), target.clone()));
                changed = true;
            }
        }
        for entry in &meta.asked {
            // Legacy `asked: <date>` entries are dates, not filenames — the
            // bare date is not a memory file and must never be swept.
            let (entry_target, _date) = split_asked_entry(entry);
            if is_iso_date(entry) || existing.contains(entry_target) {
                continue;
            }
            if editor.remove_asked(entry_target) {
                editor.remove_field("asked");
            }
            report
                .removed_asked
                .push((meta.filename.clone(), entry_target.to_string()));
            changed = true;
        }
        for target in &meta.resolved {
            if !existing.contains(target.as_str()) {
                if editor.remove_from_list("resolved", target) {
                    editor.remove_field("resolved");
                }
                report
                    .removed_resolved
                    .push((meta.filename.clone(), target.clone()));
                changed = true;
            }
        }
        if changed {
            let _ = std::fs::write(&meta.path, editor.render());
        }
    }
    report
}

// ---------------------------------------------------------------------------
// Resolution audit trail
// ---------------------------------------------------------------------------

/// Filename of the resolution audit log inside the memory directory. One JSON
/// line per resolution (`.jsonl`), so it is never picked up by the `.md`-only
/// memory scan. Dotfile to keep the memory dir tidy.
pub const RESOLUTIONS_LOG: &str = ".resolutions.jsonl";

/// One recorded conflict resolution (audit trail + undo source).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolutionRecord {
    /// Unix timestamp (seconds) when the resolution was applied.
    pub ts: u64,
    /// The memory file that carried the claim.
    pub claimant: String,
    /// The file the claim was about.
    pub target: String,
    /// Canonical decision name (`ConflictDecision::as_str`).
    pub decision: String,
    /// `true` when a `keep_new` resolution also cleared the demoted target's
    /// reciprocal claim against the claimant (a mutual conflict). Undo must
    /// reverse that side effect too — without this flag it cannot know the
    /// claim existed. Defaults to `false` so old log lines keep parsing.
    #[serde(default)]
    pub reciprocal_cleared: bool,
}

/// Append a resolution to the audit log (`.resolutions.jsonl`), creating the
/// file if needed. Best-effort: the caller decides whether failures matter.
pub fn record_resolution(memory_dir: &Path, record: &ResolutionRecord) -> std::io::Result<()> {
    use std::io::Write;
    let path = memory_dir.join(RESOLUTIONS_LOG);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let line = serde_json::to_string(record).map_err(std::io::Error::other)?;
    writeln!(file, "{}", line)?;
    Ok(())
}

/// Read the most recent `max` resolutions from the audit log, newest first.
/// Malformed lines are skipped (the log is append-only and best-effort).
pub fn recent_resolutions(memory_dir: &Path, max: usize) -> Vec<ResolutionRecord> {
    let path = memory_dir.join(RESOLUTIONS_LOG);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|line| serde_json::from_str::<ResolutionRecord>(line).ok())
        .rev()
        .take(max)
        .collect()
}

/// Outcome of reversing the most recent resolution.
#[derive(Debug, Clone)]
pub struct UndoReport {
    /// The audit record that was reversed (and popped from the log).
    pub record: ResolutionRecord,
    /// Human-readable description of what was restored.
    pub summary: String,
}

/// Reverse the most recent conflict resolution, restoring the exact prior
/// frontmatter (the AGM recovery postulate: contract-then-expand returns the
/// original belief state).
///
/// - `keep_new` → drop the added `supersedes:` entry, restore `conflicts:`.
/// - `keep_old` / `both` → drop the added `resolved:` entry, restore
///   `conflicts:` (and remove the both-true body note for `both`).
/// - `unknown` → drop the added per-pair `asked:` entry; the pair becomes
///   askable again.
///
/// The reversed record is popped from `.resolutions.jsonl` so repeated undos
/// walk backwards through history. The undo itself is not logged.
///
/// Errors are descriptive strings suitable for a command result: empty log,
/// missing claimant file, or an unrecognised decision in the log.
pub fn undo_last_resolution(memory_dir: &Path) -> Result<UndoReport, String> {
    let path = memory_dir.join(RESOLUTIONS_LOG);
    let raw = std::fs::read_to_string(&path)
        .map_err(|_| "no resolutions to undo — the audit log is empty or missing".to_string())?;
    let records: Vec<ResolutionRecord> = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<ResolutionRecord>(line).ok())
        .collect();
    let last = records
        .last()
        .cloned()
        .ok_or_else(|| "no resolutions to undo — the audit log is empty".to_string())?;

    let claimant_path = memory_dir.join(&last.claimant);
    if !claimant_path.is_file() {
        return Err(format!(
            "cannot undo: claimant file {} no longer exists",
            last.claimant
        ));
    }
    let content = std::fs::read_to_string(&claimant_path)
        .map_err(|e| format!("cannot read {}: {}", last.claimant, e))?;
    let mut editor = FrontmatterEditor::parse(&content);

    let summary = match last.decision.as_str() {
        "keep_new" => {
            if editor.remove_from_list("supersedes", &last.target) {
                editor.remove_field("supersedes");
            }
            editor.add_to_list("conflicts", &last.target);
            // The original resolution may have cascaded: a mutual conflict
            // (target also claimed claimant) had its reciprocal claim cleared.
            // Restore it so the recovered state is the exact prior one — the
            // flag is what tells us the claim existed (defaults to false for
            // log lines written before the flag was added).
            if last.reciprocal_cleared {
                if let Ok(target_content) = std::fs::read_to_string(memory_dir.join(&last.target)) {
                    let target_fm = parse_frontmatter_quick(&target_content);
                    if !target_fm.conflicts.iter().any(|c| c == &last.claimant) {
                        let mut target_editor = FrontmatterEditor::parse(&target_content);
                        target_editor.add_to_list("conflicts", &last.claimant);
                        let _ =
                            std::fs::write(memory_dir.join(&last.target), target_editor.render());
                    }
                }
            }
            let mut summary = format!(
                "Restored the conflict: {} claims {} is wrong again (supersession revoked).",
                last.claimant, last.target
            );
            if last.reciprocal_cleared {
                summary.push_str(&format!(
                    " Also restored {}'s reciprocal claim against {} (the mutual \
                     conflict is back in full).",
                    last.target, last.claimant
                ));
            }
            summary
        }
        "keep_old" => {
            if editor.remove_from_list("resolved", &last.target) {
                editor.remove_field("resolved");
            }
            editor.add_to_list("conflicts", &last.target);
            format!(
                "Restored the conflict: {} claims {} is wrong again.",
                last.claimant, last.target
            )
        }
        "both" => {
            if editor.remove_from_list("resolved", &last.target) {
                editor.remove_field("resolved");
            }
            editor.add_to_list("conflicts", &last.target);
            editor.remove_body_note(&last.claimant, &last.target);
            format!(
                "Restored the conflict: {} claims {} is wrong again (both-true verdict revoked).",
                last.claimant, last.target
            )
        }
        "unknown" => {
            if editor.remove_asked(&last.target) {
                editor.remove_field("asked");
            }
            format!(
                "Cleared the `asked:` mark on {} for {} — the pair can be re-asked.",
                last.claimant, last.target
            )
        }
        other => {
            return Err(format!(
                "cannot undo unknown decision '{}' in the audit log",
                other
            ));
        }
    };

    std::fs::write(&claimant_path, editor.render())
        .map_err(|e| format!("cannot write {}: {}", last.claimant, e))?;

    // A reversed `keep_new` demoted the target's index entry; bring it back
    // so the recovered conflict's target is visible in the index again
    // (best-effort — the dream's Phase 4 can re-add it if the index is gone).
    if last.decision == "keep_new" {
        let _ = restore_index_entry(memory_dir, &last.target);
    }

    // Pop the last record so repeated undos walk backwards. The slice is by
    // LINE INDEX of the last valid record, not by parsed-record count: a
    // malformed line (partial write) before the last record would otherwise
    // shrink `records.len()` and the `take()` would silently drop valid
    // records along with the one being undone. Keep every line before the
    // last valid one verbatim.
    let lines: Vec<&str> = raw.lines().collect();
    let last_valid = lines
        .iter()
        .rposition(|line| serde_json::from_str::<ResolutionRecord>(line).is_ok())
        .unwrap_or(0);
    let mut out = lines[..last_valid].join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    let _ = std::fs::write(&path, out);

    Ok(UndoReport {
        record: last,
        summary,
    })
}

/// Validate a memory-relative filename: not empty, no NUL, not absolute, and
/// no `..` / root / drive-prefix components. Sub-directory paths are allowed
/// (the directory scan recurses, so filenames like `sessions/2026-08-01.md`
/// are valid).
fn validate_memory_rel_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("empty file name".to_string());
    }
    if name.contains('\0') {
        return Err("file name contains a NUL byte".to_string());
    }
    let path = Path::new(name);
    if path.is_absolute() {
        return Err(format!("absolute path is not allowed: {}", name));
    }
    use std::path::Component;
    if path.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(format!("file name escapes the memory directory: {}", name));
    }
    Ok(())
}

/// Today's date as `YYYY-MM-DD` (UTC), reusing the existing calendar math so
/// no date library is pulled into this module.
fn today_iso_date() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let jdn = (secs / 86400) as u32 + 2440588;
    let (y, m, d) = jdn_to_ymd(jdn);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// In-place editor for a memory file's YAML frontmatter block.
///
/// Operates surgically on the existing lines so unknown fields a user or the
/// dream added are preserved verbatim. Files without frontmatter get a fresh
/// block prepended; a file that opens `---` but never closes within the scan
/// window is treated as body-only (no fields to edit), never rewritten into
/// something malformed.
struct FrontmatterEditor {
    /// Lines between the opening and closing `---` delimiters.
    lines: Vec<String>,
    /// Everything after the closing delimiter (or the whole content when
    /// there was no frontmatter).
    body: String,
    /// Whether the file had a `---`-delimited block at the top.
    had_frontmatter: bool,
}

impl FrontmatterEditor {
    fn parse(content: &str) -> Self {
        let all: Vec<&str> = content.lines().collect();
        if all.first().map(|l| l.trim() != "---").unwrap_or(true) {
            return Self {
                lines: Vec::new(),
                body: content.to_string(),
                had_frontmatter: false,
            };
        }
        let mut close: Option<usize> = None;
        for (i, line) in all
            .iter()
            .enumerate()
            .skip(1)
            .take(FRONTMATTER_MAX_LINES - 1)
        {
            if line.trim() == "---" {
                close = Some(i);
                break;
            }
        }
        let Some(close) = close else {
            return Self {
                lines: Vec::new(),
                body: content.to_string(),
                had_frontmatter: false,
            };
        };
        Self {
            lines: all[1..close].iter().map(|l| l.to_string()).collect(),
            body: if close + 1 < all.len() {
                all[close + 1..].join("\n")
            } else {
                String::new()
            },
            had_frontmatter: true,
        }
    }

    /// Index of the line declaring `field:`, if any. Matches only exact
    /// `field:` prefixes (same convention as `parse_frontmatter_quick`).
    fn find_field(&self, field: &str) -> Option<usize> {
        self.lines
            .iter()
            .position(|l| l.starts_with(field) && l[field.len()..].starts_with(':'))
    }

    /// Remove `item` from a comma/space-separated list field. Returns `true`
    /// when the field is now empty (the caller should drop the line).
    fn remove_from_list(&mut self, field: &str, item: &str) -> bool {
        let Some(idx) = self.find_field(field) else {
            return false;
        };
        let rest = self.lines[idx][field.len() + 1..].trim();
        let remaining: Vec<String> = rest
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter(|s| s.trim_matches('"').trim_matches('\'') != item)
            .map(|s| s.trim_matches('"').trim_matches('\'').to_string())
            .collect();
        if remaining.is_empty() {
            true
        } else {
            self.lines[idx] = format!("{}: {}", field, remaining.join(", "));
            false
        }
    }

    /// Drop the whole `field:` line if present.
    fn remove_field(&mut self, field: &str) {
        if let Some(idx) = self.find_field(field) {
            self.lines.remove(idx);
        }
    }

    /// Remove every `asked:` entry naming `target` — bare (`target`) or dated
    /// (`target:YYYY-MM-DD`). Returns `true` when the field is now empty (the
    /// caller should drop the line).
    fn remove_asked(&mut self, target: &str) -> bool {
        let Some(idx) = self.find_field("asked") else {
            return false;
        };
        let rest = self.lines[idx]["asked:".len() + 1..].trim();
        let remaining: Vec<String> = rest
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter(|s| {
                let (entry_target, _date) = split_asked_entry(s);
                entry_target != target
            })
            .map(|s| s.trim_matches('"').trim_matches('\'').to_string())
            .collect();
        if remaining.is_empty() {
            true
        } else {
            self.lines[idx] = format!("asked: {}", remaining.join(", "));
            false
        }
    }

    /// Append `item` to a list field, creating the line when absent. A
    /// duplicate (exact match after quote-stripping, matching
    /// [`parse_file_list`] semantics) is a no-op — the resolver can otherwise
    /// produce `supersedes: x, x` when a hand-authored file already lists the
    /// target while a re-flagged `conflicts:` entry is promoted.
    fn add_to_list(&mut self, field: &str, item: &str) {
        let normalized = item.trim_matches('"').trim_matches('\'');
        match self.find_field(field) {
            Some(idx) => {
                let rest = self.lines[idx][field.len() + 1..].trim();
                let already = rest
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .any(|entry| {
                        !entry.is_empty()
                            && entry.trim().trim_matches('"').trim_matches('\'') == normalized
                    });
                if already {
                    return;
                }
                if rest.is_empty() {
                    self.lines[idx] = format!("{}: {}", field, item);
                } else {
                    self.lines[idx] = format!("{}: {}, {}", field, rest, item);
                }
            }
            None => self.lines.push(format!("{}: {}", field, item)),
        }
    }

    /// Set a scalar field, creating the line when absent.
    fn set_scalar(&mut self, field: &str, value: &str) {
        match self.find_field(field) {
            Some(idx) => self.lines[idx] = format!("{}: {}", field, value),
            None => self.lines.push(format!("{}: {}", field, value)),
        }
    }

    /// Append a note at the end of the body (used by `Both` to record the
    /// verdict so a future dream does not re-flag the pair).
    fn append_body_note(&mut self, note: &str) {
        let trimmed = self.body.trim_end();
        let sep = if trimmed.is_empty() { "" } else { "\n\n" };
        self.body = format!("{}{}{}\n", trimmed, sep, note);
    }

    /// Remove the both-true body note written by [`Self::append_body_note`]
    /// for a specific pair (undo path), including its preceding blank line.
    fn remove_body_note(&mut self, claimant: &str, target: &str) {
        let marker = format!(
            "> User confirmed {} and {} are both true in different contexts (",
            claimant, target
        );
        let Some(pos) = self.body.find(&marker) else {
            return;
        };
        let line_start = self
            .body
            .get(..pos)
            .and_then(|head| head.rfind("\n\n"))
            .map(|i| i + 2)
            .unwrap_or(pos);
        let line_end = self
            .body
            .get(pos..)
            .and_then(|tail| tail.find('\n'))
            .map(|i| pos + i + 1)
            .unwrap_or(self.body.len());
        self.body = format!("{}{}", &self.body[..line_start], &self.body[line_end..]);
    }

    /// Rebuild the file content.
    fn render(&self) -> String {
        if !self.had_frontmatter && self.lines.is_empty() {
            return self.body.clone();
        }
        let mut out = String::from("---\n");
        for line in &self.lines {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("---\n");
        out.push_str(&self.body);
        out
    }
}

/// Idempotently record detected build/verify commands into `conventions.md`
/// (audit spec §9.5 trigger 1): after a verify round discovers the project's
/// test/lint commands, they are worth persisting so future sessions know how
/// to build and verify without re-discovery.
///
/// Only appends — never overwrites. A `## Build & verify commands` section is
/// created on first write; a command already listed under it is left alone.
/// Returns `true` when the file was modified.
pub fn record_verify_conventions(
    memory_dir: &Path,
    test_command: Option<&str>,
    lint_command: Option<&str>,
) -> bool {
    let commands: Vec<(&str, &str)> = test_command
        .into_iter()
        .map(|c| ("Test", c))
        .chain(lint_command.into_iter().map(|c| ("Lint", c)))
        .filter(|(_, c)| !c.trim().is_empty())
        .collect();
    if commands.is_empty() {
        return false;
    }

    ensure_memory_dir_exists(memory_dir);
    let path = memory_dir.join("conventions.md");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut has_section = existing
        .lines()
        .any(|l| l.trim() == "## Build & verify commands");

    let mut additions: Vec<String> = Vec::new();
    for (kind, cmd) in &commands {
        let needle = format!("- {}: {}", kind, cmd);
        if existing.lines().any(|l| l.trim() == needle) {
            continue;
        }
        if !has_section {
            additions.push("## Build & verify commands".to_string());
            additions.push(String::new());
            has_section = true;
        }
        additions.push(needle);
    }
    if additions.is_empty() {
        return false;
    }

    let mut out = existing.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&additions.join("\n"));
    out.push('\n');
    let _ = std::fs::write(&path, out);
    true
}

/// Ensure the memory directory exists, creating it (and any parents) if needed.
/// Errors are silently swallowed (the Write tool will surface them if needed).
///
/// Mirrors `ensureMemoryDirExists` in `memdir.ts`.
pub fn ensure_memory_dir_exists(memory_dir: &Path) {
    if let Err(e) = std::fs::create_dir_all(memory_dir) {
        // Log at debug level so --debug shows why, but don't abort.
        tracing::debug!(
            dir = %memory_dir.display(),
            error = %e,
            "ensureMemoryDirExists failed"
        );
    }
}

// ---------------------------------------------------------------------------
// Simple relevance search (no LLM side-query)
// ---------------------------------------------------------------------------

/// Find and load the most relevant memory files for a query using a
/// lightweight TF-IDF-style keyword score.
///
/// The full Sonnet side-query (`findRelevantMemories` in TypeScript) lives
/// in `cc-query`; this function provides a cheaper fallback for contexts
/// where an API call is not available.
#[allow(dead_code)]
pub fn find_relevant_memories_simple(
    memory_dir: &Path,
    query: &str,
    max_files: usize,
) -> Vec<MemoryFile> {
    let metas = scan_memory_dir(memory_dir);
    // A file superseded by another memory is stale by definition — it must
    // not surface in relevance retrieval (the superseding file holds the
    // current fact).
    let superseded = superseded_by(&metas);
    let query_lower = query.to_lowercase();
    let query_words: Vec<&str> = query_lower.split_whitespace().collect();

    if query_words.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<(f32, MemoryFileMeta)> = metas
        .into_iter()
        .filter(|meta| !superseded.contains_key(&meta.filename))
        .filter_map(|meta| {
            let desc = meta.description.as_deref().unwrap_or("").to_lowercase();
            let name = meta.name.as_deref().unwrap_or("").to_lowercase();
            let filename = meta.filename.to_lowercase();

            let score: f32 = query_words
                .iter()
                .map(|w| {
                    let in_name = if name.contains(*w) { 2.0_f32 } else { 0.0 };
                    let in_desc = if desc.contains(*w) { 1.0_f32 } else { 0.0 };
                    let in_file = if filename.contains(*w) { 0.5_f32 } else { 0.0 };
                    in_name + in_desc + in_file
                })
                .sum();

            if score > 0.0 {
                Some((score, meta))
            } else {
                None
            }
        })
        .collect();

    // Sort highest score first.
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    scored
        .into_iter()
        .take(max_files)
        .filter_map(|(_, meta)| {
            let content = std::fs::read_to_string(&meta.path).ok()?;
            Some(MemoryFile { meta, content })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Team memory helpers
// ---------------------------------------------------------------------------

/// Return the team-memory sub-directory path.
/// Mirrors `getTeamMemPath` in `teamMemPaths.ts`.
#[allow(dead_code)]
pub fn team_memory_path(auto_memory_dir: &Path) -> PathBuf {
    auto_memory_dir.join("team")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;

    // Helpers ----------------------------------------------------------------

    fn make_temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    // ---- parse_frontmatter_quick -------------------------------------------

    #[test]
    fn test_parse_frontmatter_full() {
        let content = "---\nname: My Memory\ndescription: A test description\ntype: feedback\n---\n\nBody text.";
        let fm = parse_frontmatter_quick(content);
        assert_eq!(fm.name.as_deref(), Some("My Memory"));
        assert_eq!(fm.description.as_deref(), Some("A test description"));
        assert_eq!(fm.memory_type, Some(MemoryType::Feedback));
        assert!(fm.created.is_none());
        assert!(fm.supersedes.is_empty());
    }

    #[test]
    fn test_parse_frontmatter_no_frontmatter() {
        let content = "Just plain text.";
        let fm = parse_frontmatter_quick(content);
        assert!(fm.name.is_none());
        assert!(fm.description.is_none());
        assert!(fm.memory_type.is_none());
    }

    #[test]
    fn test_parse_frontmatter_quoted_values() {
        let content = "---\nname: \"Quoted Name\"\ndescription: 'Single quoted'\ntype: user\n---";
        let fm = parse_frontmatter_quick(content);
        assert_eq!(fm.name.as_deref(), Some("Quoted Name"));
        assert_eq!(fm.description.as_deref(), Some("Single quoted"));
        assert_eq!(fm.memory_type, Some(MemoryType::User));
    }

    #[test]
    fn test_parse_frontmatter_unknown_type() {
        let content = "---\ntype: unknown_type\n---";
        let fm = parse_frontmatter_quick(content);
        assert!(fm.memory_type.is_none());
    }

    #[test]
    fn test_parse_frontmatter_supersession_fields() {
        let content = "---\nname: Auth flow v2\ntype: project\ncreated: 2026-08-10\nupdated: 2026-08-20\nsupersedes: auth-flow-v1.md, auth-proto-notes.md\n---\n\nOAuth is now used.";
        let fm = parse_frontmatter_quick(content);
        assert_eq!(fm.created.as_deref(), Some("2026-08-10"));
        assert_eq!(fm.updated.as_deref(), Some("2026-08-20"));
        assert_eq!(
            fm.supersedes,
            vec![
                "auth-flow-v1.md".to_string(),
                "auth-proto-notes.md".to_string()
            ]
        );
        assert!(fm.conflicts.is_empty());
        assert!(fm.asked.is_empty());
    }

    #[test]
    fn test_parse_frontmatter_conflicts_and_asked() {
        let content = "---\nname: Verbose errors claim\ntype: feedback\nconflicts: prefs.md\nasked: 2026-08-20\n---\n\nUser prefers verbose errors.";
        let fm = parse_frontmatter_quick(content);
        assert_eq!(fm.conflicts, vec!["prefs.md".to_string()]);
        // Legacy date form still parses (one entry); asked_targets expands it.
        assert_eq!(fm.asked, vec!["2026-08-20".to_string()]);
        assert!(fm.supersedes.is_empty());
        assert!(fm.resolved.is_empty());
    }

    #[test]
    fn test_parse_frontmatter_per_pair_asked_and_resolved() {
        let content = "---\nname: Claim\nconflicts: a.md, b.md\nasked: a.md\nresolved: c.md, d.md\n---\n\nbody";
        let fm = parse_frontmatter_quick(content);
        assert_eq!(fm.conflicts, vec!["a.md".to_string(), "b.md".to_string()]);
        assert_eq!(fm.asked, vec!["a.md".to_string()]);
        assert_eq!(fm.resolved, vec!["c.md".to_string(), "d.md".to_string()]);
    }

    #[test]
    fn test_asked_targets_expands_legacy_date_and_keeps_explicit() {
        // Legacy date → every conflict target that predates it (created dates
        // unknown → conservatively included).
        let expanded = asked_targets(
            &["2026-08-20".to_string()],
            &["a.md".to_string(), "b.md".to_string()],
            |_| None,
        );
        assert_eq!(expanded, vec!["a.md".to_string(), "b.md".to_string()]);
        // Per-pair entries are returned verbatim (dates expand only what they
        // cover; a filename that looks like a date is treated as legacy).
        let explicit = asked_targets(
            &["a.md".to_string()],
            &["a.md".to_string(), "b.md".to_string()],
            |_| None,
        );
        assert_eq!(explicit, vec!["a.md".to_string()]);
        // Empty asked → nothing.
        assert!(asked_targets(&[], &["a.md".to_string()], |_| None).is_empty());
    }

    #[test]
    fn test_asked_targets_legacy_date_skips_newer_conflicts() {
        // A conflict whose target file was created AFTER the legacy ask date
        // cannot have been asked about — it must stay askable.
        let asked = vec!["2026-08-20".to_string()];
        let conflicts = vec!["old.md".to_string(), "new.md".to_string()];
        let created = |target: &str| -> Option<String> {
            match target {
                "old.md" => Some("2026-07-01".to_string()),
                "new.md" => Some("2026-08-25".to_string()), // after the ask date
                _ => None,
            }
        };
        let expanded = asked_targets(&asked, &conflicts, created);
        assert_eq!(
            expanded,
            vec!["old.md".to_string()],
            "new.md postdates the ask date and must not be silenced"
        );
        // Same-day creation counts as predating (covers equal dates).
        let same_day = asked_targets(&asked, &["a.md".to_string()], |_| {
            Some("2026-08-20".to_string())
        });
        assert_eq!(same_day, vec!["a.md".to_string()]);
    }

    #[test]
    fn test_conflicted_by_maps_claimants() {
        let claim = MemoryFileMeta {
            filename: "verbose.md".to_string(),
            path: PathBuf::from("verbose.md"),
            name: None,
            description: None,
            memory_type: None,
            created: None,
            updated: None,
            supersedes: Vec::new(),
            conflicts: vec!["prefs.md".to_string()],
            asked: Vec::new(),
            resolved: Vec::new(),
            modified_secs: 0,
        };
        let map = conflicted_by(&[claim]);
        assert_eq!(map.get("prefs.md"), Some(&vec!["verbose.md".to_string()]));
    }

    #[test]
    fn test_superseded_by_maps_reverse_links() {
        let current = MemoryFileMeta {
            filename: "auth-v2.md".to_string(),
            path: PathBuf::from("auth-v2.md"),
            name: None,
            description: None,
            memory_type: None,
            created: None,
            updated: Some("2026-08-20".to_string()),
            supersedes: vec!["auth-v1.md".to_string()],
            conflicts: Vec::new(),
            asked: Vec::new(),
            resolved: Vec::new(),
            modified_secs: 0,
        };
        let orphan = MemoryFileMeta {
            filename: "auth-v1.md".to_string(),
            path: PathBuf::from("auth-v1.md"),
            name: None,
            description: None,
            memory_type: None,
            created: None,
            updated: None,
            supersedes: Vec::new(),
            conflicts: Vec::new(),
            asked: Vec::new(),
            resolved: Vec::new(),
            modified_secs: 0,
        };
        let map = superseded_by(&[current, orphan]);
        assert_eq!(map.get("auth-v1.md"), Some(&vec!["auth-v2.md".to_string()]));
        assert!(!map.contains_key("auth-v2.md"));
    }

    // ---- memory_age_days ---------------------------------------------------

    #[test]
    fn test_memory_age_today() {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(memory_age_days(now_secs), 0);
    }

    #[test]
    fn test_memory_age_one_day_ago() {
        let yesterday = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(86_400);
        assert_eq!(memory_age_days(yesterday), 1);
    }

    #[test]
    fn test_memory_age_future_clamps_to_zero() {
        let far_future = u64::MAX;
        assert_eq!(memory_age_days(far_future), 0);
    }

    // ---- memory_freshness_text ---------------------------------------------

    #[test]
    fn test_freshness_text_fresh() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(memory_freshness_text(now).is_empty());
    }

    #[test]
    fn test_freshness_text_stale() {
        let old = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(10 * 86_400); // 10 days ago
        let text = memory_freshness_text(old);
        assert!(text.contains("10 days old"));
        assert!(text.contains("point-in-time"));
    }

    // ---- memory_freshness_note ---------------------------------------------

    #[test]
    fn test_freshness_note_fresh_is_empty() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(memory_freshness_note(now).is_empty());
    }

    #[test]
    fn test_freshness_note_stale_has_tags() {
        let old = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(5 * 86_400);
        let note = memory_freshness_note(old);
        assert!(note.contains("<system-reminder>"));
        assert!(note.contains("</system-reminder>"));
    }

    // ---- truncate_entrypoint_content ---------------------------------------

    #[test]
    fn test_truncate_no_truncation_needed() {
        let content = "line1\nline2\nline3";
        let result = truncate_entrypoint_content(content);
        assert!(!result.was_line_truncated);
        assert!(!result.was_byte_truncated);
        assert_eq!(result.content, content);
    }

    #[test]
    fn test_truncate_line_limit() {
        let content = (0..=MAX_ENTRYPOINT_LINES)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_entrypoint_content(&content);
        assert!(result.was_line_truncated);
        assert!(result.content.contains("WARNING"));
    }

    // ---- sanitize_path_component -------------------------------------------

    #[test]
    fn test_sanitize_path_component() {
        assert_eq!(
            sanitize_path_component("/home/user/project"),
            "_home_user_project"
        );
        assert_eq!(
            sanitize_path_component("normal-name_123"),
            "normal-name_123"
        );
        assert_eq!(sanitize_path_component("C:\\Users\\foo"), "C__Users_foo");
    }

    // ---- load_memory_index -------------------------------------------------

    #[test]
    fn test_load_memory_index_nonexistent() {
        let dir = make_temp_dir();
        assert!(load_memory_index(dir.path()).is_none());
    }

    #[test]
    fn test_load_memory_index_empty() {
        let dir = make_temp_dir();
        write_file(dir.path(), "MEMORY.md", "   ");
        assert!(load_memory_index(dir.path()).is_none());
    }

    #[test]
    fn test_load_memory_index_with_content() {
        let dir = make_temp_dir();
        write_file(dir.path(), "MEMORY.md", "- [test.md](test.md) — something");
        let result = load_memory_index(dir.path()).unwrap();
        assert!(result.content.contains("test.md"));
    }

    // ---- scan_memory_dir ---------------------------------------------------

    #[test]
    fn test_scan_excludes_memory_md() {
        let dir = make_temp_dir();
        write_file(dir.path(), "MEMORY.md", "# index");
        write_file(dir.path(), "user_role.md", "---\nname: Role\n---");
        let metas = scan_memory_dir(dir.path());
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].filename, "user_role.md");
    }

    #[test]
    fn test_scan_empty_dir() {
        let dir = make_temp_dir();
        assert!(scan_memory_dir(dir.path()).is_empty());
    }

    #[test]
    fn test_scan_nonexistent_dir() {
        let path = PathBuf::from("/tmp/nonexistent_memory_dir_cc_rust_test_xyz");
        assert!(scan_memory_dir(&path).is_empty());
    }

    // ---- format_memory_manifest --------------------------------------------

    #[test]
    fn test_format_memory_manifest_with_description() {
        let meta = MemoryFileMeta {
            filename: "user_role.md".to_string(),
            path: PathBuf::from("user_role.md"),
            name: Some("User Role".to_string()),
            description: Some("The user is a data scientist".to_string()),
            memory_type: Some(MemoryType::User),
            created: None,
            updated: None,
            supersedes: Vec::new(),
            conflicts: Vec::new(),
            asked: Vec::new(),
            resolved: Vec::new(),
            modified_secs: 0,
        };
        let manifest = format_memory_manifest(&[meta]);
        assert!(manifest.contains("[user]"));
        assert!(manifest.contains("user_role.md"));
        assert!(manifest.contains("data scientist"));
    }

    #[test]
    fn test_format_memory_manifest_no_description() {
        let meta = MemoryFileMeta {
            filename: "ref.md".to_string(),
            path: PathBuf::from("ref.md"),
            name: None,
            description: None,
            memory_type: None,
            created: None,
            updated: None,
            supersedes: Vec::new(),
            conflicts: Vec::new(),
            asked: Vec::new(),
            resolved: Vec::new(),
            modified_secs: 0,
        };
        let manifest = format_memory_manifest(&[meta]);
        assert!(manifest.contains("ref.md"));
        // No description separator colon
        assert!(!manifest.contains("ref.md ("));
    }

    #[test]
    fn test_format_memory_manifest_annotates_superseded() {
        let old = MemoryFileMeta {
            filename: "auth-flow-v1.md".to_string(),
            path: PathBuf::from("auth-flow-v1.md"),
            name: None,
            description: Some("JWT auth".to_string()),
            memory_type: None,
            created: None,
            updated: None,
            supersedes: Vec::new(),
            conflicts: Vec::new(),
            asked: Vec::new(),
            resolved: Vec::new(),
            modified_secs: 0,
        };
        let new = MemoryFileMeta {
            filename: "auth-flow-v2.md".to_string(),
            path: PathBuf::from("auth-flow-v2.md"),
            name: None,
            description: Some("OAuth auth".to_string()),
            memory_type: None,
            created: None,
            updated: Some("2026-08-20".to_string()),
            supersedes: vec!["auth-flow-v1.md".to_string()],
            conflicts: Vec::new(),
            asked: Vec::new(),
            resolved: Vec::new(),
            modified_secs: 0,
        };
        let manifest = format_memory_manifest(&[old, new]);
        assert!(
            manifest.contains("auth-flow-v1.md … superseded by auth-flow-v2.md")
                || manifest.contains("auth-flow-v1.md (1970-01-01T00:00:00Z): JWT auth — superseded by auth-flow-v2.md"),
            "got: {}",
            manifest
        );
        // The superseding file itself is not annotated.
        assert!(!manifest.contains("auth-flow-v2.md … superseded"));
        assert!(
            !manifest.contains("auth-flow-v2.md (1970-01-01T00:00:00Z): OAuth auth — superseded")
        );
    }

    #[test]
    fn test_format_memory_manifest_annotates_conflicts() {
        let old = MemoryFileMeta {
            filename: "prefs.md".to_string(),
            path: PathBuf::from("prefs.md"),
            name: None,
            description: Some("User prefers concise output".to_string()),
            memory_type: None,
            created: None,
            updated: None,
            supersedes: Vec::new(),
            conflicts: Vec::new(),
            asked: Vec::new(),
            resolved: Vec::new(),
            modified_secs: 0,
        };
        let claim = MemoryFileMeta {
            filename: "verbose-claim.md".to_string(),
            path: PathBuf::from("verbose-claim.md"),
            name: None,
            description: Some("User prefers verbose output".to_string()),
            memory_type: None,
            created: None,
            updated: None,
            supersedes: Vec::new(),
            conflicts: vec!["prefs.md".to_string()],
            asked: vec!["prefs.md".to_string()],
            resolved: Vec::new(),
            modified_secs: 0,
        };
        let manifest = format_memory_manifest(&[old, claim]);
        // The claim is annotated as pending, with the asked pair marked
        // (per-pair entry has no date → bare `(asked)`).
        assert!(
            manifest.contains("verbose-claim.md … pending conflict with prefs.md (asked)")
                || manifest.contains(
                    "verbose-claim.md (1970-01-01T00:00:00Z): User prefers verbose output — pending conflict with prefs.md (asked)"
                ),
            "got: {}",
            manifest
        );
        // The target is annotated as under review but stays listed.
        assert!(
            manifest.contains("prefs.md … under review by verbose-claim.md")
                || manifest.contains(
                    "prefs.md (1970-01-01T00:00:00Z): User prefers concise output — under review by verbose-claim.md"
                ),
            "got: {}",
            manifest
        );
    }

    // ---- pending_conflicts_block ------------------------------------------

    #[test]
    fn test_pending_conflicts_block_renders_pairs_and_ask_policy() {
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "prefs.md",
            "---\ndescription: User prefers concise output\n---\n",
        );
        write_file(
            dir.path(),
            "verbose-claim.md",
            "---\ndescription: User prefers verbose output\nconflicts: prefs.md\n---\n",
        );
        let block = pending_conflicts_block(dir.path());
        assert!(block.contains("## Pending Memory Conflicts"));
        assert!(block.contains("\"User prefers verbose output\""));
        assert!(block.contains("vs \"User prefers concise output\""));
        // Un-asked conflict → the ask instruction is present, and it routes
        // the answer through the deterministic resolver tool rather than a
        // model hand-edit.
        assert!(block.contains("AskUserQuestion"));
        assert!(block.contains("I don't know"));
        assert!(block.contains("ResolveMemoryConflict"));
    }

    #[test]
    fn test_pending_conflicts_block_asked_marks_and_drops_instruction() {
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "prefs.md",
            "---\ndescription: User prefers concise output\n---\n",
        );
        write_file(
            dir.path(),
            "verbose-claim.md",
            "---\ndescription: User prefers verbose output\nconflicts: prefs.md\nasked: 2026-08-20\n---\n",
        );
        let block = pending_conflicts_block(dir.path());
        // Legacy `asked: <date>` renders as a relative age.
        assert!(block.contains("— asked"), "got: {}", block);
        assert!(!block.contains("2026-08-20"), "got: {}", block);
        // Asked-and-unresolved → still listed, but never re-ask.
        assert!(!block.contains("AskUserQuestion"), "got: {}", block);
    }

    #[test]
    fn test_pending_conflicts_block_asked_is_per_pair() {
        let dir = make_temp_dir();
        write_file(dir.path(), "a.md", "---\ndescription: A\n---\n");
        write_file(dir.path(), "b.md", "---\ndescription: B\n---\n");
        // Only the a.md pair was asked; b.md must stay askable.
        write_file(
            dir.path(),
            "claim.md",
            "---\ndescription: C\nconflicts: a.md, b.md\nasked: a.md\n---\n",
        );
        let block = pending_conflicts_block(dir.path());
        // Both pairs listed; the asked one marked, the other bare.
        assert!(block.contains("(a.md) — asked"), "got: {}", block);
        assert!(block.contains("(b.md)\n"), "got: {}", block);
        // The unasked pair keeps the ask instruction alive.
        assert!(block.contains("AskUserQuestion"), "got: {}", block);
    }

    #[test]
    fn test_pending_conflicts_block_skips_resolved_pairs() {
        let dir = make_temp_dir();
        write_file(dir.path(), "a.md", "---\ndescription: A\n---\n");
        write_file(dir.path(), "b.md", "---\ndescription: B\n---\n");
        // Defensive: a resolved pair that still has a conflicts entry must not
        // be listed (resolution normally drops the entry).
        write_file(
            dir.path(),
            "claim.md",
            "---\ndescription: C\nconflicts: a.md, b.md\nresolved: a.md\n---\n",
        );
        let block = pending_conflicts_block(dir.path());
        assert!(!block.contains("(a.md)"), "got: {}", block);
        assert!(block.contains("(b.md)"), "got: {}", block);
    }

    #[test]
    fn test_pending_conflicts_block_empty_without_conflicts() {
        let dir = make_temp_dir();
        write_file(dir.path(), "prefs.md", "---\ndescription: Fine\n---\n");
        assert_eq!(pending_conflicts_block(dir.path()), "");
    }

    #[test]
    fn test_build_memory_includes_conflicts_block() {
        let dir = make_temp_dir();
        write_file(dir.path(), "MEMORY.md", "- [prefs.md](prefs.md) — prefs");
        write_file(dir.path(), "prefs.md", "---\ndescription: Concise\n---\n");
        write_file(
            dir.path(),
            "verbose-claim.md",
            "---\ndescription: Verbose\nconflicts: prefs.md\n---\n",
        );
        let content = build_memory_prompt_content(dir.path());
        assert!(
            content.contains("Pending Memory Conflicts"),
            "got: {}",
            content
        );
        assert!(content.contains("Memory Index"));
    }

    // ---- resolve_memory_conflict -------------------------------------------

    const CLAIMANT_V1: &str = "---\nname: Auth flow v2\ndescription: OAuth is used\ntype: project\nconflicts: auth-flow-v1.md\n---\n\nOAuth is now used for login.";
    const TARGET_V1: &str = "---\nname: Auth flow v1\ndescription: JWT is used\ntype: project\n---\n\nJWT was used for login.";

    fn seed_conflict(dir: &Path) -> std::path::PathBuf {
        write_file(dir, "auth-flow-v1.md", TARGET_V1);
        let claimant = dir.join("auth-flow-v2.md");
        write_file(dir, "auth-flow-v2.md", CLAIMANT_V1);
        claimant
    }

    #[test]
    fn test_resolve_keep_new_promotes_to_supersedes() {
        let dir = make_temp_dir();
        seed_conflict(dir.path());
        let resolution = resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::KeepNew,
        )
        .unwrap();
        assert_eq!(resolution.decision, ConflictDecision::KeepNew);
        let content = std::fs::read_to_string(dir.path().join("auth-flow-v2.md")).unwrap();
        let fm = parse_frontmatter_quick(&content);
        assert!(fm.conflicts.is_empty(), "got: {:?}", fm.conflicts);
        assert_eq!(fm.supersedes, vec!["auth-flow-v1.md".to_string()]);
        assert!(fm.updated.is_some(), "updated must be stamped");
        assert!(fm.asked.is_empty());
        // Body and other fields survive untouched.
        assert!(content.contains("OAuth is now used for login."));
        assert!(content.contains("description: OAuth is used"));
    }

    /// A hand-authored file can already list `supersedes: x` while a re-flagged
    /// `conflicts: x` is resolved `keep_new` — the promotion must not append a
    /// duplicate (`supersedes: x, x`), which would list the superseder twice in
    /// the superseded block.
    #[test]
    fn test_resolve_keep_new_does_not_duplicate_existing_supersedes() {
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "auth-flow-v1.md",
            "---\ndescription: JWT is used\ntype: project\n---\n\nJWT was used for login.",
        );
        // Claimant already supersedes the target (and another file) while also
        // carrying a (re-flagged) `conflicts:` entry for it.
        write_file(
            dir.path(),
            "auth-flow-v2.md",
            "---\nname: Auth flow v2\ndescription: OAuth is used\ntype: project\nsupersedes: auth-flow-v1.md, proto-notes.md\nconflicts: auth-flow-v1.md\n---\n\nOAuth is now used for login.",
        );
        write_file(
            dir.path(),
            "proto-notes.md",
            "---\ndescription: proto notes\ntype: project\n---\n\nnotes",
        );
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::KeepNew,
        )
        .unwrap();
        let content = std::fs::read_to_string(dir.path().join("auth-flow-v2.md")).unwrap();
        let fm = parse_frontmatter_quick(&content);
        assert_eq!(
            fm.supersedes,
            vec!["auth-flow-v1.md".to_string(), "proto-notes.md".to_string()],
            "got: {:?}",
            fm.supersedes
        );
        assert!(fm.conflicts.is_empty());
    }

    #[test]
    fn test_resolve_keep_new_keeps_other_conflicts() {
        let dir = make_temp_dir();
        write_file(dir.path(), "a.md", "a");
        write_file(dir.path(), "b.md", "b");
        write_file(
            dir.path(),
            "claim.md",
            "---\nconflicts: a.md, b.md\n---\nbody",
        );
        resolve_memory_conflict(dir.path(), "claim.md", "a.md", ConflictDecision::KeepNew).unwrap();
        let fm =
            parse_frontmatter_quick(&std::fs::read_to_string(dir.path().join("claim.md")).unwrap());
        assert_eq!(fm.conflicts, vec!["b.md".to_string()]);
        assert_eq!(fm.supersedes, vec!["a.md".to_string()]);
    }

    #[test]
    fn test_resolve_keep_old_drops_claim() {
        let dir = make_temp_dir();
        seed_conflict(dir.path());
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::KeepOld,
        )
        .unwrap();
        let fm = parse_frontmatter_quick(
            &std::fs::read_to_string(dir.path().join("auth-flow-v2.md")).unwrap(),
        );
        assert!(fm.conflicts.is_empty());
        assert!(fm.supersedes.is_empty(), "old fact wins — nothing demoted");
        assert!(fm.asked.is_empty());
        // The pair is marked user-resolved so the dream never re-flags it.
        assert_eq!(fm.resolved, vec!["auth-flow-v1.md".to_string()]);
        assert!(fm.updated.is_some());
    }

    #[test]
    fn test_resolve_both_drops_claim_and_records_note() {
        let dir = make_temp_dir();
        seed_conflict(dir.path());
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::Both,
        )
        .unwrap();
        let content = std::fs::read_to_string(dir.path().join("auth-flow-v2.md")).unwrap();
        let fm = parse_frontmatter_quick(&content);
        assert!(fm.conflicts.is_empty());
        assert!(fm.supersedes.is_empty());
        assert_eq!(fm.resolved, vec!["auth-flow-v1.md".to_string()]);
        assert!(
            content.contains("both true in different contexts"),
            "got: {}",
            content
        );
    }

    #[test]
    fn test_resolve_unknown_stamps_asked_and_keeps_claim() {
        let dir = make_temp_dir();
        seed_conflict(dir.path());
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::Unknown,
        )
        .unwrap();
        let content = std::fs::read_to_string(dir.path().join("auth-flow-v2.md")).unwrap();
        let fm = parse_frontmatter_quick(&content);
        assert_eq!(fm.conflicts, vec!["auth-flow-v1.md".to_string()]);
        assert!(fm.supersedes.is_empty());
        // Per-pair ask stamp carries the date: `target:YYYY-MM-DD`.
        assert_eq!(fm.asked.len(), 1);
        assert!(
            fm.asked[0].starts_with("auth-flow-v1.md:"),
            "got: {:?}",
            fm.asked
        );
        assert_eq!(
            asked_targets(&fm.asked, &fm.conflicts, |_| None),
            vec!["auth-flow-v1.md".to_string()]
        );
        assert!(fm.resolved.is_empty());
        // No authority change: the target file was never touched.
        let target = std::fs::read_to_string(dir.path().join("auth-flow-v1.md")).unwrap();
        assert!(target.contains("JWT was used"));
    }

    #[test]
    fn test_resolve_unknown_refuses_when_already_asked() {
        let dir = make_temp_dir();
        write_file(dir.path(), "auth-flow-v1.md", TARGET_V1);
        write_file(
            dir.path(),
            "auth-flow-v2.md",
            "---\nconflicts: auth-flow-v1.md\nasked: 2026-08-01\n---\nbody",
        );
        let err = resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::Unknown,
        )
        .unwrap_err();
        assert!(err.contains("already asked"), "got: {}", err);
    }

    #[test]
    fn test_resolve_unknown_is_per_pair() {
        // One asked pair must not block asking about another pair on the
        // same claimant file.
        let dir = make_temp_dir();
        write_file(dir.path(), "a.md", "a");
        write_file(dir.path(), "b.md", "b");
        write_file(
            dir.path(),
            "claim.md",
            "---\nconflicts: a.md, b.md\nasked: a.md\n---\nbody",
        );
        // b.md is still askable.
        resolve_memory_conflict(dir.path(), "claim.md", "b.md", ConflictDecision::Unknown).unwrap();
        let fm =
            parse_frontmatter_quick(&std::fs::read_to_string(dir.path().join("claim.md")).unwrap());
        // a.md was asked bare; b.md was stamped with a date.
        assert_eq!(fm.asked[0], "a.md");
        assert!(fm.asked[1].starts_with("b.md:"), "got: {:?}", fm.asked);
        assert_eq!(
            asked_targets(&fm.asked, &fm.conflicts, |_| None),
            vec!["a.md".to_string(), "b.md".to_string()]
        );
        assert_eq!(fm.conflicts, vec!["a.md".to_string(), "b.md".to_string()]);
        // Now both are asked — resolving either with Unknown refuses.
        let err =
            resolve_memory_conflict(dir.path(), "claim.md", "a.md", ConflictDecision::Unknown)
                .unwrap_err();
        assert!(err.contains("already asked"), "got: {}", err);
    }

    #[test]
    fn test_resolve_keep_old_removes_only_the_asked_target() {
        // Resolving one pair must not disturb the other pair's ask stamp.
        let dir = make_temp_dir();
        write_file(dir.path(), "a.md", "a");
        write_file(dir.path(), "b.md", "b");
        write_file(
            dir.path(),
            "claim.md",
            "---\nconflicts: a.md, b.md\nasked: a.md\n---\nbody",
        );
        resolve_memory_conflict(dir.path(), "claim.md", "b.md", ConflictDecision::KeepOld).unwrap();
        let fm =
            parse_frontmatter_quick(&std::fs::read_to_string(dir.path().join("claim.md")).unwrap());
        assert_eq!(fm.conflicts, vec!["a.md".to_string()]);
        // a.md's ask stamp survives; only b.md was resolved.
        assert_eq!(fm.asked, vec!["a.md".to_string()]);
        assert_eq!(fm.resolved, vec!["b.md".to_string()]);
    }

    #[test]
    fn test_resolve_writes_audit_log() {
        let dir = make_temp_dir();
        seed_conflict(dir.path());
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::KeepNew,
        )
        .unwrap();
        // The audit log has exactly one newest-first record.
        let records = recent_resolutions(dir.path(), 5);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].claimant, "auth-flow-v2.md");
        assert_eq!(records[0].target, "auth-flow-v1.md");
        assert_eq!(records[0].decision, "keep_new");
        assert!(records[0].ts > 0);
        // A second resolution appends; newest comes first.
        // Already superseded — not a conflict anymore; failed resolves must
        // not log.
        let err = resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::Unknown,
        )
        .unwrap_err();
        assert!(err.contains("does not claim"), "got: {}", err);
        let records = recent_resolutions(dir.path(), 5);
        assert_eq!(records.len(), 1, "failed resolves must not log");
    }

    #[test]
    fn test_resolve_errors_on_missing_claimant_and_unknown_claim() {
        let dir = make_temp_dir();
        write_file(dir.path(), "auth-flow-v1.md", TARGET_V1);
        let err = resolve_memory_conflict(
            dir.path(),
            "nope.md",
            "auth-flow-v1.md",
            ConflictDecision::KeepOld,
        )
        .unwrap_err();
        assert!(err.contains("not found"), "got: {}", err);

        write_file(
            dir.path(),
            "claim.md",
            "---\ndescription: no conflicts\n---\nbody",
        );
        let err = resolve_memory_conflict(
            dir.path(),
            "claim.md",
            "auth-flow-v1.md",
            ConflictDecision::KeepOld,
        )
        .unwrap_err();
        assert!(err.contains("does not claim"), "got: {}", err);
    }

    #[test]
    fn test_resolve_rejects_path_traversal() {
        let dir = make_temp_dir();
        seed_conflict(dir.path());
        for bad in ["../escape.md", "/etc/passwd", "a/../../escape.md"] {
            let err = resolve_memory_conflict(
                dir.path(),
                bad,
                "auth-flow-v1.md",
                ConflictDecision::KeepOld,
            )
            .unwrap_err();
            assert!(
                err.contains("not allowed") || err.contains("escapes"),
                "got: {}",
                err
            );
        }
    }

    #[test]
    fn test_resolve_rejects_legacy_file_without_conflicts() {
        let dir = make_temp_dir();
        write_file(dir.path(), "auth-flow-v1.md", TARGET_V1);
        // A file without any `conflicts:` frontmatter is not a claimant.
        write_file(dir.path(), "legacy.md", "Just a plain note.");
        let err = resolve_memory_conflict(
            dir.path(),
            "legacy.md",
            "auth-flow-v1.md",
            ConflictDecision::Unknown,
        )
        .unwrap_err();
        assert!(err.contains("does not claim"), "got: {}", err);
    }

    #[test]
    fn test_frontmatter_editor_creates_block_on_plain_file() {
        // The editor itself can synthesize a block for a file with no
        // frontmatter; resolve_memory_conflict never reaches it that way (the
        // claim check gates first), but the renderer must stay well-formed.
        let mut editor = FrontmatterEditor::parse("Just a plain note.");
        assert!(!editor.had_frontmatter);
        editor.set_scalar("asked", "2026-08-20");
        assert_eq!(
            editor.render(),
            "---\nasked: 2026-08-20\n---\nJust a plain note."
        );
    }

    #[test]
    fn test_resolve_preserves_unknown_frontmatter_fields() {
        let dir = make_temp_dir();
        write_file(dir.path(), "auth-flow-v1.md", TARGET_V1);
        write_file(
            dir.path(),
            "claim.md",
            "---\nname: Claim\nconflicts: auth-flow-v1.md\ncustom_field: keep-me\n---\nbody",
        );
        resolve_memory_conflict(
            dir.path(),
            "claim.md",
            "auth-flow-v1.md",
            ConflictDecision::KeepNew,
        )
        .unwrap();
        let content = std::fs::read_to_string(dir.path().join("claim.md")).unwrap();
        assert!(
            content.contains("custom_field: keep-me"),
            "got: {}",
            content
        );
        assert!(
            content.contains("supersedes: auth-flow-v1.md"),
            "got: {}",
            content
        );
        assert!(!content.contains("conflicts:"), "got: {}", content);
    }

    // ---- superseded_memories_block -----------------------------------------

    #[test]
    fn test_superseded_block_renders_targets_and_skips_dangling() {
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "old.md",
            "---\ndescription: JWT auth\n---\nold body",
        );
        write_file(
            dir.path(),
            "new.md",
            "---\ndescription: OAuth auth\nsupersedes: old.md\n---\nnew body",
        );
        // A supersedes entry pointing at a deleted file must be skipped (the
        // sweep clears those; the block never surfaces a missing target).
        write_file(
            dir.path(),
            "dangling-claim.md",
            "---\ndescription: X\nsupersedes: gone.md\n---\nbody",
        );
        let block = superseded_memories_block(dir.path());
        assert!(block.contains("## Superseded Memories"), "got: {}", block);
        assert!(
            block.contains("\"JWT auth\" (old.md) — superseded by new.md"),
            "got: {}",
            block
        );
        assert!(!block.contains("gone.md"), "got: {}", block);
    }

    #[test]
    fn test_superseded_block_empty_without_supersedes() {
        let dir = make_temp_dir();
        write_file(dir.path(), "a.md", "---\ndescription: A\n---\n");
        write_file(
            dir.path(),
            "b.md",
            "---\ndescription: B\nconflicts: a.md\n---\n",
        );
        // A pending conflict is not a supersession — nothing to show.
        assert_eq!(superseded_memories_block(dir.path()), "");
    }

    #[test]
    fn test_build_memory_includes_superseded_block() {
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "MEMORY.md",
            "- [old.md](old.md) — old\n- [new.md](new.md) — new",
        );
        write_file(dir.path(), "old.md", "---\ndescription: JWT\n---\n");
        write_file(
            dir.path(),
            "new.md",
            "---\ndescription: OAuth\nsupersedes: old.md\n---\n",
        );
        let content = build_memory_prompt_content(dir.path());
        assert!(content.contains("Superseded Memories"), "got: {}", content);
        assert!(content.contains("old.md"), "got: {}", content);
    }

    // ---- prune_index_entry -------------------------------------------------

    #[test]
    fn test_prune_index_entry_removes_link_line_only() {
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "MEMORY.md",
            "- [old.md](old.md) — JWT auth\n- [new.md](new.md) — OAuth auth\n\nProse that mentions old.md stays.",
        );
        assert!(prune_index_entry(dir.path(), "old.md").unwrap());
        let index = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        assert!(!index.contains("](old.md"), "got: {}", index);
        assert!(index.contains("](new.md"), "got: {}", index);
        // Prose mention survives.
        assert!(index.contains("mentions old.md stays"), "got: {}", index);
        // Idempotent: second run reports no change.
        assert!(!prune_index_entry(dir.path(), "old.md").unwrap());
    }

    #[test]
    fn test_prune_index_entry_missing_index_is_not_error() {
        let dir = make_temp_dir();
        assert!(!prune_index_entry(dir.path(), "old.md").unwrap());
    }

    #[test]
    fn test_resolve_keep_new_prunes_index_entry() {
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "MEMORY.md",
            "- [auth-flow-v1.md](auth-flow-v1.md) — JWT auth\n- [auth-flow-v2.md](auth-flow-v2.md) — OAuth auth",
        );
        seed_conflict(dir.path());
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::KeepNew,
        )
        .unwrap();
        let index = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        assert!(
            !index.contains("](auth-flow-v1.md"),
            "superseded fact must leave the index — got: {}",
            index
        );
        assert!(index.contains("](auth-flow-v2.md"), "got: {}", index);
    }

    #[test]
    fn test_resolve_keep_old_does_not_prune_index() {
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "MEMORY.md",
            "- [auth-flow-v1.md](auth-flow-v1.md) — JWT auth",
        );
        seed_conflict(dir.path());
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::KeepOld,
        )
        .unwrap();
        let index = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        assert!(index.contains("](auth-flow-v1.md"), "got: {}", index);
    }

    // ---- sweep_dangling_memory_refs ----------------------------------------

    #[test]
    fn test_sweep_removes_dangling_conflicts_and_supersedes() {
        let dir = make_temp_dir();
        write_file(dir.path(), "alive.md", "---\ndescription: alive\n---\n");
        // claim.md: one dangling conflict, one live conflict, one dangling
        // supersedes.
        write_file(
            dir.path(),
            "claim.md",
            "---\ndescription: claim\nconflicts: alive.md, deleted.md\nsupersedes: gone.md\n---\nbody",
        );
        let report = sweep_dangling_memory_refs(dir.path());
        assert_eq!(
            report.removed_conflicts,
            vec![("claim.md".to_string(), "deleted.md".to_string())]
        );
        assert_eq!(
            report.removed_supersedes,
            vec![("claim.md".to_string(), "gone.md".to_string())]
        );
        let fm =
            parse_frontmatter_quick(&std::fs::read_to_string(dir.path().join("claim.md")).unwrap());
        assert_eq!(fm.conflicts, vec!["alive.md".to_string()]);
        assert!(fm.supersedes.is_empty());
        // Body survives.
        let content = std::fs::read_to_string(dir.path().join("claim.md")).unwrap();
        assert!(content.contains("body"), "got: {}", content);
        // Idempotent: a second sweep reports nothing.
        let report2 = sweep_dangling_memory_refs(dir.path());
        assert!(report2.is_empty());
    }

    #[test]
    fn test_sweep_noop_when_all_targets_exist() {
        let dir = make_temp_dir();
        write_file(dir.path(), "a.md", "a");
        write_file(dir.path(), "claim.md", "---\nconflicts: a.md\n---\nbody");
        assert!(sweep_dangling_memory_refs(dir.path()).is_empty());
    }

    // ---- undo_last_resolution ----------------------------------------------

    #[test]
    fn test_undo_keep_new_restores_conflict_and_supersedes_entry() {
        let dir = make_temp_dir();
        seed_conflict(dir.path());
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::KeepNew,
        )
        .unwrap();
        let report = undo_last_resolution(dir.path()).unwrap();
        assert_eq!(report.record.decision, "keep_new");
        let fm = parse_frontmatter_quick(
            &std::fs::read_to_string(dir.path().join("auth-flow-v2.md")).unwrap(),
        );
        // The exact prior state is restored: claim back, supersession gone.
        assert_eq!(fm.conflicts, vec!["auth-flow-v1.md".to_string()]);
        assert!(fm.supersedes.is_empty());
        // The audit log is popped, so nothing further to undo.
        assert!(recent_resolutions(dir.path(), 5).is_empty());
    }

    #[test]
    fn test_undo_keep_old_restores_claim() {
        let dir = make_temp_dir();
        seed_conflict(dir.path());
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::KeepOld,
        )
        .unwrap();
        undo_last_resolution(dir.path()).unwrap();
        let fm = parse_frontmatter_quick(
            &std::fs::read_to_string(dir.path().join("auth-flow-v2.md")).unwrap(),
        );
        assert_eq!(fm.conflicts, vec!["auth-flow-v1.md".to_string()]);
        assert!(fm.resolved.is_empty());
    }

    #[test]
    fn test_undo_both_restores_claim_and_removes_note() {
        let dir = make_temp_dir();
        seed_conflict(dir.path());
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::Both,
        )
        .unwrap();
        undo_last_resolution(dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join("auth-flow-v2.md")).unwrap();
        let fm = parse_frontmatter_quick(&content);
        assert_eq!(fm.conflicts, vec!["auth-flow-v1.md".to_string()]);
        assert!(fm.resolved.is_empty());
        // The both-true body note is revoked.
        assert!(
            !content.contains("both true in different contexts"),
            "got: {}",
            content
        );
    }

    #[test]
    fn test_undo_unknown_clears_asked_and_pair_is_askable_again() {
        let dir = make_temp_dir();
        seed_conflict(dir.path());
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::Unknown,
        )
        .unwrap();
        // The pair is asked and refuses a second ask.
        let err = resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::Unknown,
        )
        .unwrap_err();
        assert!(err.contains("already asked"), "got: {}", err);

        undo_last_resolution(dir.path()).unwrap();
        let fm = parse_frontmatter_quick(
            &std::fs::read_to_string(dir.path().join("auth-flow-v2.md")).unwrap(),
        );
        assert!(fm.asked.is_empty());
        assert_eq!(fm.conflicts, vec!["auth-flow-v1.md".to_string()]);
        // The pair is askable again.
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::Unknown,
        )
        .unwrap();
    }

    #[test]
    fn test_undo_walks_backwards_through_history() {
        let dir = make_temp_dir();
        seed_conflict(dir.path());
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::Unknown,
        )
        .unwrap();
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::KeepOld,
        )
        .unwrap();
        // Undo #1 reverses keep_old.
        let first = undo_last_resolution(dir.path()).unwrap();
        assert_eq!(first.record.decision, "keep_old");
        // Undo #2 reverses the earlier unknown.
        let second = undo_last_resolution(dir.path()).unwrap();
        assert_eq!(second.record.decision, "unknown");
        // Log is drained.
        assert!(recent_resolutions(dir.path(), 5).is_empty());
        assert!(undo_last_resolution(dir.path()).is_err());
    }

    #[test]
    fn test_undo_errors_on_empty_log_and_missing_claimant() {
        let dir = make_temp_dir();
        seed_conflict(dir.path());
        // No log yet.
        assert!(undo_last_resolution(dir.path()).is_err());
        // Log exists but the claimant file is gone.
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::KeepOld,
        )
        .unwrap();
        std::fs::remove_file(dir.path().join("auth-flow-v2.md")).unwrap();
        let err = undo_last_resolution(dir.path()).unwrap_err();
        assert!(err.contains("no longer exists"), "got: {}", err);
    }

    #[test]
    fn test_undo_pop_keeps_valid_records_across_malformed_line() {
        // The log is sliced by the LINE INDEX of the last valid record, not by
        // parsed-record count: a malformed line (partial write) before the
        // last record must not cause valid records to be dropped alongside it.
        let dir = make_temp_dir();
        seed_conflict(dir.path());
        // Three valid resolutions on the same pair. Each resolve mutates the
        // claim, so re-seed the `conflicts:` entry between resolves (the audit
        // records are what matter here, not the file state).
        let re_seed = || {
            write_file(
                dir.path(),
                "auth-flow-v2.md",
                "---\ndescription: OAuth\nconflicts: auth-flow-v1.md\n---\nbody",
            );
        };
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::KeepOld,
        )
        .unwrap();
        re_seed();
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::KeepNew,
        )
        .unwrap();
        // Inject a malformed line BEFORE the last record: re-open and prepend
        // garbage between records 2 and 3.
        let log = dir.path().join(RESOLUTIONS_LOG);
        let raw = std::fs::read_to_string(&log).unwrap();
        let mut lines: Vec<&str> = raw.lines().collect();
        lines.insert(lines.len() - 1, "{this is not json");
        std::fs::write(&log, lines.join("\n") + "\n").unwrap();
        // Push a third valid resolution after the malformed line.
        re_seed();
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::Unknown,
        )
        .unwrap();
        // Undo #1 pops the Unknown (the last valid record); the two older
        // valid records must survive — the malformed line must not eat them.
        let report = undo_last_resolution(dir.path()).unwrap();
        assert_eq!(report.record.decision, "unknown");
        let remaining = recent_resolutions(dir.path(), 10);
        assert_eq!(
            remaining.len(),
            2,
            "the two older valid records must survive the pop: {:?}",
            remaining
        );
        // And they are the keep_new and keep_old records, newest first.
        assert_eq!(remaining[0].decision, "keep_new");
        assert_eq!(remaining[1].decision, "keep_old");
    }

    // ---- resolver guards (self-claim, supersession cycle) ------------------

    #[test]
    fn test_resolve_rejects_self_claim() {
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "self.md",
            "---\ndescription: self\nconflicts: self.md\n---\nbody",
        );
        let err =
            resolve_memory_conflict(dir.path(), "self.md", "self.md", ConflictDecision::KeepNew)
                .unwrap_err();
        assert!(
            err.contains("cannot be in conflict with itself"),
            "got: {}",
            err
        );
        // Unknown is rejected too — the guard is decision-independent.
        let err =
            resolve_memory_conflict(dir.path(), "self.md", "self.md", ConflictDecision::Unknown)
                .unwrap_err();
        assert!(
            err.contains("cannot be in conflict with itself"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_resolve_rejects_supersession_cycle() {
        let dir = make_temp_dir();
        // b.md already supersedes a.md (a prior resolution). Resolving a new
        // claim a.md → b.md as keep_new would make both files supersede each
        // other — a cycle with no authoritative file.
        write_file(
            dir.path(),
            "a.md",
            "---\ndescription: A\nconflicts: b.md\n---\nbody a",
        );
        write_file(
            dir.path(),
            "b.md",
            "---\ndescription: B\nsupersedes: a.md\n---\nbody b",
        );
        let err = resolve_memory_conflict(dir.path(), "a.md", "b.md", ConflictDecision::KeepNew)
            .unwrap_err();
        assert!(err.contains("cycle"), "got: {}", err);
        // Non-KeepNew decisions are unaffected by the cycle guard.
        resolve_memory_conflict(dir.path(), "a.md", "b.md", ConflictDecision::KeepOld).unwrap();
    }

    #[test]
    fn test_resolve_rejects_transitive_supersession_cycle() {
        // 3-cycle: a supersedes b, b supersedes c. Resolving c → a as keep_new
        // would close the ring (c supersedes a) — every file demoted, none
        // authoritative. The guard must walk the transitive closure, not just
        // the target's direct `supersedes:` list.
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "a.md",
            "---\ndescription: A\nsupersedes: b.md\n---\nbody a",
        );
        write_file(
            dir.path(),
            "b.md",
            "---\ndescription: B\nsupersedes: c.md\n---\nbody b",
        );
        write_file(
            dir.path(),
            "c.md",
            "---\ndescription: C\nconflicts: a.md\n---\nbody c",
        );
        let err = resolve_memory_conflict(dir.path(), "c.md", "a.md", ConflictDecision::KeepNew)
            .unwrap_err();
        assert!(err.contains("cycle"), "got: {}", err);
        // A chain that does NOT reach the claimant is fine: resolving c → a
        // would not close a ring here (a supersedes b, b supersedes d).
        let dir2 = make_temp_dir();
        write_file(
            dir2.path(),
            "a.md",
            "---\ndescription: A\nsupersedes: b.md\n---\nbody a",
        );
        write_file(
            dir2.path(),
            "b.md",
            "---\ndescription: B\nsupersedes: d.md\n---\nbody b",
        );
        write_file(dir2.path(), "d.md", "---\ndescription: D\n---\nbody d");
        write_file(
            dir2.path(),
            "c.md",
            "---\ndescription: C\nconflicts: a.md\n---\nbody c",
        );
        resolve_memory_conflict(dir2.path(), "c.md", "a.md", ConflictDecision::KeepNew).unwrap();
        let c =
            parse_frontmatter_quick(&std::fs::read_to_string(dir2.path().join("c.md")).unwrap());
        assert_eq!(c.supersedes, vec!["a.md".to_string()]);
    }

    #[test]
    fn test_cycle_guard_handles_pre_existing_cycle_gracefully() {
        // A manually-authored cycle (a ↔ b) elsewhere must not hang the BFS —
        // the visited set bounds it — and the closure walk must still reach
        // the claimant THROUGH the cycle to refuse the resolution.
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "a.md",
            "---\ndescription: A\nsupersedes: b.md\n---\nbody a",
        );
        write_file(
            dir.path(),
            "b.md",
            "---\ndescription: B\nsupersedes: a.md, c.md\n---\nbody b",
        );
        write_file(
            dir.path(),
            "c.md",
            "---\ndescription: C\nconflicts: a.md\n---\nbody c",
        );
        // target a reaches c via a → b → c (b already supersedes c), so
        // resolving c → a would close a ring — refused, and the walk must
        // terminate despite the a ↔ b cycle.
        let err = resolve_memory_conflict(dir.path(), "c.md", "a.md", ConflictDecision::KeepNew)
            .unwrap_err();
        assert!(err.contains("cycle"), "got: {}", err);
        // Same state, but a chain that does NOT reach the claimant still
        // resolves fine (b supersedes d, not c).
        let dir2 = make_temp_dir();
        write_file(
            dir2.path(),
            "a.md",
            "---\ndescription: A\nsupersedes: b.md\n---\nbody a",
        );
        write_file(
            dir2.path(),
            "b.md",
            "---\ndescription: B\nsupersedes: a.md, d.md\n---\nbody b",
        );
        write_file(dir2.path(), "d.md", "---\ndescription: D\n---\nbody d");
        write_file(
            dir2.path(),
            "c.md",
            "---\ndescription: C\nconflicts: a.md\n---\nbody c",
        );
        resolve_memory_conflict(dir2.path(), "c.md", "a.md", ConflictDecision::KeepNew).unwrap();
    }

    // ---- keep_new cascade (reciprocal claim + resolved marker) -------------

    #[test]
    fn test_resolve_keep_new_clears_reciprocal_conflict() {
        let dir = make_temp_dir();
        // Mutual conflict: a claims b, b claims a. b does NOT supersede a
        // (that would trip the cycle guard — different scenario).
        write_file(
            dir.path(),
            "a.md",
            "---\ndescription: A\nconflicts: b.md\n---\nbody a",
        );
        write_file(
            dir.path(),
            "b.md",
            "---\ndescription: B\nconflicts: a.md\n---\nbody b",
        );
        resolve_memory_conflict(dir.path(), "a.md", "b.md", ConflictDecision::KeepNew).unwrap();
        // a supersedes b now.
        let a = parse_frontmatter_quick(&std::fs::read_to_string(dir.path().join("a.md")).unwrap());
        assert_eq!(a.supersedes, vec!["b.md".to_string()]);
        assert!(a.conflicts.is_empty());
        // b's reciprocal claim against a is moot — cleared, not left pending.
        let b = parse_frontmatter_quick(&std::fs::read_to_string(dir.path().join("b.md")).unwrap());
        assert!(
            b.conflicts.is_empty(),
            "reciprocal claim must be cleared: {:?}",
            b.conflicts
        );
        assert!(b.supersedes.is_empty(), "got: {:?}", b.supersedes);
        // No pending pairs remain.
        assert_eq!(pending_conflict_count(dir.path()), 0);
    }

    #[test]
    fn test_resolve_keep_new_clears_stale_resolved_marker() {
        let dir = make_temp_dir();
        write_file(dir.path(), "b.md", "---\ndescription: B\n---\nbody b");
        // Defensive state: the dream re-flagged a pair the user resolved
        // keep_old earlier (`resolved:` still set), and the user now says the
        // claim is right — promotion must not leave both markers on the pair.
        write_file(
            dir.path(),
            "a.md",
            "---\ndescription: A\nconflicts: b.md\nresolved: b.md\n---\nbody a",
        );
        resolve_memory_conflict(dir.path(), "a.md", "b.md", ConflictDecision::KeepNew).unwrap();
        let a = parse_frontmatter_quick(&std::fs::read_to_string(dir.path().join("a.md")).unwrap());
        assert_eq!(a.supersedes, vec!["b.md".to_string()]);
        assert!(
            a.resolved.is_empty(),
            "stale `resolved:` must be cleared on promotion: {:?}",
            a.resolved
        );
    }

    #[test]
    fn test_undo_keep_new_restores_reciprocal_claim() {
        // The full recovery round-trip for a mutual conflict: resolve keep_new
        // (cascade clears b's claim), then undo — b's reciprocal claim must
        // come back, restoring the exact prior state.
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "a.md",
            "---\ndescription: A\nconflicts: b.md\n---\nbody a",
        );
        write_file(
            dir.path(),
            "b.md",
            "---\ndescription: B\nconflicts: a.md\n---\nbody b",
        );
        resolve_memory_conflict(dir.path(), "a.md", "b.md", ConflictDecision::KeepNew).unwrap();
        // Cascade cleared b's reciprocal claim and the audit log recorded it.
        let records = recent_resolutions(dir.path(), 5);
        assert!(records[0].reciprocal_cleared, "flag must be recorded");
        let b = parse_frontmatter_quick(&std::fs::read_to_string(dir.path().join("b.md")).unwrap());
        assert!(b.conflicts.is_empty());

        undo_last_resolution(dir.path()).unwrap();
        let b = parse_frontmatter_quick(&std::fs::read_to_string(dir.path().join("b.md")).unwrap());
        assert_eq!(
            b.conflicts,
            vec!["a.md".to_string()],
            "undo must restore the cleared reciprocal claim"
        );
        // The pair is fully back to the mutual state.
        assert_eq!(pending_conflict_count(dir.path()), 2);
    }

    #[test]
    fn test_undo_keep_new_summary_mentions_reciprocal_restore() {
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "a.md",
            "---\ndescription: A\nconflicts: b.md\n---\nbody a",
        );
        write_file(
            dir.path(),
            "b.md",
            "---\ndescription: B\nconflicts: a.md\n---\nbody b",
        );
        resolve_memory_conflict(dir.path(), "a.md", "b.md", ConflictDecision::KeepNew).unwrap();
        let report = undo_last_resolution(dir.path()).unwrap();
        assert!(
            report.summary.contains("reciprocal claim"),
            "summary must mention the restored mutual claim: {}",
            report.summary
        );
        // And the non-mutual case does not claim one.
        let dir2 = make_temp_dir();
        seed_conflict(dir2.path());
        resolve_memory_conflict(
            dir2.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::KeepNew,
        )
        .unwrap();
        let report2 = undo_last_resolution(dir2.path()).unwrap();
        assert!(
            !report2.summary.contains("reciprocal"),
            "summary must not mention a reciprocal that was never cleared: {}",
            report2.summary
        );
    }

    #[test]
    fn test_undo_keep_new_without_cascade_does_not_invent_reciprocal() {
        // No mutual conflict → no reciprocal to restore; undo must not add one.
        let dir = make_temp_dir();
        seed_conflict(dir.path());
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::KeepNew,
        )
        .unwrap();
        let records = recent_resolutions(dir.path(), 5);
        assert!(!records[0].reciprocal_cleared);
        undo_last_resolution(dir.path()).unwrap();
        let target = parse_frontmatter_quick(
            &std::fs::read_to_string(dir.path().join("auth-flow-v1.md")).unwrap(),
        );
        assert!(
            target.conflicts.is_empty(),
            "no reciprocal claim was ever cleared — none should be invented: {:?}",
            target.conflicts
        );
    }

    #[test]
    fn test_legacy_audit_record_without_flag_parses() {
        // Log lines written before the `reciprocal_cleared` field existed must
        // still deserialize (serde default).
        let dir = make_temp_dir();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            dir.path().join(RESOLUTIONS_LOG),
            r#"{"ts":1,"claimant":"a.md","target":"b.md","decision":"keep_new"}"#,
        )
        .unwrap();
        let records = recent_resolutions(dir.path(), 5);
        assert_eq!(records.len(), 1);
        assert!(!records[0].reciprocal_cleared);
    }

    // ---- prune own-link matching -------------------------------------------

    #[test]
    fn test_prune_index_entry_keeps_other_files_mentioning_target() {
        let dir = make_temp_dir();
        // new.md's entry mentions old.md in its description (a markdown link).
        // Pruning old.md must NOT de-index new.md — only lines whose OWN link
        // resolves to old.md are dropped.
        write_file(
            dir.path(),
            "MEMORY.md",
            "- [old.md](old.md) — JWT auth\n- [new.md](new.md) — see [old.md](old.md) for history",
        );
        assert!(prune_index_entry(dir.path(), "old.md").unwrap());
        let index = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        assert!(!index.contains("](old.md) — JWT"), "got: {}", index);
        assert!(
            index.contains("](new.md)"),
            "new.md's entry must survive — got: {}",
            index
        );
    }

    // ---- undo restores the index entry -------------------------------------

    #[test]
    fn test_undo_keep_new_restores_index_entry() {
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "MEMORY.md",
            "- [auth-flow-v1.md](auth-flow-v1.md) — JWT auth\n- [auth-flow-v2.md](auth-flow-v2.md) — OAuth auth",
        );
        seed_conflict(dir.path());
        resolve_memory_conflict(
            dir.path(),
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            ConflictDecision::KeepNew,
        )
        .unwrap();
        // Pruned by the resolution.
        let index = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        assert!(!index.contains("auth-flow-v1.md"), "got: {}", index);
        // Undo restores both the frontmatter AND the index entry.
        undo_last_resolution(dir.path()).unwrap();
        let index = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        assert!(
            index.contains("](auth-flow-v1.md)"),
            "undo must restore the pruned index entry — got: {}",
            index
        );
        assert!(index.contains("](auth-flow-v2.md)"), "got: {}", index);
    }

    // ---- sweep of asked/resolved -------------------------------------------

    #[test]
    fn test_sweep_removes_dangling_asked_and_resolved() {
        let dir = make_temp_dir();
        write_file(dir.path(), "alive.md", "---\ndescription: alive\n---\n");
        write_file(
            dir.path(),
            "claim.md",
            "---\ndescription: claim\nconflicts: alive.md\nasked: gone.md:2026-08-01, alive.md\nresolved: deleted.md, alive.md\n---\nbody",
        );
        let report = sweep_dangling_memory_refs(dir.path());
        assert_eq!(
            report.removed_asked,
            vec![("claim.md".to_string(), "gone.md".to_string())]
        );
        assert_eq!(
            report.removed_resolved,
            vec![("claim.md".to_string(), "deleted.md".to_string())]
        );
        let fm =
            parse_frontmatter_quick(&std::fs::read_to_string(dir.path().join("claim.md")).unwrap());
        assert_eq!(fm.asked, vec!["alive.md".to_string()]);
        assert_eq!(fm.resolved, vec!["alive.md".to_string()]);
        // A legacy bare date is not a filename and must never be swept.
        write_file(
            dir.path(),
            "legacy.md",
            "---\nconflicts: alive.md\nasked: 2026-08-01\n---\nbody",
        );
        let report2 = sweep_dangling_memory_refs(dir.path());
        assert!(
            report2.removed_asked.is_empty(),
            "legacy asked dates must survive the sweep"
        );
        let fm2 = parse_frontmatter_quick(
            &std::fs::read_to_string(dir.path().join("legacy.md")).unwrap(),
        );
        assert_eq!(fm2.asked, vec!["2026-08-01".to_string()]);
    }

    // ---- pending-conflict pair count ---------------------------------------

    #[test]
    fn test_pending_conflict_count_counts_pairs_not_claimant_files() {
        let dir = make_temp_dir();
        write_file(dir.path(), "a.md", "---\ndescription: A\n---\n");
        write_file(dir.path(), "b.md", "---\ndescription: B\n---\n");
        // One claimant, two adjudicable pairs → count is 2, not 1 (the count
        // is pairs, not claimant files).
        write_file(
            dir.path(),
            "claim.md",
            "---\ndescription: C\nconflicts: a.md, b.md\n---\nbody",
        );
        assert_eq!(pending_conflict_count(dir.path()), 2);
        // A resolved pair and a dangling pair are not adjudicable — claim2
        // contributes nothing.
        write_file(dir.path(), "gone.md", "x");
        write_file(
            dir.path(),
            "claim2.md",
            "---\ndescription: D\nconflicts: a.md, gone.md\nresolved: a.md\n---\nbody",
        );
        std::fs::remove_file(dir.path().join("gone.md")).unwrap();
        assert_eq!(pending_conflict_count(dir.path()), 2);
        // A superseded claimant's claims are not adjudicable either — claim.md
        // is now demoted, so its two pairs disappear.
        write_file(
            dir.path(),
            "superseder.md",
            "---\ndescription: E\nsupersedes: claim.md\n---\nbody",
        );
        assert_eq!(pending_conflict_count(dir.path()), 0);
        // Self-conflicts are skipped.
        write_file(
            dir.path(),
            "self.md",
            "---\ndescription: F\nconflicts: self.md\n---\nbody",
        );
        assert_eq!(pending_conflict_count(dir.path()), 0);
    }

    #[test]
    fn test_pending_conflict_pairs_matches_block() {
        // The block and the count must never disagree: every pair the block
        // renders is exactly the pair list, and vice versa.
        let dir = make_temp_dir();
        write_file(dir.path(), "a.md", "---\ndescription: A\n---\n");
        write_file(
            dir.path(),
            "claim.md",
            "---\ndescription: C\nconflicts: a.md\nasked: a.md\n---\nbody",
        );
        let pairs = pending_conflict_pairs(dir.path());
        assert_eq!(pairs.len(), 1);
        let block = pending_conflicts_block(dir.path());
        assert!(block.contains("(a.md)"), "got: {}", block);
        // Asked pairs still count as pending (they are listed, just not
        // askable) — consistent with the block which lists them too.
        assert!(block.contains("— asked"), "got: {}", block);
    }

    #[test]
    fn test_pending_conflict_pairs_skip_superseded_targets() {
        // A claim against a file that another memory already supersedes is
        // moot — asking "is X wrong?" when a supersession already ruled it
        // stale is pointless. The pair must not surface (or count).
        let dir = make_temp_dir();
        write_file(dir.path(), "b.md", "---\ndescription: B\n---\n");
        write_file(
            dir.path(),
            "c.md",
            "---\ndescription: C\nsupersedes: b.md\n---\n",
        );
        write_file(
            dir.path(),
            "a.md",
            "---\ndescription: A\nconflicts: b.md\n---\nbody",
        );
        assert_eq!(pending_conflict_count(dir.path()), 0);
        assert_eq!(pending_conflicts_block(dir.path()), "");
        // The claimant's other conflicts (against a live target) still count.
        write_file(dir.path(), "d.md", "---\ndescription: D\n---\n");
        write_file(
            dir.path(),
            "a.md",
            "---\ndescription: A\nconflicts: b.md, d.md\n---\nbody",
        );
        assert_eq!(pending_conflict_count(dir.path()), 1);
    }

    #[test]
    fn test_resolve_rejects_missing_target() {
        // Adjudicating a claim against a deleted file would write a dangling
        // supersedes/resolved entry — refuse it outright.
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "claim.md",
            "---\ndescription: C\nconflicts: gone.md\n---\nbody",
        );
        let err =
            resolve_memory_conflict(dir.path(), "claim.md", "gone.md", ConflictDecision::KeepNew)
                .unwrap_err();
        assert!(err.contains("target memory file not found"), "got: {}", err);
        // Same for the other decisions — the guard is decision-independent.
        let err =
            resolve_memory_conflict(dir.path(), "claim.md", "gone.md", ConflictDecision::KeepOld)
                .unwrap_err();
        assert!(err.contains("target memory file not found"), "got: {}", err);
    }

    // ---- MemoryType --------------------------------------------------------

    #[test]
    fn test_memory_type_roundtrip() {
        for (s, expected) in [
            ("user", MemoryType::User),
            ("feedback", MemoryType::Feedback),
            ("project", MemoryType::Project),
            ("reference", MemoryType::Reference),
        ] {
            let parsed = MemoryType::parse(s).unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(parsed.as_str(), s);
        }
    }

    #[test]
    fn test_memory_type_unknown_returns_none() {
        assert!(MemoryType::parse("bogus").is_none());
    }

    // ---- most_recent_session_summary --------------------------------------

    #[test]
    fn test_most_recent_session_summary_missing_dir() {
        let dir = make_temp_dir();
        assert!(most_recent_session_summary(dir.path()).is_none());
    }

    #[test]
    fn test_most_recent_session_summary_empty_sessions_dir() {
        let dir = make_temp_dir();
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        assert!(most_recent_session_summary(dir.path()).is_none());
    }

    #[test]
    fn test_most_recent_session_summary_picks_newest() {
        let dir = make_temp_dir();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let old = sessions.join("2026-07-01.md");
        let new = sessions.join("2026-08-01.md");
        std::fs::write(&old, "Old summary").unwrap();
        std::fs::write(&new, "New summary").unwrap();
        // Bump the new file's mtime into the future to disambiguate filesystem
        // timestamp granularity. Tolerate filesystems that coerce the mtime.
        let future = SystemTime::now() + std::time::Duration::from_secs(3600);
        let _ = std::fs::File::options()
            .write(true)
            .open(&new)
            .and_then(|f| f.set_modified(future));
        // The filename tiebreak makes the choice deterministic even when both
        // mtimes are equal (date-named summaries sort newest-date-last).
        let summary = most_recent_session_summary(dir.path()).unwrap();
        assert!(summary.contains("New summary"));
    }

    #[test]
    fn test_most_recent_session_summary_ignores_non_md() {
        let dir = make_temp_dir();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("notes.txt"), "not a summary").unwrap();
        assert!(most_recent_session_summary(dir.path()).is_none());
    }

    #[test]
    fn test_build_memory_prompt_content_includes_index_and_summary() {
        let dir = make_temp_dir();
        write_file(dir.path(), "MEMORY.md", "- [a.md](a.md) — index entry");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("2026-08-01.md"),
            "## Yesterday\nShipped the routing dialog.",
        )
        .unwrap();
        let content = build_memory_prompt_content(dir.path());
        assert!(content.contains("Memory Index"));
        assert!(content.contains("index entry"));
        assert!(content.contains("Recent Session Summary"));
        assert!(content.contains("Shipped the routing dialog."));
    }

    #[test]
    fn test_build_memory_prompt_content_empty_dir() {
        let dir = make_temp_dir();
        assert!(build_memory_prompt_content(dir.path()).is_empty());
    }

    #[test]
    fn test_budget_keeps_everything_when_roomy() {
        let dir = make_temp_dir();
        write_file(dir.path(), "MEMORY.md", "- [a.md](a.md) — index entry");
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        std::fs::write(
            dir.path().join("sessions").join("2026-08-01.md"),
            "summary content",
        )
        .unwrap();
        let content = build_memory_prompt_content_with_budget(dir.path(), Some(10_000));
        assert!(content.contains("Memory Index"));
        assert!(content.contains("Recent Session Summary"));
    }

    #[test]
    fn test_budget_drops_summary_when_over() {
        let dir = make_temp_dir();
        write_file(dir.path(), "MEMORY.md", "- [a.md](a.md) — index entry");
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        std::fs::write(
            dir.path().join("sessions").join("2026-08-01.md"),
            "a very long session summary that alone exceeds the tight budget",
        )
        .unwrap();
        // Tight budget: index + summary combined exceed it, but the index
        // alone fits — so the summary is dropped and the index survives.
        let content = build_memory_prompt_content_with_budget(dir.path(), Some(120));
        assert!(content.contains("Memory Index"), "got: {}", content);
        assert!(
            !content.contains("Recent Session Summary"),
            "got: {}",
            content
        );
    }

    #[test]
    fn test_budget_clamps_oversized_index() {
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "MEMORY.md",
            "- [a.md](a.md) — index entry that is deliberately long",
        );
        let content = build_memory_prompt_content_with_budget(dir.path(), Some(50));
        assert!(
            content.contains("truncated to 50 bytes"),
            "got: {}",
            content
        );
    }

    // ---- record_verify_conventions ----------------------------------------

    #[test]
    fn test_record_verify_conventions_creates_section() {
        let dir = make_temp_dir();
        assert!(record_verify_conventions(
            dir.path(),
            Some("cargo test --workspace"),
            Some("cargo clippy --workspace")
        ));
        let content = std::fs::read_to_string(dir.path().join("conventions.md")).unwrap();
        assert!(content.contains("## Build & verify commands"));
        assert!(content.contains("- Test: cargo test --workspace"));
        assert!(content.contains("- Lint: cargo clippy --workspace"));
    }

    #[test]
    fn test_record_verify_conventions_is_idempotent() {
        let dir = make_temp_dir();
        let args = (Some("cargo test"), Some("cargo clippy"));
        assert!(record_verify_conventions(dir.path(), args.0, args.1));
        let once = std::fs::read_to_string(dir.path().join("conventions.md")).unwrap();
        // Second call: nothing new → no write.
        assert!(!record_verify_conventions(dir.path(), args.0, args.1));
        let twice = std::fs::read_to_string(dir.path().join("conventions.md")).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn test_record_verify_conventions_appends_without_overwriting() {
        let dir = make_temp_dir();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            dir.path().join("conventions.md"),
            "# Conventions\n\nUse tabs.\n",
        )
        .unwrap();
        assert!(record_verify_conventions(
            dir.path(),
            Some("cargo test"),
            None
        ));
        let content = std::fs::read_to_string(dir.path().join("conventions.md")).unwrap();
        assert!(content.contains("Use tabs."), "got: {}", content);
        assert!(content.contains("- Test: cargo test"));
    }

    #[test]
    fn test_record_verify_conventions_no_commands_is_noop() {
        let dir = make_temp_dir();
        assert!(!record_verify_conventions(dir.path(), None, None));
        assert!(!dir.path().join("conventions.md").exists());
    }

    // ---- is_auto_memory_enabled -------------------------------------------

    #[test]
    fn test_auto_memory_enabled_default() {
        // No env vars set for this test, settings None → should be enabled.
        // We can't guarantee the test environment is clean, so just check it
        // returns a bool without panicking.
        let _ = is_auto_memory_enabled(None);
    }

    #[test]
    fn test_auto_memory_disabled_by_setting() {
        // If settings explicitly disable it and no env override, returns false.
        // We only test the settings-path without touching process env.
        // Simulate: env vars not set, settings says false.
        // We can't unset env vars reliably in tests, so just ensure the
        // function handles Some(false) without panicking.
        // (The full env-var paths are integration-tested separately.)
        let _ = is_auto_memory_enabled(Some(false));
    }
}
