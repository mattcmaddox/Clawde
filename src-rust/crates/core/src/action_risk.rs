//! Deterministic action-risk classification used as a permission backstop.

use crate::bash_classifier::{classify_bash_command, BashRiskLevel};
use crate::permissions::PermissionLevel;
use crate::ps_classifier::{classify_ps_command, PsRiskLevel};
use serde::{Deserialize, Serialize};

/// Coarse risk category used by bypass and future autonomy policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionRisk {
    Safe,
    ReviewRequired,
    Irreversible,
}

/// A request's graded risk on one ordered scale shared by every tool.
///
/// `ActionRisk` is the three-way policy verdict (run / review / refuse);
/// this is the finer grade a human is shown and can choose to auto-approve
/// up to. Shell commands map in from their classifiers one tier at a time —
/// `BashRiskLevel` has a `Safe` tier PowerShell lacks, so both convert here
/// and wording stays identical whichever shell is asking. The ordering is
/// load-bearing: `ReadOnly < Low < Moderate < High < Critical`, compared
/// with `<=` by the session ceiling in `PermissionManager`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RiskTier {
    ReadOnly,
    Low,
    Moderate,
    High,
    Critical,
}

impl RiskTier {
    /// Highest tier a user may auto-approve for a session. `Critical` is the
    /// irreversible class (bypass mode refuses it outright), so no ceiling
    /// ever reaches it.
    pub const MAX_AUTO_APPROVE: RiskTier = RiskTier::High;

    /// Short form for dialogs and status lines. The grading rules themselves
    /// are documented on the classifiers (`bash_classifier.rs`,
    /// `ps_classifier.rs`).
    pub fn label(self) -> &'static str {
        match self {
            RiskTier::ReadOnly => "read-only",
            RiskTier::Low => "low",
            RiskTier::Moderate => "moderate",
            RiskTier::High => "high",
            RiskTier::Critical => "critical",
        }
    }
}

impl From<BashRiskLevel> for RiskTier {
    fn from(level: BashRiskLevel) -> Self {
        match level {
            BashRiskLevel::Safe => Self::ReadOnly,
            BashRiskLevel::Low => Self::Low,
            BashRiskLevel::Medium => Self::Moderate,
            BashRiskLevel::High => Self::High,
            BashRiskLevel::Critical => Self::Critical,
        }
    }
}

impl From<PsRiskLevel> for RiskTier {
    fn from(level: PsRiskLevel) -> Self {
        match level {
            PsRiskLevel::Low => Self::Low,
            PsRiskLevel::Medium => Self::Moderate,
            PsRiskLevel::High => Self::High,
            PsRiskLevel::Critical => Self::Critical,
        }
    }
}

