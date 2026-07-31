//! Parsing a command line into [`Arguments`].
//!
//! A deliberately small grammar, chosen so parsing is unambiguous without a
//! per-command specification:
//!
//! | Form | Meaning |
//! |---|---|
//! | `word` | a positional argument |
//! | `--name=value` | an option |
//! | `--name` | a flag |
//! | `-abc` | the flags `a`, `b` and `c` |
//! | `--` | everything after this is positional |
//!
//! `--name value` is **not** an option, because whether `value` belongs to
//! `--name` or stands alone cannot be known without knowing the command's
//! signature — and getting it wrong silently swallows an argument.

use std::collections::{HashMap, HashSet};

/// A parsed command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Arguments {
    command: String,
    positional: Vec<String>,
    options: HashMap<String, String>,
    flags: HashSet<String>,
}

impl Arguments {
    /// Parse an argument list, **without** the program name.
    ///
    /// The first positional word is the command; the rest belong to it.
    pub fn parse<I, S>(argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut parsed = Arguments::default();
        let mut only_positional = false;

        for raw in argv {
            let argument: String = raw.into();

            if only_positional {
                parsed.push_positional(argument);
                continue;
            }

            if argument == "--" {
                only_positional = true;
                continue;
            }

            if let Some(rest) = argument.strip_prefix("--") {
                match rest.split_once('=') {
                    Some((name, value)) => {
                        parsed.options.insert(name.to_string(), value.to_string());
                    }
                    None => {
                        parsed.flags.insert(rest.to_string());
                    }
                }
                continue;
            }

            if let Some(rest) = argument.strip_prefix('-') {
                if !rest.is_empty() && !rest.starts_with(|c: char| c.is_ascii_digit()) {
                    // `-abc` is three flags, the convention every POSIX tool
                    // follows. A leading digit means a negative number.
                    for flag in rest.chars() {
                        parsed.flags.insert(flag.to_string());
                    }
                    continue;
                }
            }

            parsed.push_positional(argument);
        }

        parsed
    }

    fn push_positional(&mut self, argument: String) {
        if self.command.is_empty() {
            self.command = argument;
        } else {
            self.positional.push(argument);
        }
    }

    /// The command name, or `""` if none was given.
    pub fn command(&self) -> &str {
        &self.command
    }

    /// Whether no command was given.
    pub fn is_empty(&self) -> bool {
        self.command.is_empty()
    }

    /// The positional arguments after the command.
    pub fn positional(&self) -> &[String] {
        &self.positional
    }

    /// The positional argument at `index`, after the command.
    pub fn argument(&self, index: usize) -> Option<&str> {
        self.positional.get(index).map(String::as_str)
    }

    /// An option's value.
    ///
    /// Options are `--name=value`. A bare `--name` is a [flag](Self::flag), so
    /// `--port 8000` gives a flag called `port` and a stray positional
    /// `8000` — and the value goes nowhere.
    ///
    /// That form cannot be supported without knowing which options take a
    /// value (`--verbose migrate` is a flag and a command, not an option), so
    /// instead it is **reported**: asking for an option that was given as a
    /// bare flag warns and says what to write. Serving on the wrong port
    /// because a `=` was missing is a bad afternoon.
    pub fn option(&self, name: &str) -> Option<&str> {
        let value = self.options.get(name).map(String::as_str);

        if value.is_none() && self.flags.contains(name) {
            tracing::warn!(
                "`--{name}` was given with no value; write `--{name}=value`. Ignoring it."
            );
        }
        value
    }

    /// An option's value, or `default`.
    pub fn option_or<'a>(&'a self, name: &str, default: &'a str) -> &'a str {
        self.option(name).unwrap_or(default)
    }

    /// An option parsed into `T`, or `default` if absent or unparsable.
    pub fn parsed_or<T: std::str::FromStr>(&self, name: &str, default: T) -> T {
        self.option(name).and_then(|value| value.parse().ok()).unwrap_or(default)
    }

