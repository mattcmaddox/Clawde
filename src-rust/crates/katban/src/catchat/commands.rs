//! Guest slash commands for Cat Chat.
//!
//! Cat Chat's guest surface was chat-only: anything a friend typed starting
//! with `/` went to the model as prose, so `/help` asked the model a question
//! and `/model` was answered by the model instead of by the host. This module
//! is the guest command surface — a fixed vocabulary resolved *before* the
//! model call, whose replies are host-authored text (never model output, so
//! they can carry no reasoning).
//!
//! Two rules hold for every command, whatever the host enables:
//!
//! * A command never reaches the model and never costs a model call.
//! * A command can only change *this guest session* — its voice, effort, mode,
//!   model and history. Nothing here can touch the host's files, keys, other
//!   guests, or the board. `/katban` is the single command that reads host
//!   state: it is excluded from the default set, and when a host does enable
//!   it, it reports a guest-safe summary (board names, site counts — never a
//!   path and never admin state).
//!
//! The host decides the vocabulary and the models a guest may pick, via
//! `catchat serve --commands` / `--models` (or [`COMMANDS_ENV`] /
//! [`MODELS_ENV`] for an always-on unit). A guest's own text can never widen
//! either list.

use super::engine::{GuestSession, GuestVoice};
use clawde_core::effort::EffortLevel;
use clawde_core::modes::builtin_modes;

/// Host allowlist for the guest command surface, e.g. `help,model,new`.
/// `all` and `none` are accepted spellings; an empty value means "unset".
pub const COMMANDS_ENV: &str = "CLAWDE_CATCHAT_COMMANDS";
/// Host list of models a guest may pick (`/model <id>`), first entry = default.
pub const MODELS_ENV: &str = "CLAWDE_CATCHAT_MODELS";

/// The model guests ride when the host pins nothing: the free chain.
pub const DEFAULT_MODEL: &str = "free/auto";

/// Modes a guest may switch to. `walkaway` is deliberately absent: it exists to
/// auto-approve file edits, and a guest has no filesystem — offering it would
/// promise something the guest contract forbids.
pub const GUEST_MODES: [&str; 3] = ["default", "careful", "fast"];

/// Effort levels offered to guests, in ladder order. `ultracode` is left out:
/// it activates the coding delegation/verification workflow, which has nothing
/// to do in a chat with no tools.
pub const GUEST_EFFORT_LEVELS: [&str; 7] =
    ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// One command in the guest vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GuestCommand {
    Help,
    Model,
    Auto,
    Effort,
    Mode,
    New,
    History,
    Katban,
    Cathead,
    Caveman,
    Normal,
}

impl GuestCommand {
    /// Every command the surface knows, in the order `/help` lists them.
    pub const ALL: [GuestCommand; 11] = [
        GuestCommand::Help,
        GuestCommand::Model,
        GuestCommand::Auto,
        GuestCommand::Effort,
        GuestCommand::Mode,
        GuestCommand::New,
        GuestCommand::History,
        GuestCommand::Katban,
        GuestCommand::Cathead,
        GuestCommand::Caveman,
        GuestCommand::Normal,
    ];

    pub fn name(self) -> &'static str {
        match self {
            GuestCommand::Help => "help",
            GuestCommand::Model => "model",
            GuestCommand::Auto => "auto",
            GuestCommand::Effort => "effort",
            GuestCommand::Mode => "mode",
            GuestCommand::New => "new",
            GuestCommand::History => "history",
            GuestCommand::Katban => "katban",
            GuestCommand::Cathead => "cathead",
            GuestCommand::Caveman => "caveman",
            GuestCommand::Normal => "normal",
        }
    }

    /// One line for `/help` — written for a guest, not for an operator.
    pub fn description(self) -> &'static str {
        match self {
            GuestCommand::Help => "Show the commands this host offers",
            GuestCommand::Model => "Show or change the model for this chat",
            GuestCommand::Auto => "Go back to the host's default model chain",
            GuestCommand::Effort => "Show or set how hard the model thinks",
            GuestCommand::Mode => "Show or set the chat's mode (default, careful, fast)",
            GuestCommand::New => "Start a fresh chat in this session",
            GuestCommand::History => "List what you have asked in this chat",
            GuestCommand::Katban => "Show how many boards and dev sites the host runs",
            GuestCommand::Cathead => "Speak in the cat persona",
            GuestCommand::Caveman => "Speak in the caveman persona",
            GuestCommand::Normal => "Drop the persona and speak plainly",
        }
    }

    /// Resolve a typed command name (with or without its leading slash).
    pub fn parse(name: &str) -> Option<Self> {
        let lowered = name.trim().trim_start_matches('/').to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|command| command.name() == lowered)
    }
}

