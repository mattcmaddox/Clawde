// input.rs — Slash command helpers and input mode types.

/// Check whether a string looks like a slash command (e.g. "/help").
pub fn is_slash_command(input: &str) -> bool {
    input.starts_with('/') && !input.starts_with("//")
}

/// Parse a slash command into `(command_name, args)`.
/// Returns `("", "")` if the input is not a slash command.
///
/// The input is trimmed first: the multi-line prompt treats plain Enter as a
/// newline, so a buffer submitted after an accidental Enter carries a
/// trailing `\n` (e.g. "/update\n"). Without the trim the command name would
/// embed that whitespace and miss the registry ("Unknown command").
pub fn parse_slash_command(input: &str) -> (&str, &str) {
    let input = input.trim();
    if !is_slash_command(input) {
        return ("", "");
    }
    let without_slash = &input[1..];
    if let Some(space_idx) = without_slash.find(' ') {
        (
            &without_slash[..space_idx],
            without_slash[space_idx + 1..].trim(),
        )
    } else {
        (without_slash, "")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_command_detection() {
        assert!(is_slash_command("/help"));
        assert!(is_slash_command("/compact args"));
        assert!(!is_slash_command("//comment"));
        assert!(!is_slash_command("hello"));
        assert!(!is_slash_command(""));
    }

    #[test]
    fn parse_no_args() {
        let (cmd, args) = parse_slash_command("/help");
        assert_eq!(cmd, "help");
        assert_eq!(args, "");
    }

    #[test]
    fn parse_trailing_newline_from_multiline_submit() {
        // Plain Enter inserts a newline in the multi-line prompt; submitting
        // afterwards leaves a trailing \n in the buffer. The command must
        // still resolve instead of surfacing "Unknown command".
        let (cmd, args) = parse_slash_command("/update\n");
        assert_eq!(cmd, "update");
        assert_eq!(args, "");

        let (cmd, args) = parse_slash_command("/keys set abc \n");
        assert_eq!(cmd, "keys");
        assert_eq!(args, "set abc");

        let (cmd, args) = parse_slash_command("  /help  \n");
        assert_eq!(cmd, "help");
        assert_eq!(args, "");
    }

    #[test]
    fn parse_with_args() {
        let (cmd, args) = parse_slash_command("/compact  --force ");
        assert_eq!(cmd, "compact");
        assert_eq!(args, "--force");
    }

    #[test]
    fn parse_non_slash() {
        let (cmd, args) = parse_slash_command("hello world");
        assert_eq!(cmd, "");
        assert_eq!(args, "");
    }
}