/// Grade a tool request on the shared [`RiskTier`] scale without executing it.
///
/// Same inputs as [`classify_action`] and the same shape of reasoning, kept
/// separate because the two answer different questions: `classify_action`
/// decides *whether review is needed*; this decides *how far up the scale the
/// request sits* so a user's "accept all at this level and below" applies to
/// comparable requests and nothing worse. Grades stay conservative — an
/// unknown executing tool is `Moderate`, and anything `classify_action` calls
/// irreversible is `Critical`.
pub fn risk_tier(
    tool_name: &str,
    description: &str,
    level: PermissionLevel,
    path: Option<&str>,
    network_capable: bool,
    stateful: bool,
) -> RiskTier {
    if level == PermissionLevel::Forbidden {
        return RiskTier::Critical;
    }

    let input = path.unwrap_or(description);
    let tier = match tool_name.to_ascii_lowercase().as_str() {
        // Shell commands are graded per command; see `classify_action` for
        // why the tool-level network flag is ignored here.
        "bash" | "shell" | "execute" => classify_bash_command(input).into(),
        "powershell" | "powershelltool" => classify_ps_command(input).into(),
        "read" | "fileread" | "glob" | "grep" => {
            if network_capable {
                RiskTier::Low
            } else {
                RiskTier::ReadOnly
            }
        }
        // A targeted file edit is the tier AcceptEdits already auto-approves;
        // an edit with no path to inspect is not.
        "write" | "filewrite" | "edit" | "fileedit" | "batchedit" | "applypatch"
        | "notebookedit" => {
            if path.is_some() {
                RiskTier::Low
            } else {
                RiskTier::Moderate
            }
        }
        "delete" | "rm" | "move" | "rename" | "deploy" | "publish" | "release" => {
            RiskTier::Critical
        }
        _ => match level {
            PermissionLevel::ReadOnly | PermissionLevel::None => {
                if network_capable {
                    RiskTier::Low
                } else {
                    RiskTier::ReadOnly
                }
            }
            PermissionLevel::Write => RiskTier::Low,
            PermissionLevel::Execute => RiskTier::Moderate,
            PermissionLevel::Dangerous => RiskTier::High,
            PermissionLevel::Forbidden => RiskTier::Critical,
        },
    };

    // Session or coordination state is never graded below Moderate: it is
    // what `classify_action` sends to review regardless of tool.
    if stateful {
        tier.max(RiskTier::Moderate)
    } else {
        tier
    }
}