/// The set of commands a host allows guests to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestCommandSet {
    allowed: Vec<GuestCommand>,
}

impl GuestCommandSet {
    /// The default set: everything except `/katban`.
    ///
    /// `/katban` is the one command that reads host state, and a friend has no
    /// business enumerating the owner's projects, so reading it requires an
    /// explicit opt-in. Every other command only touches the guest's own
    /// session.
    pub fn guest_default() -> Self {
        Self {
            allowed: GuestCommand::ALL
                .into_iter()
                .filter(|command| *command != GuestCommand::Katban)
                .collect(),
        }
    }

    pub fn none() -> Self {
        Self {
            allowed: Vec::new(),
        }
    }

    pub fn all() -> Self {
        Self {
            allowed: GuestCommand::ALL.to_vec(),
        }
    }

    /// Parse a `--commands` spec: `all`, `none`, or a comma-separated list of
    /// command names. An empty spec means "unset" (the caller keeps its
    /// default), mirroring `--search-endpoint ''`.
    ///
    /// An unknown name is an error rather than a silent skip: a typo in an
    /// always-on unit's `--commands help,modle` would otherwise leave a guest
    /// surface the host believes it enabled.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Ok(Self::guest_default());
        }
        match spec.to_ascii_lowercase().as_str() {
            "all" => return Ok(Self::all()),
            "none" => return Ok(Self::none()),
            _ => {}
        }
        let mut allowed: Vec<GuestCommand> = Vec::new();
        for token in spec.split(',') {
            let token = token.trim().trim_start_matches('/');
            if token.is_empty() {
                continue;
            }
            let Some(command) = GuestCommand::parse(token) else {
                let valid: Vec<&str> = GuestCommand::ALL.iter().map(|c| c.name()).collect();
                return Err(format!(
                    "unknown guest command '{token}' — valid names: {}",
                    valid.join(", ")
                ));
            };
            if !allowed.contains(&command) {
                allowed.push(command);
            }
        }
        if allowed.is_empty() {
            return Err("guest commands spec listed no commands — use 'none' for that".to_string());
        }
        Ok(Self { allowed })
    }

    /// The `--commands` spec, falling back to [`COMMANDS_ENV`] and then to
    /// [`Self::guest_default`].
    pub fn resolve(spec: Option<&str>) -> Result<Self, String> {
        match spec.map(str::to_string).or_else(|| env_spec(COMMANDS_ENV)) {
            Some(spec) => Self::parse(&spec),
            None => Ok(Self::guest_default()),
        }
    }

    pub fn allows(&self, command: GuestCommand) -> bool {
        self.allowed.contains(&command)
    }

    /// Allowed command names, in vocabulary order.
    pub fn names(&self) -> Vec<&'static str> {
        GuestCommand::ALL
            .into_iter()
            .filter(|command| self.allows(*command))
            .map(GuestCommand::name)
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty()
    }
}

/// Everything the guest surface needs to know about the host's policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestOptions {
    pub commands: GuestCommandSet,
    /// Models a guest may pick. The first entry is the default; a single entry
    /// means guests cannot choose at all.
    pub models: Vec<String>,
}

impl GuestOptions {
    pub fn guest_default() -> Self {
        Self {
            commands: GuestCommandSet::guest_default(),
            models: vec![DEFAULT_MODEL.to_string()],
        }
    }

