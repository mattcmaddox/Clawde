//! Durable progress state for an approved structured specification.
//!
//! `Spec` remains the immutable, user-approved plan. `PlanProgress` is the
//! separate execution record: it tracks checkable steps and bounded evidence
//! without changing the approved spec or authorizing a write by itself.

use crate::spec::Spec;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const PLAN_PROGRESS_SCHEMA_VERSION: u32 = 1;

const MAX_EVIDENCE_SUMMARY_CHARS: usize = 2_000;
const MAX_EVIDENCE_KIND_CHARS: usize = 64;

/// Consecutive real deterministic failures before the harness asks the next
/// turn to change approach (the replan signal).
pub const PLAN_FAILURE_REPLAN_THRESHOLD: u32 = 2;

/// Bounded replan budget: after this many replan signals on the same plan, the
/// harness fail-closes the plan as `Blocked` instead of letting the model
/// retry forever. The approved spec is never mutated; the user must approve a
/// new spec to continue.
pub const PLAN_MAX_REPLANS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Active,
    Complete,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    Pending,
    Active,
    Complete,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepPhase {
    Explore,
    Diagnose,
    Implement,
    Verify,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEvidence {
    /// Stable machine-readable source category, e.g. `diff`, `check`, or `tool`.
    pub kind: String,
    /// Bounded human-readable explanation of what was observed.
    pub summary: String,
    /// Optional repository-relative artifact reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub id: String,
    pub title: String,
    pub phase: PlanStepPhase,
    pub acceptance: Vec<String>,
    pub status: PlanStepStatus,
    #[serde(default)]
    pub evidence: Vec<PlanEvidence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanAdvanceEvidence {
    pub turn_made_writes: bool,
    pub has_scoped_diff: bool,
    pub deterministic_checks_run: bool,
    pub deterministic_passed: bool,
    /// A real executed check failed. Unavailable/skipped checks must not set this.
    #[serde(default)]
    pub deterministic_failed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTransition {
    pub completed_step_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_step_id: Option<String>,
    pub plan_status: PlanStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanProgressEvent {
    pub task_id: String,
    pub session_id: String,
    pub plan_status: PlanStatus,
    /// Harness-observed coordination phase; not a permission or acceptance gate.
    pub phase: PlanStepPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_step_id: Option<String>,
    /// Consecutive real deterministic failures on the active step.
    #[serde(default)]
    pub failure_streak: u32,
    /// The harness asks the next turn to change approach after bounded failures.
    #[serde(default)]
    pub replan_required: bool,
    /// Bounded replan budget counter; at [`PLAN_MAX_REPLANS`] the plan is
    /// fail-closed as `Blocked`.
    #[serde(default)]
    pub replan_count: u32,
    /// At most one prior completed step to revisit before retrying the active step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backtrack_target_step_id: Option<String>,
    pub evidence: PlanEvidence,
    pub persisted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition: Option<PlanTransition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanProgress {
    pub schema_version: u32,
    pub task_id: String,
    pub session_id: String,
    pub spec_path: String,
    pub spec_hash: String,
    pub status: PlanStatus,
    /// Harness-observed phase; it does not authorize tools or acceptance.
    #[serde(default = "default_plan_phase")]
    pub phase: PlanStepPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_step_id: Option<String>,
    #[serde(default)]
    pub failure_streak: u32,
    #[serde(default)]
    pub replan_required: bool,
    /// How many replan signals the active plan has emitted; the bounded budget
    /// that fail-closes the plan at [`PLAN_MAX_REPLANS`]. Cleared when a
    /// passing check or step transition ends the recovery cycle.
    #[serde(default)]
    pub replan_count: u32,
    /// A bounded recovery hint; this never changes step status by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backtrack_target_step_id: Option<String>,
    pub steps: Vec<PlanStep>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanStateError {
    InvalidIdentifier,
    InvalidSpecPath,
    MissingStep(String),
    InvalidTransition(String),
    MissingEvidence(String),
    EvidenceTooLarge,
    UnsafeEvidenceReference,
    Corrupt(String),
}

impl std::fmt::Display for PlanStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidIdentifier => write!(f, "plan identifier is invalid"),
            Self::InvalidSpecPath => write!(f, "approved spec path is invalid"),
            Self::MissingStep(id) => write!(f, "plan step '{id}' was not found"),
            Self::InvalidTransition(message) => write!(f, "invalid plan transition: {message}"),
            Self::MissingEvidence(id) => write!(f, "plan step '{id}' has no evidence"),
            Self::EvidenceTooLarge => write!(f, "plan evidence exceeds its size limit"),
            Self::UnsafeEvidenceReference => {
                write!(f, "plan evidence reference is not repository-relative")
            }
            Self::Corrupt(message) => write!(f, "corrupt plan progress: {message}"),
        }
    }
}

impl std::error::Error for PlanStateError {}

impl PlanProgress {
    /// Return the only valid on-disk location for a task's progress artifact.
    pub fn path_for(project_root: &Path, task_id: &str) -> Option<PathBuf> {
        valid_identifier(task_id)
            .then(|| project_root.join("plans").join(format!("{task_id}.json")))
    }

    /// Initialize and persist progress for an approved spec.
    pub fn initialize_for_spec(
        project_root: &Path,
        spec_path: &Path,
        raw_spec: &str,
        spec: &Spec,
        session_id: &str,
    ) -> std::io::Result<Self> {
        let progress = Self::from_spec(project_root, spec_path, raw_spec, spec, session_id)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        progress.save(project_root)?;
        Ok(progress)
    }

    fn from_spec(
        project_root: &Path,
        spec_path: &Path,
        raw_spec: &str,
        spec: &Spec,
        session_id: &str,
    ) -> Result<Self, PlanStateError> {
        if !valid_identifier(&spec.task_id) || session_id.trim().is_empty() {
            return Err(PlanStateError::InvalidIdentifier);
        }
        let canonical_root = project_root
            .canonicalize()
            .map_err(|_| PlanStateError::InvalidSpecPath)?;
        let canonical_spec = spec_path
            .canonicalize()
            .map_err(|_| PlanStateError::InvalidSpecPath)?;
        if !canonical_spec.starts_with(canonical_root.join("specs"))
            || canonical_spec.file_name().is_none()
        {
            return Err(PlanStateError::InvalidSpecPath);
        }
        if spec.session_id.as_deref() != Some(session_id) {
            return Err(PlanStateError::InvalidIdentifier);
        }

        let mut steps = Vec::new();
        if spec.requirements.is_empty() {
            steps.push(PlanStep {
                id: "implementation".to_string(),
                title: "Implement the approved task".to_string(),
                phase: PlanStepPhase::Implement,
                acceptance: vec![spec.task.clone()],
                status: PlanStepStatus::Active,
                evidence: Vec::new(),
            });
        } else {
            for (index, requirement) in spec.requirements.iter().enumerate() {
                steps.push(PlanStep {
                    id: format!("requirement-{}", index + 1),
                    title: format!("Satisfy requirement {}", index + 1),
                    phase: PlanStepPhase::Implement,
                    acceptance: vec![requirement.clone()],
                    status: if index == 0 {
                        PlanStepStatus::Active
                    } else {
                        PlanStepStatus::Pending
                    },
                    evidence: Vec::new(),
                });
            }
        }
        let acceptance = if spec.acceptance_tests.is_empty() {
            vec!["Deterministic verification and external acceptance checks pass.".to_string()]
        } else {
            spec.acceptance_tests
                .iter()
                .map(|test| test.description.clone())
                .collect()
        };
        steps.push(PlanStep {
            id: "verification".to_string(),
            title: "Verify the completed implementation".to_string(),
            phase: PlanStepPhase::Verify,
            acceptance,
            status: PlanStepStatus::Pending,
            evidence: Vec::new(),
        });

        let now = now_ms();
        Ok(Self {
            schema_version: PLAN_PROGRESS_SCHEMA_VERSION,
            task_id: spec.task_id.clone(),
            session_id: session_id.to_string(),
            spec_path: canonical_spec
                .strip_prefix(&canonical_root)
                .map_err(|_| PlanStateError::InvalidSpecPath)?
                .to_string_lossy()
                .replace('\\', "/"),
            spec_hash: Spec::content_hash(raw_spec),
            status: PlanStatus::Active,
            phase: PlanStepPhase::Explore,
            active_step_id: steps.first().map(|step| step.id.clone()),
            failure_streak: 0,
            replan_required: false,
            replan_count: 0,
            backtrack_target_step_id: None,
            steps,
            created_at_ms: now,
            updated_at_ms: now,
        })
    }

    /// Load a progress artifact only when its task, session, and approved spec
    /// hash all match. A corrupt artifact returns an error rather than resetting
    /// progress or authorizing a new write.
    pub fn load_for(
        project_root: &Path,
        task_id: &str,
        session_id: &str,
        spec_hash: &str,
    ) -> std::io::Result<Option<Self>> {
        let Some(path) = Self::path_for(project_root, task_id) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                PlanStateError::InvalidIdentifier,
            ));
        };
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let progress: Self = serde_json::from_str(&raw).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                PlanStateError::Corrupt(error.to_string()),
            )
        })?;
        progress
            .validate()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        if progress.task_id != task_id
            || progress.session_id != session_id
            || progress.spec_hash != spec_hash
        {
            return Ok(None);
        }
        Ok(Some(progress))
    }

    /// Append evidence to the plan bound to the current approved spec.
    ///
    /// This Phase B compatibility entry point never advances a step. Phase C
    /// callers use `record_evidence_and_advance_for_approved_spec` with
    /// structured harness evidence instead.
    pub fn record_evidence_for_approved_spec(
        project_root: &Path,
        task_id: &str,
        session_id: &str,
        evidence: PlanEvidence,
    ) -> std::io::Result<Option<PlanProgressEvent>> {
        Self::record_evidence_and_advance_for_approved_spec(
            project_root,
            task_id,
            session_id,
            evidence,
            PlanAdvanceEvidence::default(),
        )
    }

    /// Append turn evidence and advance at most one step using only structured
    /// harness observations. Model prose is not an input to this decision.
    pub fn record_evidence_and_advance_for_approved_spec(
        project_root: &Path,
        task_id: &str,
        session_id: &str,
        evidence: PlanEvidence,
        advance_evidence: PlanAdvanceEvidence,
    ) -> std::io::Result<Option<PlanProgressEvent>> {
        let Some((spec_path, spec)) = Spec::approved_in(project_root, session_id) else {
            return Ok(None);
        };
        if spec.task_id != task_id {
            return Ok(None);
        }
        let raw_spec = std::fs::read_to_string(&spec_path)?;
        let spec_hash = Spec::content_hash(&raw_spec);
        let mut progress = Self::load_for(project_root, task_id, session_id, &spec_hash)?
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "approved spec has no matching plan progress artifact",
                )
            })?;
        if let Err(error) = progress.record_evidence(evidence.clone()) {
            // Terminal plans (Blocked/Complete) cannot record more evidence.
            // Surface the durable terminal state as a persisted event instead
            // of a misleading "evidence not persisted" error on every turn.
            if progress.status != PlanStatus::Active {
                return Ok(Some(PlanProgressEvent {
                    task_id: progress.task_id.clone(),
                    session_id: progress.session_id.clone(),
                    plan_status: progress.status,
                    phase: progress.phase,
                    active_step_id: progress.active_step_id.clone(),
                    failure_streak: progress.failure_streak,
                    replan_required: progress.replan_required,
                    replan_count: progress.replan_count,
                    backtrack_target_step_id: progress.backtrack_target_step_id.clone(),
                    evidence,
                    persisted: true,
                    transition: None,
                    error: None,
                }));
            }
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error));
        }
        let transition = progress
            .coordinate_from_evidence(advance_evidence)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        progress.save(project_root)?;
        Ok(Some(PlanProgressEvent {
            task_id: progress.task_id.clone(),
            session_id: progress.session_id.clone(),
            plan_status: progress.status,
            phase: progress.phase,
            active_step_id: progress.active_step_id.clone(),
            failure_streak: progress.failure_streak,
            replan_required: progress.replan_required,
            replan_count: progress.replan_count,
            backtrack_target_step_id: progress.backtrack_target_step_id.clone(),
            evidence,
            persisted: true,
            transition,
            error: None,
        }))
    }

    /// Apply bounded failure handling, then advance one active step only when
    /// deterministic harness evidence is sufficient for that step's phase.
    /// Real failed checks raise a persisted streak; after the threshold the
    /// next turn must replan rather than blindly repeat the same approach.
    pub fn coordinate_from_evidence(
        &mut self,
        evidence: PlanAdvanceEvidence,
    ) -> Result<Option<PlanTransition>, PlanStateError> {
        // Terminal plans are immutable: no further failure handling, evidence
        // coordination, or replan accounting runs against them.
        if self.status == PlanStatus::Complete || self.status == PlanStatus::Blocked {
            return Ok(None);
        }
        if evidence.deterministic_failed {
            self.phase = PlanStepPhase::Diagnose;
        } else if evidence.deterministic_checks_run {
            self.phase = PlanStepPhase::Verify;
        } else if evidence.turn_made_writes || evidence.has_scoped_diff {
            self.phase = PlanStepPhase::Implement;
        }
        if evidence.deterministic_failed {
            self.failure_streak = self
                .failure_streak
                .saturating_add(1)
                .min(PLAN_FAILURE_REPLAN_THRESHOLD);
            self.replan_required = self.failure_streak >= PLAN_FAILURE_REPLAN_THRESHOLD;
            if self.replan_required {
                // The bounded replan budget is harness-owned: after
                // PLAN_MAX_REPLANS replan signals the plan fail-closes as
                // Blocked instead of retrying forever. The approved spec is
                // never mutated — the user must approve a new spec to
                // continue.
                self.replan_count = self.replan_count.saturating_add(1);
                self.backtrack_target_step_id = self.previous_completed_step_id();
                if self.replan_count >= PLAN_MAX_REPLANS {
                    self.block_for_replan_budget()?;
                    return Ok(None);
                }
            }
        }
        let transition = self.advance_from_evidence(evidence)?;
        if transition.is_some() {
            self.phase = self
                .active_step_id
                .as_deref()
                .and_then(|active_id| self.steps.iter().find(|step| step.id == active_id))
                .map(|step| step.phase)
                .unwrap_or(PlanStepPhase::Verify);
            self.failure_streak = 0;
            self.replan_required = false;
            self.replan_count = 0;
            self.backtrack_target_step_id = None;
        } else if evidence.deterministic_passed {
            self.failure_streak = 0;
            self.replan_required = false;
            self.replan_count = 0;
            self.backtrack_target_step_id = None;
        }
        Ok(transition)
    }

    /// Fail-closed bounded termination: mark the active step and the whole
    /// plan `Blocked` once the replan budget is exhausted. Evidence recorded
    /// earlier in the turn stays on the step; the approved spec is untouched.
    fn block_for_replan_budget(&mut self) -> Result<(), PlanStateError> {
        let step_id = self.active_step_id.clone().ok_or_else(|| {
            PlanStateError::InvalidTransition("the plan has no active step".to_string())
        })?;
        let step = self.step_mut(&step_id)?;
        if step.status != PlanStepStatus::Active {
            return Err(PlanStateError::InvalidTransition(format!(
                "active step '{step_id}' is not active"
            )));
        }
        step.status = PlanStepStatus::Blocked;
        self.active_step_id = None;
        self.status = PlanStatus::Blocked;
        self.updated_at_ms = now_ms();
        Ok(())
    }

    /// Advance one active step only when deterministic harness evidence is
    /// sufficient for that step's phase. Returns `None` when the evidence is
    /// insufficient or the plan is already terminal.
    pub fn advance_from_evidence(
        &mut self,
        evidence: PlanAdvanceEvidence,
    ) -> Result<Option<PlanTransition>, PlanStateError> {
        let Some(step_id) = self.active_step_id.clone() else {
            return Ok(None);
        };
        let step = self
            .steps
            .iter()
            .find(|step| step.id == step_id)
            .ok_or_else(|| PlanStateError::MissingStep(step_id.clone()))?;
        let ready = match step.phase {
            PlanStepPhase::Verify => {
                evidence.deterministic_checks_run && evidence.deterministic_passed
            }
            PlanStepPhase::Explore | PlanStepPhase::Diagnose | PlanStepPhase::Implement => {
                evidence.turn_made_writes
                    && evidence.has_scoped_diff
                    && evidence.deterministic_checks_run
                    && evidence.deterministic_passed
            }
        };
        if !ready {
            return Ok(None);
        }
        let completed_step_id = step_id.clone();
        self.complete_active_step()?;
        Ok(Some(PlanTransition {
            completed_step_id,
            active_step_id: self.active_step_id.clone(),
            plan_status: self.status,
        }))
    }

    /// Persist progress atomically beneath `project_root/plans/`.
    pub fn save(&self, project_root: &Path) -> std::io::Result<()> {
        self.validate()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let Some(path) = Self::path_for(project_root, &self.task_id) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                PlanStateError::InvalidIdentifier,
            ));
        };
        let parent = path.parent().expect("plan path always has a parent");
        std::fs::create_dir_all(parent)?;
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        let temp = parent.join(format!(".{}.tmp-{}", self.task_id, std::process::id()));
        std::fs::write(&temp, bytes)?;
        if let Err(error) = std::fs::rename(&temp, &path) {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
        Ok(())
    }

    /// Add bounded evidence to the currently active step.
    pub fn record_evidence(&mut self, evidence: PlanEvidence) -> Result<(), PlanStateError> {
        validate_evidence(&evidence)?;
        let step_id = self.active_step_id.clone().ok_or_else(|| {
            PlanStateError::InvalidTransition("the plan has no active step".to_string())
        })?;
        let step = self.step_mut(&step_id)?;
        if step.status != PlanStepStatus::Active {
            return Err(PlanStateError::InvalidTransition(format!(
                "active step '{step_id}' is not active"
            )));
        }
        step.evidence.push(evidence);
        self.updated_at_ms = now_ms();
        Ok(())
    }

    /// Complete the active step only after at least one evidence record exists.
    /// The next pending step becomes active deterministically.
    pub fn complete_active_step(&mut self) -> Result<(), PlanStateError> {
        let step_id = self.active_step_id.clone().ok_or_else(|| {
            PlanStateError::InvalidTransition("the plan has no active step".to_string())
        })?;
        let step = self.step_mut(&step_id)?;
        if step.status != PlanStepStatus::Active {
            return Err(PlanStateError::InvalidTransition(format!(
                "step '{step_id}' is not active"
            )));
        }
        if step.evidence.is_empty() {
            return Err(PlanStateError::MissingEvidence(step_id));
        }
        step.status = PlanStepStatus::Complete;
        if let Some(next) = self
            .steps
            .iter_mut()
            .find(|candidate| candidate.status == PlanStepStatus::Pending)
        {
            next.status = PlanStepStatus::Active;
            self.active_step_id = Some(next.id.clone());
        } else {
            self.active_step_id = None;
            self.status = PlanStatus::Complete;
        }
        self.updated_at_ms = now_ms();
        Ok(())
    }

    /// Mark the active step and plan blocked without deleting its evidence.
    pub fn block_active_step(&mut self, evidence: PlanEvidence) -> Result<(), PlanStateError> {
        validate_evidence(&evidence)?;
        let step_id = self.active_step_id.clone().ok_or_else(|| {
            PlanStateError::InvalidTransition("the plan has no active step".to_string())
        })?;
        let step = self.step_mut(&step_id)?;
        if step.status != PlanStepStatus::Active {
            return Err(PlanStateError::InvalidTransition(format!(
                "step '{step_id}' is not active"
            )));
        }
        step.evidence.push(evidence);
        step.status = PlanStepStatus::Blocked;
        self.active_step_id = None;
        self.status = PlanStatus::Blocked;
        self.updated_at_ms = now_ms();
        Ok(())
    }

    fn previous_completed_step_id(&self) -> Option<String> {
        let active_id = self.active_step_id.as_deref()?;
        let active_index = self.steps.iter().position(|step| step.id == active_id)?;
        self.steps[..active_index]
            .iter()
            .rev()
            .find(|step| step.status == PlanStepStatus::Complete)
            .map(|step| step.id.clone())
    }

    fn step_mut(&mut self, id: &str) -> Result<&mut PlanStep, PlanStateError> {
        self.steps
            .iter_mut()
            .find(|step| step.id == id)
            .ok_or_else(|| PlanStateError::MissingStep(id.to_string()))
    }

    fn validate(&self) -> Result<(), PlanStateError> {
        let spec_path = Path::new(&self.spec_path);
        let valid_spec_path = !self.spec_path.trim().is_empty()
            && !spec_path.is_absolute()
            && !spec_path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
            && matches!(
                spec_path.components().next(),
                Some(Component::Normal(component)) if component == "specs"
            );
        if self.schema_version != PLAN_PROGRESS_SCHEMA_VERSION
            || !valid_identifier(&self.task_id)
            || self.session_id.trim().is_empty()
            || self.spec_hash.trim().is_empty()
            || !valid_spec_path
            || self.steps.is_empty()
        {
            return Err(PlanStateError::Corrupt("invalid metadata".to_string()));
        }
        let mut ids = std::collections::HashSet::new();
        let mut active_ids = Vec::new();
        for step in &self.steps {
            if step.id.trim().is_empty() || !ids.insert(step.id.clone()) {
                return Err(PlanStateError::Corrupt(
                    "duplicate or empty step ID".to_string(),
                ));
            }
            for evidence in &step.evidence {
                validate_evidence(evidence)?;
            }
            if step.status == PlanStepStatus::Active {
                active_ids.push(step.id.clone());
            }
        }
        match self.status {
            PlanStatus::Active
                if active_ids.len() != 1
                    || self.active_step_id.as_ref() != active_ids.first()
                    || self.failure_streak > PLAN_FAILURE_REPLAN_THRESHOLD
                    || (!self.replan_required
                        && self.failure_streak >= PLAN_FAILURE_REPLAN_THRESHOLD)
                    || (self.replan_required
                        && self.failure_streak < PLAN_FAILURE_REPLAN_THRESHOLD)
                    || self
                        .backtrack_target_step_id
                        .as_ref()
                        .is_some_and(|target| {
                            self.active_step_id.as_deref() == Some(target.as_str())
                                || !self.steps.iter().any(|step| {
                                    step.id == *target && step.status == PlanStepStatus::Complete
                                })
                        }) =>
            {
                Err(PlanStateError::Corrupt(
                    "active plan has inconsistent active step".to_string(),
                ))
            }
            PlanStatus::Complete | PlanStatus::Blocked
                if self.active_step_id.is_some() || !active_ids.is_empty() =>
            {
                Err(PlanStateError::Corrupt(
                    "terminal plan still has an active step".to_string(),
                ))
            }
            _ => Ok(()),
        }
    }
}