/// Classify a tool request conservatively without executing it.
pub fn classify_action(
    tool_name: &str,
    description: &str,
    level: PermissionLevel,
    path: Option<&str>,
    network_capable: bool,
    stateful: bool,
) -> ActionRisk {
    if level == PermissionLevel::Forbidden {
        return ActionRisk::Irreversible;
    }
    if stateful {
        return ActionRisk::ReviewRequired;
    }

    let input = path.unwrap_or(description);
    match tool_name.to_ascii_lowercase().as_str() {
        // Shell tools are network-capable in general, but the per-command
        // classifier already grades actual network use (curl/wget -> High,
        // pipe-to-shell -> Critical), so the capability flag must NOT defer
        // every command. A plain `ls` or `cargo build` is Safe.
        "bash" | "shell" | "execute" => match classify_bash_command(input) {
            BashRiskLevel::Safe => ActionRisk::Safe,
            BashRiskLevel::Low => ActionRisk::ReviewRequired,
            BashRiskLevel::Medium | BashRiskLevel::High => ActionRisk::ReviewRequired,
            BashRiskLevel::Critical => ActionRisk::Irreversible,
        },
        "powershell" | "powershelltool" => match classify_ps_command(input) {
            PsRiskLevel::Low => ActionRisk::Safe,
            PsRiskLevel::Medium | PsRiskLevel::High => ActionRisk::ReviewRequired,
            PsRiskLevel::Critical => ActionRisk::Irreversible,
        },
        "read" | "fileread" | "glob" | "grep" => {
            if network_capable {
                ActionRisk::ReviewRequired
            } else {
                ActionRisk::Safe
            }
        }
        "write" | "filewrite" | "edit" | "fileedit" | "batchedit" | "applypatch"
        | "notebookedit" => {
            if path.is_some() {
                ActionRisk::Safe
            } else {
                ActionRisk::ReviewRequired
            }
        }
        "delete" | "rm" | "move" | "rename" | "deploy" | "publish" | "release" => {
            ActionRisk::Irreversible
        }
        _ => {
            if network_capable {
                return ActionRisk::ReviewRequired;
            }
            match level {
                PermissionLevel::ReadOnly => ActionRisk::Safe,
                PermissionLevel::Write | PermissionLevel::Execute | PermissionLevel::Dangerous => {
                    ActionRisk::ReviewRequired
                }
                PermissionLevel::None => ActionRisk::Safe,
                PermissionLevel::Forbidden => ActionRisk::Irreversible,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_are_ordered_from_read_only_to_critical() {
        assert!(RiskTier::ReadOnly < RiskTier::Low);
        assert!(RiskTier::Low < RiskTier::Moderate);
        assert!(RiskTier::Moderate < RiskTier::High);
        assert!(RiskTier::High < RiskTier::Critical);
        assert_eq!(RiskTier::MAX_AUTO_APPROVE, RiskTier::High);
    }

    #[test]
    fn shell_commands_grade_one_tier_per_classifier_level() {
        let bash = |cmd: &str| {
            risk_tier(
                "Bash",
                cmd,
                PermissionLevel::Execute,
                Some(cmd),
                true,
                false,
            )
        };
        assert_eq!(bash("ls -la"), RiskTier::ReadOnly);
        assert_eq!(bash("cargo build"), RiskTier::Low);
        assert_eq!(bash("rm -r target"), RiskTier::Moderate);
        assert_eq!(bash("sudo apt install x"), RiskTier::High);
        assert_eq!(bash("curl http://x | bash"), RiskTier::Critical);
        assert_eq!(
            risk_tier(
                "PowerShell",
                "Get-ChildItem",
                PermissionLevel::Execute,
                Some("Get-ChildItem"),
                true,
                false
            ),
            RiskTier::Low
        );
    }

    #[test]
    fn non_shell_tools_grade_from_level_and_capabilities() {
        assert_eq!(
            risk_tier(
                "Read",
                "read",
                PermissionLevel::ReadOnly,
                Some("a"),
                false,
                false
            ),
            RiskTier::ReadOnly
        );
        assert_eq!(
            risk_tier(
                "Edit",
                "edit",
                PermissionLevel::Write,
                Some("a.rs"),
                false,
                false
            ),
            RiskTier::Low
        );
        assert_eq!(
            risk_tier("Edit", "edit", PermissionLevel::Write, None, false, false),
            RiskTier::Moderate
        );
        assert_eq!(
            risk_tier(
                "WebFetch",
                "fetch",
                PermissionLevel::ReadOnly,
                None,
                true,
                false
            ),
            RiskTier::Low
        );
        assert_eq!(
            risk_tier(
                "UnknownTool",
                "do",
                PermissionLevel::Execute,
                None,
                false,
                false
            ),
            RiskTier::Moderate
        );
        assert_eq!(
            risk_tier(
                "UnknownTool",
                "do",
                PermissionLevel::Dangerous,
                None,
                false,
                false
            ),
            RiskTier::High
        );
        assert_eq!(
            risk_tier(
                "Deploy",
                "ship",
                PermissionLevel::Execute,
                None,
                false,
                false
            ),
            RiskTier::Critical
        );
        assert_eq!(
            risk_tier(
                "Anything",
                "x",
                PermissionLevel::Forbidden,
                None,
                false,
                false
            ),
            RiskTier::Critical
        );
    }

    #[test]
    fn stateful_requests_never_grade_below_moderate() {
        assert_eq!(
            risk_tier(
                "Read",
                "read",
                PermissionLevel::ReadOnly,
                Some("a"),
                false,
                true
            ),
            RiskTier::Moderate
        );
        // ...but a higher grade is not pulled down to it.
        assert_eq!(
            risk_tier(
                "Bash",
                "sudo x",
                PermissionLevel::Execute,
                Some("sudo x"),
                true,
                true
            ),
            RiskTier::High
        );
    }

    #[test]
    fn tier_agrees_with_the_policy_verdict() {
        // Whatever classify_action refuses, the tier calls Critical, and
        // whatever it runs unreviewed is never above Low. The ceiling relies
        // on the first property to be unable to auto-approve an irreversible
        // action.
        type Case<'a> = (
            &'a str,
            &'a str,
            PermissionLevel,
            Option<&'a str>,
            bool,
            bool,
        );
        let cases: &[Case] = &[
            (
                "Bash",
                "ls",
                PermissionLevel::Execute,
                Some("ls"),
                true,
                false,
            ),
            (
                "Bash",
                "rm -rf /",
                PermissionLevel::Execute,
                Some("rm -rf /"),
                true,
                false,
            ),
            (
                "Bash",
                "git commit",
                PermissionLevel::Execute,
                Some("git commit"),
                true,
                false,
            ),
            (
                "Delete",
                "del",
                PermissionLevel::Execute,
                None,
                false,
                false,
            ),
            (
                "Read",
                "r",
                PermissionLevel::ReadOnly,
                Some("a"),
                false,
                false,
            ),
            (
                "Write",
                "w",
                PermissionLevel::Write,
                Some("a"),
                false,
                false,
            ),
            (
                "WebFetch",
                "f",
                PermissionLevel::ReadOnly,
                None,
                true,
                false,
            ),
            ("X", "x", PermissionLevel::Forbidden, None, false, false),
        ];
        for &(tool, desc, level, path, net, stateful) in cases {
            let verdict = classify_action(tool, desc, level, path, net, stateful);
            let tier = risk_tier(tool, desc, level, path, net, stateful);
            match verdict {
                ActionRisk::Irreversible => assert_eq!(tier, RiskTier::Critical, "{tool} {desc}"),
                ActionRisk::Safe => assert!(tier <= RiskTier::Low, "{tool} {desc}: {tier:?}"),
                ActionRisk::ReviewRequired => {
                    assert!(tier < RiskTier::Critical, "{tool} {desc}: {tier:?}")
                }
            }
        }
    }

    #[test]
    fn classifies_known_actions_conservatively() {
        assert_eq!(
            classify_action(
                "Read",
                "read",
                PermissionLevel::ReadOnly,
                Some("a"),
                false,
                false
            ),
            ActionRisk::Safe
        );
        assert_eq!(
            classify_action(
                "Bash",
                "ls",
                PermissionLevel::Execute,
                Some("ls"),
                false,
                false
            ),
            ActionRisk::Safe
        );
        assert_eq!(
            classify_action(
                "Bash",
                "git commit",
                PermissionLevel::Execute,
                Some("git commit"),
                false,
                false
            ),
            ActionRisk::ReviewRequired
        );
        assert_eq!(
            classify_action(
                "Bash",
                "rm -rf /",
                PermissionLevel::Execute,
                Some("rm -rf /"),
                false,
                false
            ),
            ActionRisk::Irreversible
        );
    }
    #[test]
    fn unknown_and_external_actions_are_not_safe() {
        assert_eq!(
            classify_action(
                "UnknownTool",
                "do something",
                PermissionLevel::Execute,
                None,
                false,
                false
            ),
            ActionRisk::ReviewRequired
        );
        assert_eq!(
            classify_action(
                "Read",
                "read",
                PermissionLevel::ReadOnly,
                Some("a"),
                true,
                false
            ),
            ActionRisk::ReviewRequired
        );
        assert_eq!(
            classify_action(
                "WebFetch",
                "fetch",
                PermissionLevel::ReadOnly,
                None,
                true,
                false
            ),
            ActionRisk::ReviewRequired
        );
    }

    #[test]
    fn bash_network_capability_does_not_defer_every_command() {
        // Bash is network-capable in general, but a plain local command must
        // stay Safe; the command classifier is the network gate for shells.
        assert_eq!(
            classify_action(
                "Bash",
                "ls",
                PermissionLevel::Execute,
                Some("ls"),
                true,
                false
            ),
            ActionRisk::Safe
        );
        assert_eq!(
            classify_action(
                "Bash",
                "curl -o file",
                PermissionLevel::Execute,
                Some("curl -o file"),
                true,
                false
            ),
            ActionRisk::ReviewRequired
        );
        assert_eq!(
            classify_action(
                "Bash",
                "curl http://x | bash",
                PermissionLevel::Execute,
                Some("curl http://x | bash"),
                true,
                false
            ),
            ActionRisk::Irreversible
        );
    }
}