    /// Resolve both lists from the command line, then the environment, then the
    /// defaults. An empty value means "unset" at each step.
    pub fn resolve(commands: Option<&str>, models: Option<&str>) -> Result<Self, String> {
        let commands = GuestCommandSet::resolve(commands)?;
        let spec = models
            .map(str::to_string)
            .or_else(|| env_spec(MODELS_ENV))
            .unwrap_or_default();
        let mut resolved: Vec<String> = Vec::new();
        for token in spec.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            if token.chars().any(char::is_whitespace) {
                return Err(format!("model id '{token}' must not contain spaces"));
            }
            if !resolved.iter().any(|existing| existing == token) {
                resolved.push(token.to_string());
            }
        }
        if resolved.is_empty() {
            resolved.push(DEFAULT_MODEL.to_string());
        }
        Ok(Self {
            commands,
            models: resolved,
        })
    }

    /// The model guests ride unless they pick another.
    pub fn default_model(&self) -> &str {
        self.models
            .first()
            .map(String::as_str)
            .unwrap_or(DEFAULT_MODEL)
    }

    pub fn allows_model(&self, model: &str) -> bool {
        self.models.iter().any(|allowed| allowed == model)
    }

    /// True when the host published more than the default, i.e. guests may
    /// actually choose a model.
    pub fn models_are_choices(&self) -> bool {
        self.models.len() > 1
    }
}

fn env_spec(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

/// Host facts `/katban` may report to a guest: deliberately names and counts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuestBoardFacts {
    pub boards: Vec<String>,
    pub sites: usize,
    pub exposed: usize,
}

/// A command's answer, plus whether it invalidated the session summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandReply {
    pub text: String,
    /// `/new` cleared the transcript, so a cached summary describes a
    /// conversation that no longer exists.
    pub cleared_history: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// Answer the guest with this text — no model call.
    Reply(CommandReply),
    /// Not a command: send it to the model as an ordinary message.
    NotACommand,
}

/// Resolve `line` as a guest command.
///
/// A line is only a command when it looks like one: `/` followed by a run of
/// ASCII letters. Anything else (`/etc/hosts is missing`, a message that merely
/// starts with a slash) is left to the model, so a guest can still ask about
/// paths.
///
/// `board` is called only by `/katban`, so the host pays for reading board
/// state only when that command actually runs.
pub fn run(
    line: &str,
    session: &mut GuestSession,
    options: &GuestOptions,
    board: &dyn Fn() -> GuestBoardFacts,
) -> CommandOutcome {
    let trimmed = line.trim();
    let Some(rest) = trimmed.strip_prefix('/') else {
        return CommandOutcome::NotACommand;
    };
    let mut tokens = rest.split_whitespace();
    let Some(name) = tokens.next() else {
        return CommandOutcome::NotACommand;
    };
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphabetic()) {
        return CommandOutcome::NotACommand;
    }
    let args: Vec<&str> = tokens.collect();
    let Some(command) = GuestCommand::parse(name) else {
        return CommandOutcome::Reply(CommandReply {
            text: match options.commands.names().as_slice() {
                [] => format!(
                    "`/{name}` is not a command here, and this host has no guest commands enabled — \
                     just type your question."
                ),
                _ => format!(
                    "`/{name}` is not a command here. Type `/help` to see what this host offers."
                ),
            },
            cleared_history: false,
        });
    };
    if !options.commands.allows(command) {
        return CommandOutcome::Reply(CommandReply {
            text: if options.commands.is_empty() {
                format!(
                    "`/{name}` is not enabled, and this host has no guest commands enabled — just \
                     type your question."
                )
            } else {
                format!(
                    "`/{name}` is a host-only command and is not enabled here. Type `/help` for the \
                     commands that are."
                )
            },
            cleared_history: false,
        });
    }
    CommandOutcome::Reply(execute(command, &args, session, options, board))
}

fn execute(
    command: GuestCommand,
    args: &[&str],
    session: &mut GuestSession,
    options: &GuestOptions,
    board: &dyn Fn() -> GuestBoardFacts,
) -> CommandReply {
    let text = match command {
        GuestCommand::Help => help_text(options),
        GuestCommand::Model => model_text(args, session, options),
        GuestCommand::Auto => {
            session.model = None;
            format!("back on the host's default: `{}`", options.default_model())
        }
        GuestCommand::Effort => effort_text(args, session),
        GuestCommand::Mode => mode_text(args, session),
        GuestCommand::New => {
            session.clear_history();
            return CommandReply {
                text:
                    "fresh chat — this conversation's history is cleared (your link and settings \
                       stay)."
                        .to_string(),
                cleared_history: true,
            };
        }
        GuestCommand::History => history_text(session),
        GuestCommand::Katban => katban_text(board()),
        GuestCommand::Cathead => voice_text(session, GuestVoice::Cathead),
        GuestCommand::Caveman => voice_text(session, GuestVoice::Caveman),
        GuestCommand::Normal => voice_text(session, GuestVoice::Normal),
    };
    CommandReply {
        text,
        cleared_history: false,
    }
}

