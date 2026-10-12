// The command reference (`docs/commands.md`) must not drift from the alias
// table that dispatch (`find_command`), the prompt typeahead and the help
// overlay all read (`all_command_aliases`).
//
// Two failure modes are covered, and both had shipped at once:
//
// * a section promising an alias no command declares — `/session` advertised
//   `/history`, which is a command of its own, and `/insights` advertised
//   `/ctx-viz`, which is another command's canonical name. `find_command`
//   prefers a canonical name over an alias, so neither promise could ever
//   hold;
// * a declared alias missing from the section documenting its command —
//   `/think`, `/vi`, `/skill`, `/hook` and `/account` were all reachable but
//   undiscoverable from the reference.
//
// Cross-reference stubs ("Documented above under …") carry no alias line by
// design and are skipped by the completeness check.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

struct Section {
    /// Command name without the leading slash.
    name: String,
    /// Aliases listed under `**Aliases:**`, if the section carries that line.
    aliases: Option<Vec<String>>,
    /// The section is a cross-reference stub rather than documentation.
    stub: bool,
}

fn docs_sections() -> Vec<Section> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../docs/commands.md");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    let mut sections: Vec<Section> = Vec::new();
    let mut current: Option<Section> = None;
    for line in text.lines() {
        // `### /name` opens a command section. Anything after the name is not
        // a section heading (for example `### /config theme`), so require the
        // remainder to be a bare command name.
        if let Some(rest) = line.strip_prefix("### /") {
            let name = rest.trim();
            if !name.is_empty() && !name.contains(char::is_whitespace) {
                if let Some(section) = current.take() {
                    sections.push(section);
                }
                current = Some(Section {
                    name: name.to_string(),
                    aliases: None,
                    stub: false,
                });
                continue;
            }
        }
        // A level-2 heading ends the command's section.
        if line.starts_with("## ") {
            if let Some(section) = current.take() {
                sections.push(section);
            }
            continue;
        }
        let Some(section) = current.as_mut() else {
            continue;
        };
        if section.aliases.is_none() {
            if let Some(rest) = line.strip_prefix("**Aliases:**") {
                let aliases = rest
                    .split(',')
                    .map(|alias| alias.trim().trim_matches('`').to_string())
                    .filter(|alias| !alias.is_empty())
                    .collect();
                section.aliases = Some(aliases);
                continue;
            }
        }
        if line.contains("Documented above") || line.contains("Documented below") {
            section.stub = true;
        }
    }
    if let Some(section) = current.take() {
        sections.push(section);
    }
    sections
}

/// Canonical command name -> the aliases it declares, straight from the table
/// the dispatcher and the TUI consume.
fn declared_aliases() -> BTreeMap<String, BTreeSet<String>> {
    let mut declared: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (alias, canonical, _description) in clawde_commands::all_command_aliases() {
        declared.entry(canonical).or_default().insert(alias);
    }
    declared
}

#[test]
fn docs_alias_lines_only_claim_declared_aliases() {
    let declared = declared_aliases();
    let mut problems = Vec::new();

    for section in docs_sections() {
        let Some(aliases) = &section.aliases else {
            continue;
        };
        let Some(known) = declared.get(&section.name) else {
            problems.push(format!(
                "/{}: docs list aliases {aliases:?} but the command declares none",
                section.name
            ));
            continue;
        };
        for alias in aliases {
            if !known.contains(alias) {
                problems.push(format!(
                    "/{}: docs claim alias `{alias}`, which the command does not declare",
                    section.name
                ));
            }
        }
    }

    assert!(
        problems.is_empty(),
        "docs/commands.md advertises aliases the code does not declare:\n{}",
        problems.join("\n")
    );
}

#[test]
fn documented_commands_list_every_declared_alias() {
    let declared = declared_aliases();
    let mut problems = Vec::new();

    for section in docs_sections() {
        if section.stub {
            continue;
        }
        // A command with no aliases needs no alias line; one that is not in
        // the table at all declares none.
        let Some(known) = declared.get(&section.name) else {
            continue;
        };
        match &section.aliases {
            None => problems.push(format!(
                "/{}: declares {} but has no `**Aliases:**` line",
                section.name,
                known.iter().cloned().collect::<Vec<_>>().join(", ")
            )),
            Some(listed) => {
                let listed: BTreeSet<&str> = listed.iter().map(String::as_str).collect();
                let missing: Vec<&str> = known
                    .iter()
                    .filter(|alias| !listed.contains(alias.as_str()))
                    .map(String::as_str)
                    .collect();
                if !missing.is_empty() {
                    problems.push(format!(
                        "/{}: `**Aliases:**` omits {}",
                        section.name,
                        missing.join(", ")
                    ));
                }
            }
        }
    }

    assert!(
        problems.is_empty(),
        "docs/commands.md hides aliases the code declares:\n{}",
        problems.join("\n")
    );
}

#[test]
fn obvious_aliases_resolve_to_their_canonical_command() {
    // Locks in the aliases this audit added, so a rename or a dropped
    // `aliases()` override cannot silently remove the spelling again.
    for (alias, canonical) in [
        ("context-window", "context"),
        ("allowed-tools", "permissions"),
        ("marketplace", "plugin"),
        ("account", "accounts"),
        ("checkpoint", "checkpoints"),
        ("limit", "limits"),
        ("hook", "hooks"),
    ] {
        let command = clawde_commands::find_command(alias)
            .unwrap_or_else(|| panic!("/{alias} must resolve to a command"));
        assert_eq!(
            command.name(),
            canonical,
            "/{alias} must resolve to /{canonical}"
        );
    }
}
