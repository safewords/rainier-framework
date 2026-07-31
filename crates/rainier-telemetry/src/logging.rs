//! How log lines should look — [`LogFormat`].
//!
//! One setting, because the choice is really one question: *is a human reading
//! this, or a log aggregator?* A human wants colour, alignment and one event
//! per paragraph. Datadog, Loki and CloudWatch want one JSON object per line
//! and nothing else, and they will happily index a pretty-printed line as an
//! unparseable blob with the fields no longer fields.
//!
//! Getting this wrong is quiet in exactly the way that costs an incident: the
//! logs are *there*, they simply cannot be searched by the field you need at
//! the moment you need it.

use rainier_support::setting_enum;

setting_enum! {
    /// The shape of a log line.
    ///
    /// ```
    /// use rainier_support::Setting;
    /// use rainier_telemetry::LogFormat;
    ///
    /// assert_eq!(LogFormat::parse("json").unwrap(), LogFormat::Json);
    /// assert!(LogFormat::parse("jsn").is_err());
    /// ```
    pub enum LogFormat: "log format" {
        /// JSON in production and staging, pretty everywhere else.
        ///
        /// The default, because it is right far more often than either fixed
        /// answer, and because the failure it prevents — a production
        /// deployment logging prose into an aggregator that wanted objects —
        /// is one nobody notices until they are searching for something.
        #[default]
        Auto = "auto",

        /// The human format: coloured, aligned, one event per line.
        ///
        /// `tracing_subscriber`'s default, which is what a Rainier
        /// application has always printed — so this is the format nobody
        /// asked for and everybody recognises.
        Pretty = "pretty",

        /// Denser than `pretty` and still readable. For `docker logs` and CI,
        /// where the terminal is narrow and nothing is colouring anything.
        Compact = "compact",

        /// One JSON object per line. For anything that indexes fields.
        Json = "json",
    }
}

impl LogFormat {
    /// What [`Auto`](Self::Auto) means in this environment.
    ///
    /// Production and staging get JSON; everything else gets `pretty`. Staging
    /// is included deliberately — it exists to be production-shaped, and a
    /// staging deployment whose logs are not searchable the same way is one
    /// that cannot rehearse an incident.
    ///
    /// ```
    /// use rainier_telemetry::LogFormat;
    ///
    /// assert_eq!(LogFormat::Auto.resolve("production"), LogFormat::Json);
    /// assert_eq!(LogFormat::Auto.resolve("local"), LogFormat::Pretty);
    /// // Anything explicit is left alone.
    /// assert_eq!(LogFormat::Compact.resolve("production"), LogFormat::Compact);
    /// ```
    pub fn resolve(self, environment: &str) -> Self {
        match self {
            Self::Auto if matches!(environment, "production" | "prod" | "staging") => Self::Json,
            Self::Auto => Self::Pretty,
            explicit => explicit,
        }
    }

    /// Whether lines in this format are meant to be parsed rather than read.
    pub fn is_structured(self) -> bool {
        matches!(self, Self::Json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_support::Setting;

    #[test]
    fn auto_follows_the_environment() {
        assert_eq!(LogFormat::Auto.resolve("production"), LogFormat::Json);
        assert_eq!(LogFormat::Auto.resolve("prod"), LogFormat::Json);
        assert_eq!(LogFormat::Auto.resolve("staging"), LogFormat::Json);
        assert_eq!(LogFormat::Auto.resolve("local"), LogFormat::Pretty);
        assert_eq!(LogFormat::Auto.resolve("testing"), LogFormat::Pretty);
        assert_eq!(LogFormat::Auto.resolve(""), LogFormat::Pretty);
    }

    #[test]
    fn an_explicit_format_is_never_second_guessed() {
        // Somebody who wrote `LOG_FORMAT=pretty` on a production box is
        // debugging, and having the setting quietly ignored is the worst
        // possible answer.
        for format in [LogFormat::Pretty, LogFormat::Compact, LogFormat::Json] {
            assert_eq!(format.resolve("production"), format);
            assert_eq!(format.resolve("local"), format);
        }
    }

    #[test]
    fn resolving_is_idempotent() {
        // The installer resolves once; nothing should change if it happens
        // twice.
        let once = LogFormat::Auto.resolve("production");
        assert_eq!(once.resolve("production"), once);
    }

    #[test]
    fn only_json_is_for_machines() {
        assert!(LogFormat::Json.is_structured());
        assert!(!LogFormat::Pretty.is_structured());
        assert!(!LogFormat::Compact.is_structured());
        // `Auto` is not an answer yet, so it is not a structured one.
        assert!(!LogFormat::Auto.is_structured());
    }

    #[test]
    fn the_default_is_auto() {
        assert_eq!(LogFormat::default(), LogFormat::Auto);
        assert_eq!(LogFormat::parse("auto").unwrap(), LogFormat::Auto);
    }
}