/// `/help` — the commands this host actually allows, each with its one-liner.
fn help_text(options: &GuestOptions) -> String {
    if options.commands.is_empty() {
        return "This host has no guest commands enabled — just type your question.".to_string();
    }
    let mut out = String::from("Cat Chat commands on this host:\n");
    for command in GuestCommand::ALL {
        if !options.commands.allows(command) {
            continue;
        }
        out.push_str(&format!(
            "  /{:<8} {}\n",
            command.name(),
            command.description()
        ));
    }
    out.push_str("\nAnything else you type goes to the model.");
    out
}

fn model_text(args: &[&str], session: &mut GuestSession, options: &GuestOptions) -> String {
    let default = options.default_model();
    match args.first() {
        None => {
            let current = session.model.clone().unwrap_or_else(|| default.to_string());
            if !options.models_are_choices() {
                return format!(
                    "this chat is served by the host's own model chain (`{current}`). Guests cannot \
                     pick a model here — the host decides which models are used."
                );
            }
            let mut out = format!(
                "current: `{current}` (host default: `{default}`)\n\nthis host \
                                   lets you pick:\n"
            );
            for model in &options.models {
                out.push_str(&format!("  {model}\n"));
            }
            out.push_str("\nUse `/model <name>`, or `/auto` for the host default.");
            out
        }
        Some(choice) => {
            if !options.allows_model(choice) {
                let allowed = options.models.join(", ");
                return format!("`{choice}` is not available here. This host offers: {allowed}");
            }
            session.model = Some(choice.to_string());
            format!("model set to `{choice}` for this chat (host default: `{default}`)")
        }
    }
}

fn effort_text(args: &[&str], session: &mut GuestSession) -> String {
    let ladder = GUEST_EFFORT_LEVELS.join(", ");
    match args.first() {
        None => {
            let current = session
                .effort
                .map(|effort| effort.as_str().to_string())
                .unwrap_or_else(|| "model default".to_string());
            format!("effort: {current}\n\nlevels: {ladder}\nSet it with `/effort <level>`.")
        }
        Some(level) => {
            match EffortLevel::from_str(level) {
                Some(EffortLevel::Ultracode) => {
                    format!("`ultracode` is a coding workflow, not a chat setting — pick one of: {ladder}")
                }
                Some(effort) => {
                    session.effort = Some(effort);
                    format!("effort set to `{}` for this chat", effort.as_str())
                }
                None => format!("unknown effort `{level}` — pick one of: {ladder}"),
            }
        }
    }
}

fn mode_text(args: &[&str], session: &mut GuestSession) -> String {
    let modes = builtin_modes();
    match args.first() {
        None => {
            let current = session
                .mode
                .clone()
                .unwrap_or_else(|| "default".to_string());
            let mut out = format!("mode: {current}\n");
            for mode in modes
                .iter()
                .filter(|mode| GUEST_MODES.contains(&mode.name.as_str()))
            {
                out.push_str(&format!("  {:<8} {}\n", mode.name, mode.description));
            }
            out.push_str("Set it with `/mode <name>`.");
            out
        }
        Some(name) => {
            let name = name.to_ascii_lowercase();
            if !GUEST_MODES.contains(&name.as_str()) {
                if modes.iter().any(|mode| mode.name == name) {
                    return format!(
                        "`{name}` is about editing files, and guests have no file access here — \
                         pick one of: {}",
                        GUEST_MODES.join(", ")
                    );
                }
                return format!(
                    "unknown mode `{name}` — pick one of: {}",
                    GUEST_MODES.join(", ")
                );
            }
            session.mode = Some(name.clone());
            format!("mode set to `{name}` for this chat")
        }
    }
}

