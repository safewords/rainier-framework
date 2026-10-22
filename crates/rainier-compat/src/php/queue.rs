//! Jobs a PHP application queued — [`QueuedJob`], read-only.
//!
//! ```
//! use rainier_compat::php::queue::QueuedJob;
//!
//! let payload = r#"{"uuid":"9b1d","displayName":"App\\Jobs\\SendEmail","job":"QueueHandler@call",
//!     "maxTries":3,"timeout":60,"attempts":1,
//!     "data":{"commandName":"App\\Jobs\\SendEmail",
//!             "command":"O:18:\"App\\Jobs\\SendEmail\":1:{s:2:\"to\";s:3:\"ada\";}"}}"#;
//!
//! let job = QueuedJob::parse(payload).unwrap();
//! assert_eq!(job.display_name, "App\\Jobs\\SendEmail");
//! assert_eq!(job.command().unwrap().unwrap().get("to").and_then(|v| v.as_str()), Some("ada"));
//! ```
//!
//! # For looking, not for running
//!
//! A queued PHP job is a JSON envelope around the job object, `serialize()`d
//! — a class name and its properties. Reading one answers the questions a
//! migration asks of a live queue: which jobs are waiting, how many of each,
//! with what arguments, on their how-manyth attempt. It is how you size the
//! Rust worker that replaces one job class before turning the PHP one off.
//!
//! Running one is a different thing: the job's behaviour is PHP code, and the
//! payload is only its constructor's leftovers. Port the job, dispatch it
//! from both sides under a new name, and drain the old queue.
//!
//! # Encrypted jobs
//!
//! A job class can ask to be encrypted, in which case `command` is the PHP
//! encrypter's envelope rather than a serialized object.
//! [`command`](QueuedJob::command) reports that as `Ok(None)`;
//! [`command_with`](QueuedJob::command_with) opens it.

use rainier_crypt::Encrypter;
use rainier_support::{Error, Result};
use serde::Deserialize;
use serde_json::Value as Json;

use super::serialize::{unserialize, Value};

/// One queued job's envelope.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedJob {
    /// Unique per dispatch.
    #[serde(default)]
    pub uuid: Option<String>,
    /// The job's class, or a description of a closure or listener.
    pub display_name: String,
    /// The handler the PHP worker calls.
    #[serde(default)]
    pub job: Option<String>,
    /// How many attempts are allowed.
    #[serde(default)]
    pub max_tries: Option<u32>,
    /// Seconds before an attempt is killed.
    #[serde(default)]
    pub timeout: Option<u64>,
    /// When to stop retrying, as a Unix timestamp.
    #[serde(default)]
    pub retry_until: Option<i64>,
    /// How many attempts have been made. Some drivers keep this outside the
    /// payload; absent then.
    #[serde(default)]
    pub attempts: Option<u32>,
    /// The job's data — `commandName` and `command` for a job class.
    #[serde(default)]
    pub data: Json,
    /// Everything else in the envelope.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Json>,
}

impl QueuedJob {
    /// Read an envelope.
    pub fn parse(payload: &str) -> Result<Self> {
        serde_json::from_str(payload)
            .map_err(|e| Error::bad_request(format!("not a queued job payload: {e}")))
    }

    /// The job's class as the data names it.
    pub fn command_name(&self) -> Option<&str> {
        self.data.get("commandName").and_then(Json::as_str)
    }

    /// The job object, unserialized — `Ok(None)` when there is none, or it is
    /// encrypted.
    pub fn command(&self) -> Result<Option<Value>> {
        let Some(command) = self.data.get("command").and_then(Json::as_str) else {
            return Ok(None);
        };
        if !command.starts_with("O:") && !command.starts_with("C:") {
            return Ok(None);
        }
        unserialize(command.as_bytes()).map(Some)
    }

    /// The job object, opening an encrypted one with `encrypter` — the PHP
    /// envelope under the application key.
    pub fn command_with(&self, encrypter: &dyn Encrypter) -> Result<Option<Value>> {
        if let Some(command) = self.command()? {
            return Ok(Some(command));
        }
        let Some(sealed) = self.data.get("command").and_then(Json::as_str) else {
            return Ok(None);
        };
        let plain = encrypter.decrypt_bytes(sealed)?;
        unserialize(&plain).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_crypt::{Key, KeyRing, PhpEncrypter};

    fn envelope(command: &str) -> String {
        serde_json::json!({
            "uuid": "6f1c",
            "displayName": "App\\Jobs\\ProcessReading",
            "job": "QueueHandler@call",
            "maxTries": 2,
            "timeout": 360,
            "retryUntil": null,
            "data": { "commandName": "App\\Jobs\\ProcessReading", "command": command },
            "trace_id": "x"
        })
        .to_string()
    }

    #[test]
    fn the_envelope_and_the_job_object_read() {
        let command = "O:23:\"App\\Jobs\\ProcessReading\":2:{s:3:\"vin\";s:3:\"5YJ\";s:8:\"\0*\0queue\";s:7:\"default\";}";
        let job = QueuedJob::parse(&envelope(command)).unwrap();

        assert_eq!(job.display_name, "App\\Jobs\\ProcessReading");
        assert_eq!(job.max_tries, Some(2));
        assert_eq!(job.timeout, Some(360));
        assert_eq!(job.command_name(), Some("App\\Jobs\\ProcessReading"));
        assert!(job.extra.contains_key("trace_id"));

        let object = job.command().unwrap().unwrap();
        assert_eq!(object.get("vin").and_then(Value::as_str), Some("5YJ"));
        assert_eq!(object.get("queue").and_then(Value::as_str), Some("default"));
    }

    #[test]
    fn an_encrypted_job_needs_the_key() {
        let encrypter = PhpEncrypter::new(KeyRing::new(Key::from_bytes([9; 32])));
        let sealed = encrypter.encrypt_bytes(br#"O:8:"App\\Job":1:{s:1:"a";i:1;}"#).unwrap();
        let job = QueuedJob::parse(&envelope(&sealed)).unwrap();

        assert_eq!(job.command().unwrap(), None);
        let object = job.command_with(&encrypter).unwrap().unwrap();
        assert_eq!(object.get("a").and_then(Value::as_int), Some(1));
    }

    #[test]
    fn junk_is_an_error() {
        assert!(QueuedJob::parse("{").is_err());
        assert!(QueuedJob::parse("{}").is_err(), "displayName is required");
    }
}