fn default_plan_phase() -> PlanStepPhase {
    PlanStepPhase::Explore
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn validate_evidence(evidence: &PlanEvidence) -> Result<(), PlanStateError> {
    if evidence.kind.trim().is_empty()
        || evidence.kind.chars().count() > MAX_EVIDENCE_KIND_CHARS
        || evidence.summary.trim().is_empty()
        || evidence.summary.chars().count() > MAX_EVIDENCE_SUMMARY_CHARS
    {
        return Err(PlanStateError::EvidenceTooLarge);
    }
    if let Some(reference) = &evidence.reference {
        let path = Path::new(reference);
        if path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(PlanStateError::UnsafeEvidenceReference);
        }
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{AcceptanceTest, Spec};

    fn sample_spec(session_id: &str) -> Spec {
        Spec {
            task_id: "task-plan-progress".to_string(),
            task: "Implement a durable plan".to_string(),
            session_id: Some(session_id.to_string()),
            title: "Durable plan".to_string(),
            requirements: vec![
                "Persist progress safely".to_string(),
                "Record evidence".to_string(),
            ],
            acceptance_tests: vec![AcceptanceTest {
                description: "The progress file round-trips".to_string(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn initializes_checkable_steps_and_persists_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("specs/task.json");
        let spec = sample_spec("session-one");
        spec.write_to(&spec_path).unwrap();
        let raw = std::fs::read_to_string(&spec_path).unwrap();
        let progress =
            PlanProgress::initialize_for_spec(dir.path(), &spec_path, &raw, &spec, "session-one")
                .unwrap();

        assert_eq!(progress.status, PlanStatus::Active);
        assert_eq!(progress.active_step_id.as_deref(), Some("requirement-1"));
        assert_eq!(progress.phase, PlanStepPhase::Explore);
        assert_eq!(progress.failure_streak, 0);
        assert!(!progress.replan_required);
        assert_eq!(progress.backtrack_target_step_id, None);
        assert_eq!(progress.steps.len(), 3);
        assert!(PlanProgress::path_for(dir.path(), &spec.task_id)
            .unwrap()
            .is_file());
        let loaded = PlanProgress::load_for(
            dir.path(),
            &spec.task_id,
            "session-one",
            &progress.spec_hash,
        )
        .unwrap()
        .unwrap();
        assert_eq!(loaded, progress);
    }

    #[test]
    fn approval_hash_and_session_bind_progress_loading() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("specs/task.json");
        let spec = sample_spec("session-one");
        spec.write_to(&spec_path).unwrap();
        let raw = std::fs::read_to_string(&spec_path).unwrap();
        let progress =
            PlanProgress::initialize_for_spec(dir.path(), &spec_path, &raw, &spec, "session-one")
                .unwrap();

        assert!(
            PlanProgress::load_for(dir.path(), &spec.task_id, "other", &progress.spec_hash)
                .unwrap()
                .is_none()
        );
        assert!(
            PlanProgress::load_for(dir.path(), &spec.task_id, "session-one", "stale")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn approved_plan_records_turn_evidence_without_advancing_step() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("specs/task.json");
        let spec = sample_spec("session-one");
        spec.write_to(&spec_path).unwrap();
        Spec::write_approval_for_session(&spec_path, "session-one").unwrap();

        let event = PlanProgress::record_evidence_for_approved_spec(
            dir.path(),
            &spec.task_id,
            "session-one",
            PlanEvidence {
                kind: "turn".to_string(),
                summary: "Turn wrote src/lib.rs and stopped after checks.".to_string(),
                reference: Some("src/lib.rs".to_string()),
            },
        )
        .unwrap()
        .unwrap();

        assert!(event.persisted);
        assert_eq!(event.active_step_id.as_deref(), Some("requirement-1"));
        let raw = std::fs::read_to_string(&spec_path).unwrap();
        let progress = PlanProgress::load_for(
            dir.path(),
            &spec.task_id,
            "session-one",
            &Spec::content_hash(&raw),
        )
        .unwrap()
        .unwrap();
        assert_eq!(progress.steps[0].evidence.len(), 1);
        assert_eq!(progress.steps[0].status, PlanStepStatus::Active);
    }

    #[test]
    fn approved_plan_transition_is_persisted_with_the_event() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("specs/task.json");
        let spec = sample_spec("session-one");
        spec.write_to(&spec_path).unwrap();
        Spec::write_approval_for_session(&spec_path, "session-one").unwrap();

        let event = PlanProgress::record_evidence_and_advance_for_approved_spec(
            dir.path(),
            &spec.task_id,
            "session-one",
            PlanEvidence {
                kind: "turn".to_string(),
                summary: "A scoped implementation write passed deterministic checks.".to_string(),
                reference: Some("src/lib.rs".to_string()),
            },
            PlanAdvanceEvidence {
                turn_made_writes: true,
                has_scoped_diff: true,
                deterministic_checks_run: true,
                deterministic_passed: true,
                deterministic_failed: false,
            },
        )
        .unwrap()
        .unwrap();

        let transition = event.transition.unwrap();
        assert_eq!(transition.completed_step_id, "requirement-1");
        assert_eq!(event.active_step_id.as_deref(), Some("requirement-2"));
        assert_eq!(event.plan_status, PlanStatus::Active);

        for _ in 0..PLAN_FAILURE_REPLAN_THRESHOLD {
            let recovery_event = PlanProgress::record_evidence_and_advance_for_approved_spec(
                dir.path(),
                &spec.task_id,
                "session-one",
                PlanEvidence {
                    kind: "check".to_string(),
                    summary: "The next requirement failed its deterministic check.".to_string(),
                    reference: Some("src/lib.rs".to_string()),
                },
                PlanAdvanceEvidence {
                    deterministic_checks_run: true,
                    deterministic_failed: true,
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            if recovery_event.replan_required {
                assert_eq!(
                    recovery_event.backtrack_target_step_id.as_deref(),
                    Some("requirement-1")
                );
            }
        }
    }

    #[test]
    fn transitions_require_evidence_and_advance_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("specs/task.json");
        let spec = sample_spec("session-one");
        spec.write_to(&spec_path).unwrap();
        let raw = std::fs::read_to_string(&spec_path).unwrap();
        let mut progress =
            PlanProgress::initialize_for_spec(dir.path(), &spec_path, &raw, &spec, "session-one")
                .unwrap();

        assert!(matches!(
            progress.complete_active_step(),
            Err(PlanStateError::MissingEvidence(_))
        ));
        progress
            .record_evidence(PlanEvidence {
                kind: "diff".to_string(),
                summary: "Changed the implementation file.".to_string(),
                reference: Some("src/lib.rs".to_string()),
            })
            .unwrap();
        progress.complete_active_step().unwrap();
        assert_eq!(progress.active_step_id.as_deref(), Some("requirement-2"));
        assert_eq!(progress.steps[0].status, PlanStepStatus::Complete);
    }

    #[test]
    fn advance_requires_structured_passing_harness_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("specs/task.json");
        let spec = sample_spec("session-one");
        spec.write_to(&spec_path).unwrap();
        let raw = std::fs::read_to_string(&spec_path).unwrap();
        let mut progress =
            PlanProgress::initialize_for_spec(dir.path(), &spec_path, &raw, &spec, "session-one")
                .unwrap();

        assert!(progress
            .advance_from_evidence(PlanAdvanceEvidence {
                turn_made_writes: true,
                has_scoped_diff: true,
                deterministic_checks_run: false,
                deterministic_passed: false,
                deterministic_failed: false,
            })
            .unwrap()
            .is_none());
        progress
            .record_evidence(PlanEvidence {
                kind: "turn".to_string(),
                summary: "Harness observed a scoped write and passing checks.".to_string(),
                reference: Some("src/lib.rs".to_string()),
            })
            .unwrap();
        let transition = progress
            .coordinate_from_evidence(PlanAdvanceEvidence {
                turn_made_writes: true,
                has_scoped_diff: true,
                deterministic_checks_run: true,
                deterministic_passed: true,
                deterministic_failed: false,
            })
            .unwrap()
            .unwrap();

        assert_eq!(transition.completed_step_id, "requirement-1");
        assert_eq!(progress.active_step_id.as_deref(), Some("requirement-2"));
        assert_eq!(progress.steps[0].status, PlanStepStatus::Complete);
    }

    #[test]
    fn repeated_real_failures_require_replanning_and_success_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("specs/task.json");
        let spec = sample_spec("session-one");
        spec.write_to(&spec_path).unwrap();
        let raw = std::fs::read_to_string(&spec_path).unwrap();
        let mut progress =
            PlanProgress::initialize_for_spec(dir.path(), &spec_path, &raw, &spec, "session-one")
                .unwrap();

        let failed = PlanAdvanceEvidence {
            deterministic_checks_run: true,
            deterministic_failed: true,
            ..Default::default()
        };
        assert!(progress.coordinate_from_evidence(failed).unwrap().is_none());
        assert_eq!(progress.phase, PlanStepPhase::Diagnose);
        assert_eq!(progress.failure_streak, 1);
        assert!(!progress.replan_required);
        assert!(progress.coordinate_from_evidence(failed).unwrap().is_none());
        assert_eq!(progress.failure_streak, PLAN_FAILURE_REPLAN_THRESHOLD);
        assert!(progress.replan_required);

        progress
            .record_evidence(PlanEvidence {
                kind: "check".to_string(),
                summary: "A revised implementation passed deterministic checks.".to_string(),
                reference: Some("src/lib.rs".to_string()),
            })
            .unwrap();
        let passed = PlanAdvanceEvidence {
            turn_made_writes: true,
            has_scoped_diff: true,
            deterministic_checks_run: true,
            deterministic_passed: true,
            deterministic_failed: false,
        };
        let transition = progress.coordinate_from_evidence(passed).unwrap().unwrap();
        assert_eq!(transition.completed_step_id, "requirement-1");
        assert_eq!(progress.phase, PlanStepPhase::Implement);
        assert_eq!(progress.failure_streak, 0);
        assert!(!progress.replan_required);
        assert_eq!(progress.backtrack_target_step_id, None);

        for _ in 0..PLAN_FAILURE_REPLAN_THRESHOLD {
            progress
                .record_evidence(PlanEvidence {
                    kind: "check".to_string(),
                    summary: "The next step failed its deterministic check.".to_string(),
                    reference: Some("src/lib.rs".to_string()),
                })
                .unwrap();
            progress
                .coordinate_from_evidence(PlanAdvanceEvidence {
                    deterministic_checks_run: true,
                    deterministic_failed: true,
                    ..Default::default()
                })
                .unwrap();
        }
        assert_eq!(
            progress.backtrack_target_step_id.as_deref(),
            Some("requirement-1")
        );
    }

    #[test]
    fn blocking_active_step_preserves_evidence_and_terminal_state() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("specs/task.json");
        let spec = sample_spec("session-one");
        spec.write_to(&spec_path).unwrap();
        let raw = std::fs::read_to_string(&spec_path).unwrap();
        let mut progress =
            PlanProgress::initialize_for_spec(dir.path(), &spec_path, &raw, &spec, "session-one")
                .unwrap();

        progress
            .block_active_step(PlanEvidence {
                kind: "blocked".to_string(),
                summary: "The required dependency is unavailable.".to_string(),
                reference: Some("evidence/dependency.txt".to_string()),
            })
            .unwrap();

        assert_eq!(progress.status, PlanStatus::Blocked);
        assert_eq!(progress.active_step_id, None);
        assert_eq!(progress.steps[0].status, PlanStepStatus::Blocked);
        assert_eq!(progress.steps[0].evidence.len(), 1);
    }

    #[test]
    fn replan_budget_exhaustion_fail_closes_plan() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("specs/task.json");
        let spec = sample_spec("session-one");
        spec.write_to(&spec_path).unwrap();
        let raw = std::fs::read_to_string(&spec_path).unwrap();
        let mut progress =
            PlanProgress::initialize_for_spec(dir.path(), &spec_path, &raw, &spec, "session-one")
                .unwrap();

        let failed = PlanAdvanceEvidence {
            deterministic_checks_run: true,
            deterministic_failed: true,
            ..Default::default()
        };
        // Threshold failures raise the streak and emit the first replan signal.
        for _ in 0..PLAN_FAILURE_REPLAN_THRESHOLD {
            assert!(progress.coordinate_from_evidence(failed).unwrap().is_none());
        }
        assert!(progress.replan_required);
        assert_eq!(progress.replan_count, 1);
        assert_eq!(progress.status, PlanStatus::Active);

        // Each further replan signal burns the bounded budget until it closes.
        for _ in 0..(PLAN_MAX_REPLANS - 1) {
            assert!(progress.coordinate_from_evidence(failed).unwrap().is_none());
        }
        assert_eq!(progress.replan_count, PLAN_MAX_REPLANS);
        assert_eq!(progress.status, PlanStatus::Blocked);
        assert_eq!(progress.active_step_id, None);
        assert_eq!(progress.steps[0].status, PlanStepStatus::Blocked);

        // Terminal plans stay immutable no-ops for further evidence.
        assert!(progress.coordinate_from_evidence(failed).unwrap().is_none());
        assert_eq!(progress.status, PlanStatus::Blocked);
        assert_eq!(progress.replan_count, PLAN_MAX_REPLANS);
    }

    #[test]
    fn replan_budget_clears_when_recovery_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("specs/task.json");
        let spec = sample_spec("session-one");
        spec.write_to(&spec_path).unwrap();
        let raw = std::fs::read_to_string(&spec_path).unwrap();
        let mut progress =
            PlanProgress::initialize_for_spec(dir.path(), &spec_path, &raw, &spec, "session-one")
                .unwrap();

        let failed = PlanAdvanceEvidence {
            deterministic_checks_run: true,
            deterministic_failed: true,
            ..Default::default()
        };
        for _ in 0..PLAN_FAILURE_REPLAN_THRESHOLD {
            assert!(progress.coordinate_from_evidence(failed).unwrap().is_none());
        }
        assert!(progress.replan_required);
        assert_eq!(progress.replan_count, 1);

        // A real recovery (writes + diff + passing checks) ends the cycle and
        // resets the whole budget, so a later failure cycle starts fresh.
        progress
            .record_evidence(PlanEvidence {
                kind: "check".to_string(),
                summary: "A revised implementation passed deterministic checks.".to_string(),
                reference: Some("src/lib.rs".to_string()),
            })
            .unwrap();
        let passed = PlanAdvanceEvidence {
            turn_made_writes: true,
            has_scoped_diff: true,
            deterministic_checks_run: true,
            deterministic_passed: true,
            deterministic_failed: false,
        };
        let transition = progress.coordinate_from_evidence(passed).unwrap().unwrap();
        assert_eq!(transition.completed_step_id, "requirement-1");
        assert_eq!(progress.replan_count, 0);
        assert!(!progress.replan_required);
        assert_eq!(progress.failure_streak, 0);
        assert_eq!(progress.backtrack_target_step_id, None);
    }

    #[test]
    fn terminal_plan_emits_persisted_event_on_further_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("specs/task.json");
        let spec = sample_spec("session-one");
        spec.write_to(&spec_path).unwrap();
        Spec::write_approval_for_session(&spec_path, "session-one").unwrap();
        let raw = std::fs::read_to_string(&spec_path).unwrap();
        let mut progress =
            PlanProgress::initialize_for_spec(dir.path(), &spec_path, &raw, &spec, "session-one")
                .unwrap();
        progress
            .block_active_step(PlanEvidence {
                kind: "blocked".to_string(),
                summary: "Replan budget exhausted.".to_string(),
                reference: Some("evidence/blocked.txt".to_string()),
            })
            .unwrap();
        progress.save(dir.path()).unwrap();

        // A later turn's evidence against a terminal plan must surface the
        // durable blocked state as a persisted event, not a bogus
        // "not persisted" error.
        let event = PlanProgress::record_evidence_and_advance_for_approved_spec(
            dir.path(),
            "task-plan-progress",
            "session-one",
            PlanEvidence {
                kind: "turn".to_string(),
                summary: "Another turn attempted against the blocked plan.".to_string(),
                reference: Some("src/lib.rs".to_string()),
            },
            PlanAdvanceEvidence {
                deterministic_checks_run: true,
                deterministic_failed: true,
                ..Default::default()
            },
        )
        .unwrap()
        .expect("terminal plan still emits a persisted event");
        assert_eq!(event.plan_status, PlanStatus::Blocked);
        assert!(event.persisted);
        assert_eq!(event.error, None);
        assert_eq!(event.active_step_id, None);
        assert_eq!(event.replan_count, 0);
    }

    #[test]
    fn invalid_evidence_and_corrupt_state_fail_closed() {
        let evidence = PlanEvidence {
            kind: "diff".to_string(),
            summary: "safe".to_string(),
            reference: Some("../outside.patch".to_string()),
        };
        assert_eq!(
            validate_evidence(&evidence),
            Err(PlanStateError::UnsafeEvidenceReference)
        );
        assert!(PlanProgress::path_for(Path::new("/tmp"), "../escape").is_none());
    }
}