fn history_text(session: &GuestSession) -> String {
    const MAX_LISTED: usize = 20;
    let prompts = session.prompts();
    if prompts.is_empty() {
        return "nothing yet — this is the first thing you have typed in this chat.".to_string();
    }
    let start = prompts.len().saturating_sub(MAX_LISTED);
    let mut out = format!(
        "your prompts in this chat ({}{}):\n",
        prompts.len(),
        if start > 0 {
            format!(", newest {MAX_LISTED}")
        } else {
            String::new()
        }
    );
    for (offset, prompt) in prompts[start..].iter().enumerate() {
        let mut line: String = prompt.chars().take(80).collect();
        if prompt.chars().count() > 80 {
            line.push('…');
        }
        line = line.replace('\n', " ");
        out.push_str(&format!("  {}. {line}\n", start + offset + 1));
    }
    // The guest's own prompts are the guest's own text, but a prompt can embed
    // anything they pasted; nothing here is host state.
    out.trim_end().to_string()
}

fn katban_text(facts: GuestBoardFacts) -> String {
    let boards = if facts.boards.is_empty() {
        "none yet".to_string()
    } else {
        facts.boards.join(", ")
    };
    format!(
        "boards: {boards}\ndev sites: {} hosted ({} exposed)\n\nboard control belongs to the host; \
         this is only a count.",
        facts.sites, facts.exposed
    )
}

