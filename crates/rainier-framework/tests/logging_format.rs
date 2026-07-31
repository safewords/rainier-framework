//! Installing the log subscriber — its own test file, because a global
//! subscriber can be installed exactly once per process and any second test
//! here would be asserting on whichever one happened to run first.

use rainier_framework::config::Config;
use rainier_framework::keys;
use rainier_framework::observability::TelemetrySettings;
use rainier_telemetry::LogFormat;

/// Run with `--nocapture` to see the line this prints; it should be one JSON
/// object with `message` and `field` at the top level, not nested under
/// `fields`.
#[test]
fn installing_once_takes_and_installing_again_leaves_it_alone() {
    let config = Config::new();
    config.set(keys::LOG_FORMAT, LogFormat::Json).unwrap();
    let settings = TelemetrySettings::from_config(&config);

    assert!(settings.install_logging("production", "info"), "the first install should take");

    // An application that installed its own subscriber, or a second `boot` in
    // the same process, must not have it swapped underneath — and must not
    // panic, which is what `.init()` would do.
    assert!(!settings.install_logging("production", "info"), "the second must not replace it");

    // A filter nobody can parse is a typo, not a request for silence: it falls
    // back rather than refusing, and this must not panic either.
    assert!(!settings.install_logging("production", "not a filter((("));

    tracing::info!(target: "test", field = 7, "a line in the installed format");
}