    /// Whether a flag was given.
    pub fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }

    /// Whether `name` was given as a bare flag when a value was wanted.
    ///
    /// What [`option`](Self::option) warns about, exposed so a command can
    /// refuse rather than carry on with a default.
    pub fn is_valueless(&self, name: &str) -> bool {
        self.flags.contains(name) && !self.options.contains_key(name)
    }

    /// Whether help was asked for, in any of its usual spellings.
    pub fn wants_help(&self) -> bool {
        self.flag("help") || self.flag("h") || self.command == "help"
    }

    /// Every option, for diagnostics.
    pub fn options(&self) -> &HashMap<String, String> {
        &self.options
    }

    /// Every flag, for diagnostics.
    pub fn flags(&self) -> &HashSet<String> {
        &self.flags
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Arguments {
        Arguments::parse(line.split_whitespace())
    }

    #[test]
    fn the_first_word_is_the_command() {
        let args = parse("route:list");
        assert_eq!(args.command(), "route:list");
        assert!(args.positional().is_empty());
        assert!(!args.is_empty());
    }

    #[test]
    fn an_empty_line_has_no_command() {
        assert!(Arguments::parse(Vec::<String>::new()).is_empty());
    }

    #[test]
    fn later_words_are_positional() {
        let args = parse("make:controller PostController extra");
        assert_eq!(args.command(), "make:controller");
        assert_eq!(args.argument(0), Some("PostController"));
        assert_eq!(args.argument(1), Some("extra"));
        assert_eq!(args.argument(2), None);
    }

    #[test]
    fn long_options_carry_a_value() {
        let args = parse("serve --port=3000 --host=0.0.0.0");
        assert_eq!(args.option("port"), Some("3000"));
        assert_eq!(args.option("host"), Some("0.0.0.0"));
        assert_eq!(args.parsed_or("port", 8000u16), 3000);
    }

    #[test]
    fn a_missing_or_unparsable_option_falls_back() {
        let args = parse("serve --port=nonsense");
        assert_eq!(args.parsed_or("port", 8000u16), 8000);
        assert_eq!(args.parsed_or("timeout", 30u32), 30);
        assert_eq!(args.option_or("host", "127.0.0.1"), "127.0.0.1");
    }

    #[test]
    fn a_valueless_long_argument_is_a_flag() {
        let args = parse("queue:work --once --quiet");
        assert!(args.flag("once"));
        assert!(args.flag("quiet"));
        assert!(!args.flag("verbose"));
        assert!(args.option("once").is_none());
    }

    #[test]
    fn short_flags_can_be_bundled() {
        let args = parse("migrate -vf");
        assert!(args.flag("v"));
        assert!(args.flag("f"));
        assert_eq!(args.flags().len(), 2);
    }

    #[test]
    fn a_separated_value_is_positional_not_an_option() {
        // `--port 3000` is ambiguous without a per-command signature, so
        // `3000` stays positional rather than being silently swallowed.
        let args = parse("serve --port 3000");
        assert!(args.flag("port"));
        assert_eq!(args.option("port"), None);
        assert_eq!(args.argument(0), Some("3000"));
    }

    #[test]
    fn a_double_dash_ends_option_parsing() {
        let args = parse("run -- --not-a-flag positional");
        assert_eq!(args.command(), "run");
        assert_eq!(args.positional(), ["--not-a-flag", "positional"]);
        assert!(args.flags().is_empty());
    }

    #[test]
    fn a_negative_number_is_not_a_flag_bundle() {
        let args = parse("offset -5");
        assert_eq!(args.argument(0), Some("-5"));
        assert!(args.flags().is_empty());
    }

    #[test]
    fn an_empty_option_value_is_kept() {
        let args = parse("serve --host=");
        assert_eq!(args.option("host"), Some(""));
    }

    #[test]
    fn help_is_recognised_however_it_is_spelled() {
        assert!(parse("serve --help").wants_help());
        assert!(parse("serve -h").wants_help());
        assert!(parse("help").wants_help());
        assert!(!parse("serve").wants_help());
    }

    #[test]
    fn an_option_given_without_a_value_is_not_silently_a_flag() {
        // `--port 8091` is how most CLIs are invoked, and here it parses as a
        // flag plus a positional. It cannot be *made* to work — `--verbose
        // migrate` is a flag and a command — so it has to be visible instead.
        let args = Arguments::parse(["serve", "--port", "8091"]);

        assert!(args.is_valueless("port"));
        assert_eq!(args.option("port"), None);
        assert_eq!(args.argument(0), Some("8091"), "the value became a positional");

        // The documented form still works, and reports nothing.
        let args = Arguments::parse(["serve", "--port=8091"]);
        assert_eq!(args.option("port"), Some("8091"));
        assert!(!args.is_valueless("port"));
    }
}