fn voice_text(session: &mut GuestSession, voice: GuestVoice) -> String {
    session.voice = voice;
    match voice {
        GuestVoice::Normal => "voice: default — no persona".to_string(),
        other => {
            let style = match other {
                GuestVoice::Cathead => {
                    clawde_core::output_styles::OutputStyleDef::builtin_cathead()
                }
                _ => clawde_core::output_styles::OutputStyleDef::builtin_caveman(),
            };
            format!("voice: {} — {}", style.label, style.description)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `line` against a host that has no boards (so `/katban` answers
    /// "none yet") and return its text.
    fn reply(line: &str, session: &mut GuestSession, options: &GuestOptions) -> String {
        let board = || GuestBoardFacts::default();
        match run(line, session, options, &board) {
            CommandOutcome::Reply(reply) => reply.text,
            CommandOutcome::NotACommand => panic!("`{line}` was not treated as a command"),
        }
    }

    fn outcome(line: &str, session: &mut GuestSession, options: &GuestOptions) -> CommandOutcome {
        let board = || GuestBoardFacts::default();
        run(line, session, options, &board)
    }

    #[test]
    fn default_set_is_every_command_except_katban() {
        let set = GuestCommandSet::guest_default();
        assert!(!set.allows(GuestCommand::Katban));
        for command in GuestCommand::ALL {
            if command != GuestCommand::Katban {
                assert!(set.allows(command), "{} must be allowed", command.name());
            }
        }
        // `/help` names exactly the allowed set, so a hidden command cannot be
        // advertised and an advertised one cannot be missing.
        let names = set.names();
        assert_eq!(names.len(), GuestCommand::ALL.len() - 1);
        assert!(!names.contains(&"katban"));
    }

    #[test]
    fn command_spec_parses_all_none_lists_and_rejects_typos() {
        assert_eq!(
            GuestCommandSet::parse("all").unwrap(),
            GuestCommandSet::all()
        );
        assert_eq!(
            GuestCommandSet::parse("none").unwrap(),
            GuestCommandSet::none()
        );
        assert!(GuestCommandSet::parse("none").unwrap().is_empty());
        let some = GuestCommandSet::parse("/help, model ,,HELP").unwrap();
        assert_eq!(some.names(), vec!["help", "model"]);
        // An unknown name must fail loudly: a typo in an always-on unit would
        // otherwise leave a surface the host believes it enabled.
        let error = GuestCommandSet::parse("help,modle").unwrap_err();
        assert!(error.contains("modle"), "error: {error}");
        assert!(GuestCommandSet::parse(",").is_err());
        // An empty spec is "unset", not "none".
        assert_eq!(
            GuestCommandSet::parse("   ").unwrap(),
            GuestCommandSet::guest_default()
        );
    }

    #[test]
    fn model_list_defaults_to_the_free_chain_and_dedupes() {
        let default = GuestOptions::resolve(None, Some("")).unwrap();
        assert_eq!(default.default_model(), DEFAULT_MODEL);
        assert!(!default.models_are_choices());
        let published =
            GuestOptions::resolve(None, Some("free/auto, free/groq/x, free/auto")).unwrap();
        assert_eq!(published.models, vec!["free/auto", "free/groq/x"]);
        assert_eq!(published.default_model(), "free/auto");
        assert!(published.allows_model("free/groq/x"));
        assert!(!published.allows_model("free/zai/y"));
        assert!(GuestOptions::resolve(None, Some("free/auto, bad id")).is_err());
    }

    #[test]
    fn non_commands_go_to_the_model() {
        for line in [
            "hello",
            "/",
            "/etc/hosts is missing",
            "/help-me-please",
            "what is /model?",
        ] {
            let mut session = GuestSession::new();
            assert!(
                matches!(
                    outcome(line, &mut session, &GuestOptions::guest_default()),
                    CommandOutcome::NotACommand
                ),
                "`{line}` must reach the model"
            );
        }
    }

    #[test]
    fn unknown_and_denied_commands_answer_without_calling_the_model() {
        let options = GuestOptions::guest_default();
        let mut session = GuestSession::new();
        let unknown = reply("/frobnicate", &mut session, &options);
        assert!(
            unknown.contains("/frobnicate") && unknown.contains("/help"),
            "{unknown}"
        );

        // `/katban` is recognised but not enabled by default, and the refusal
        // says so instead of pretending the command does not exist.
        let katban = reply("/katban", &mut session, &options);
        assert!(katban.contains("host-only command"), "{katban}");
        assert!(katban.contains("/help"), "{katban}");

        let none = GuestOptions {
            commands: GuestCommandSet::none(),
            models: vec![DEFAULT_MODEL.to_string()],
        };
        let disabled = reply("/help", &mut session, &none);
        assert!(disabled.contains("no guest commands enabled"), "{disabled}");
        assert!(!reply("/frobnicate", &mut session, &none).contains("/help"));
    }

    #[test]
    fn help_lists_only_the_allowed_commands() {
        let options = GuestOptions::guest_default();
        let mut session = GuestSession::new();
        let help = reply("/help", &mut session, &options);
        assert!(
            help.contains("/help") && help.contains("/cathead"),
            "{help}"
        );
        assert!(
            !help.contains("/katban"),
            "a disabled command must not be advertised: {help}"
        );

        let narrow = GuestOptions {
            commands: GuestCommandSet::parse("help,new").unwrap(),
            models: vec![DEFAULT_MODEL.to_string()],
        };
        let help = reply("/help", &mut session, &narrow);
        assert!(help.contains("/new"));
        assert!(!help.contains("/cathead"));
    }

    #[test]
    fn model_is_read_only_until_the_host_publishes_choices() {
        let options = GuestOptions::guest_default();
        let mut session = GuestSession::new();
        let shown = reply("/model", &mut session, &options);
        assert!(shown.contains(DEFAULT_MODEL), "{shown}");
        assert!(shown.contains("Guests cannot pick a model"), "{shown}");
        let refused = reply("/model free/groq/x", &mut session, &options);
        assert!(refused.contains("not available here"), "{refused}");
        assert_eq!(session.model, None);

        let choices = GuestOptions::resolve(None, Some("free/auto,free/groq/x")).unwrap();
        let listed = reply("/model", &mut session, &choices);
        assert!(listed.contains("free/groq/x"), "{listed}");
        let picked = reply("/model free/groq/x", &mut session, &choices);
        assert!(picked.contains("model set to `free/groq/x`"), "{picked}");
        assert_eq!(session.model.as_deref(), Some("free/groq/x"));
        // `/auto` is the way back, and it is the only thing that clears the pin.
        let reset = reply("/auto", &mut session, &choices);
        assert!(reset.contains("host's default"), "{reset}");
        assert_eq!(session.model, None);
    }

    #[test]
    fn effort_and_mode_only_touch_this_session() {
        let options = GuestOptions::guest_default();
        let mut session = GuestSession::new();
        assert!(reply("/effort", &mut session, &options).contains("model default"));
        let set = reply("/effort high", &mut session, &options);
        assert!(set.contains("`high`"), "{set}");
        assert_eq!(session.effort, Some(EffortLevel::High));
        assert!(reply("/effort loud", &mut session, &options).contains("unknown effort"));
        // The coding workflow is refused by name rather than silently ignored.
        assert!(reply("/effort ultracode", &mut session, &options).contains("coding workflow"));
        assert_eq!(session.effort, Some(EffortLevel::High));

        let mode = reply("/mode careful", &mut session, &options);
        assert!(mode.contains("`careful`"), "{mode}");
        assert_eq!(session.mode.as_deref(), Some("careful"));
        // A mode that exists for the TUI but makes no sense without files is
        // refused with the reason, not "unknown".
        let walkaway = reply("/mode walkaway", &mut session, &options);
        assert!(walkaway.contains("no file access"), "{walkaway}");
        assert_eq!(session.mode.as_deref(), Some("careful"));
        assert!(reply("/mode sideways", &mut session, &options).contains("unknown mode"));
    }

    #[test]
    fn personas_swap_the_voice_and_normal_resets_it() {
        let options = GuestOptions::guest_default();
        let mut session = GuestSession::new();
        assert_eq!(session.voice, GuestVoice::Cathead);
        let caveman = reply("/caveman", &mut session, &options);
        assert!(caveman.contains("Caveman"), "{caveman}");
        assert_eq!(session.voice, GuestVoice::Caveman);
        let cathead = reply("/cathead", &mut session, &options);
        assert!(cathead.contains("Cathead"), "{cathead}");
        assert_eq!(session.voice, GuestVoice::Cathead);
        let normal = reply("/normal", &mut session, &options);
        assert!(normal.contains("no persona"), "{normal}");
        assert_eq!(session.voice, GuestVoice::Normal);
    }

    #[test]
    fn new_clears_the_transcript_and_history_lists_the_prompts() {
        let options = GuestOptions::guest_default();
        let mut session = GuestSession::new();
        session
            .messages
            .push(clawde_core::types::Message::user("first question"));
        session
            .messages
            .push(clawde_core::types::Message::user("second question"));
        session.notes = "scratch".to_string();

        let listed = reply("/history", &mut session, &options);
        assert!(listed.contains("1. first question"), "{listed}");
        assert!(listed.contains("2. second question"), "{listed}");

        let fresh = match outcome("/new", &mut session, &options) {
            CommandOutcome::Reply(reply) => reply,
            CommandOutcome::NotACommand => panic!("/new must be a command"),
        };
        assert!(
            fresh.cleared_history,
            "/new must invalidate the cached summary"
        );
        assert!(session.messages.is_empty());
        assert!(session.notes.is_empty());
        assert!(reply("/history", &mut session, &options).contains("nothing yet"));
    }

    #[test]
    fn katban_reports_names_and_counts_only_when_the_host_enables_it() {
        let options = GuestOptions {
            commands: GuestCommandSet::parse("katban").unwrap(),
            models: vec![DEFAULT_MODEL.to_string()],
        };
        let board = || GuestBoardFacts {
            boards: vec!["default".to_string(), "shop".to_string()],
            sites: 2,
            exposed: 1,
        };
        let mut session = GuestSession::new();
        let text = match run("/katban", &mut session, &options, &board) {
            CommandOutcome::Reply(reply) => reply.text,
            CommandOutcome::NotACommand => panic!("/katban must be a command when enabled"),
        };
        assert!(text.contains("default, shop"), "{text}");
        assert!(text.contains("2 hosted (1 exposed)"), "{text}");
        // Never a path or admin state, even when the host opts in.
        assert!(!text.contains('/'), "no path may leak: {text}");
        assert!(!text.to_ascii_lowercase().contains("admin"), "{text}");
    }

    #[test]
    fn empty_board_facts_still_answer() {
        let options = GuestOptions {
            commands: GuestCommandSet::parse("katban").unwrap(),
            models: vec![DEFAULT_MODEL.to_string()],
        };
        let mut session = GuestSession::new();
        let text = reply("/katban", &mut session, &options);
        assert!(text.contains("none yet"), "{text}");
    }
}
