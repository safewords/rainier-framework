//! Cases: every occurrence of one failure, gathered under one record that
//! outlives the raw batches, with a severity and a status a person can close.
//!
//! A case is keyed by the exception's **fingerprint** (its
//! [`family_hash`](crate::entry::Entry::family_hash)) — failure kind,
//! exception kind, message with digits as `N`, first frame of the
//! application's own code. The first time a fingerprint is seen, a case
//! opens; every later occurrence adds to it. Pure functions here decide
//! everything, and every store calls them as it writes, so a case means the
//! same thing whichever store keeps it.
//!
//! **Severity** starts from what failed — a panic is critical, a server
//! error or a job's final attempt is high, an attempt that will be retried
//! is medium, a client error is low — and goes up one step when the case
//! reaches [`ESCALATE_OCCURRENCES`] occurrences or [`ESCALATE_USERS`]
//! affected users, and again each time it **regresses**: occurs after it was
//! resolved, which reopens it. A severity set by hand stays.
//!
//! **Status** is open, resolved or ignored. Resolving records when; an
//! occurrence after that reopens the case as a regression. An ignored case
//! keeps counting and stays ignored.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::entry::{Batch, Entry, EntryType};

/// Occurrences at which a case's severity goes up a step.
pub const ESCALATE_OCCURRENCES: u64 = 100;
/// Affected users at which a case's severity goes up a step.
pub const ESCALATE_USERS: usize = 10;
/// The most recent batches a case keeps a reference to.
pub const RECENT_BATCHES: usize = 20;
/// The most distinct routes, jobs or commands a case lists.
pub const SOURCES: usize = 10;
/// The most distinct users a case lists (and counts).
pub const USERS: usize = 50;
/// The most history events a case keeps.
pub const HISTORY: usize = 50;

/// Where a case stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseStatus {
    /// Needs looking at.
    Open,
    /// Somebody fixed it. Another occurrence reopens it.
    Resolved,
    /// Known and accepted. It keeps counting and stays ignored.
    Ignored,
}

impl CaseStatus {
    /// `open`, `resolved` or `ignored`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
            Self::Ignored => "ignored",
        }
    }

    /// The status named `value`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "resolved" => Some(Self::Resolved),
            "ignored" => Some(Self::Ignored),
            _ => None,
        }
    }
}

/// How bad a case is, least first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseSeverity {
    /// A client's mistake.
    Low,
    /// A failure that will be retried.
    Medium,
    /// A server error, a job that gave up.
    High,
    /// A panic, or something high that kept growing or came back.
    Critical,
}

impl CaseSeverity {
    /// The step above, or this one at the top.
    pub fn raised(self) -> Self {
        match self {
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High | Self::Critical => Self::Critical,
        }
    }

    /// The severity named `value`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "critical" => Some(Self::Critical),
            _ => None,
        }
    }
}

/// Something that happened to a case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseEvent {
    /// When.
    pub at: DateTime<Utc>,
    /// `opened`, `escalated`, `resolved`, `reopened`, `regressed`,
    /// `ignored`, `severity`.
    pub action: String,
    /// The severity after it, for `escalated`, `regressed` and `severity`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<CaseSeverity>,
    /// What the person who did it wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One failure, every time it happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Case {
    /// A short stable id: the hash of the key.
    pub id: String,
    /// The fingerprint every occurrence shares.
    pub key: String,
    /// The latest occurrence's summary: `NotFound: post 4897 does not exist`.
    pub title: String,
    /// What failed: `HttpRequest`, `JobAttempt`, `Application`, ….
    pub failure_kind: String,
    /// How bad it is.
    pub severity: CaseSeverity,
    /// Set by hand, so occurrences no longer change it.
    #[serde(default)]
    pub severity_pinned: bool,
    /// Where it stands.
    pub status: CaseStatus,
    /// The first occurrence.
    pub first_seen: DateTime<Utc>,
    /// The latest occurrence.
    pub last_seen: DateTime<Utc>,
    /// How many times it happened.
    pub occurrences: u64,
    /// How many times it came back after being resolved.
    #[serde(default)]
    pub regressions: u64,
    /// The batch of the first occurrence.
    pub first_batch_id: String,
    /// The batches of the latest occurrences, newest first.
    pub recent_batches: Vec<String>,
    /// The routes, jobs and commands it happened in, most recent first.
    pub sources: Vec<String>,
    /// The users it happened to, most recent first, at most [`USERS`].
    pub users: Vec<String>,
    /// The application's own frames of the latest occurrence, innermost first.
    #[serde(default)]
    pub frames: Vec<Value>,
    /// When it was last resolved.
    #[serde(default)]
    pub resolved_at: Option<DateTime<Utc>>,
    /// What happened to it, oldest first.
    pub history: Vec<CaseEvent>,
}

/// The id for a fingerprint: 16 hex digits of FNV-1a.
pub fn case_id(key: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// One occurrence, as a case sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Occurrence {
    /// The case it belongs to.
    pub id: String,
    /// The fingerprint.
    pub key: String,
    /// The entry's summary.
    pub title: String,
    /// `HttpRequest`, `JobAttempt`, ….
    pub failure_kind: String,
    /// What this occurrence alone says about severity.
    pub severity: CaseSeverity,
    /// When.
    pub at: DateTime<Utc>,
    /// Its batch.
    pub batch_id: String,
    /// The route name, the job or the command.
    pub source: Option<String>,
    /// Who it happened to.
    pub user: Option<String>,
    /// The application's frames, when the exception had a stack.
    pub frames: Vec<Value>,
}

/// What one exception entry says about its severity.
fn base_severity(entry: &Entry) -> CaseSeverity {
    let tagged = |tag: &str| entry.tags.iter().any(|t| t == tag);
    if tagged("panic") {
        return CaseSeverity::Critical;
    }
    let kind = entry.content.get("type").and_then(Value::as_str).unwrap_or_default();
    if kind == "JobAttempt" {
        let last = entry.content.get("final_attempt").and_then(Value::as_bool).unwrap_or(true);
        return if last { CaseSeverity::High } else { CaseSeverity::Medium };
    }
    if tagged("failed") {
        return CaseSeverity::High;
    }
    let status = entry
        .content
        .get("exception")
        .and_then(|e| e.get("status"))
        .and_then(Value::as_u64)
        .unwrap_or(500);
    if (400..500).contains(&status) {
        CaseSeverity::Low
    } else {
        CaseSeverity::Medium
    }
}

/// The occurrences in `batches`, oldest first.
pub fn occurrences(batches: &[Batch]) -> Vec<Occurrence> {
    let mut found = Vec::new();
    for batch in batches {
        let source = batch.route_name.clone().or_else(|| {
            batch.route_pattern.as_ref().map(|p| match &batch.method {
                Some(m) => format!("{m} {p}"),
                None => p.clone(),
            })
        });
        for entry in batch.entries.iter().filter(|e| e.entry_type == EntryType::Exception) {
            let key = entry.family_key().to_string();
            let source = source.clone().or_else(|| {
                entry
                    .content
                    .get("job")
                    .or_else(|| entry.content.get("command"))
                    .or_else(|| entry.content.get("task"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
            found.push(Occurrence {
                id: case_id(&key),
                key,
                title: entry.summary.clone(),
                failure_kind: entry
                    .content
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("Failure")
                    .to_string(),
                severity: base_severity(entry),
                at: entry.occurred_at,
                batch_id: batch.id.clone(),
                source,
                user: batch.user_id.clone(),
                frames: entry
                    .content
                    .get("frames")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            });
        }
    }
    found.sort_by_key(|o| o.at);
    found
}

/// `value` to the front of `list`, once, keeping at most `limit`.
fn remember(list: &mut Vec<String>, value: &str, limit: usize) {
    list.retain(|v| v != value);
    list.insert(0, value.to_string());
    list.truncate(limit);
}

fn event(at: DateTime<Utc>, action: &str, severity: Option<CaseSeverity>) -> CaseEvent {
    CaseEvent { at, action: action.to_string(), severity, note: None }
}

fn push_history(case: &mut Case, event: CaseEvent) {
    case.history.push(event);
    if case.history.len() > HISTORY {
        // Keep the opening event; drop the oldest after it.
        case.history.remove(1);
    }
}

/// `existing` with `occurrence` added, or a new case for it.
pub fn observe(existing: Option<Case>, occurrence: &Occurrence) -> Case {
    let Some(mut case) = existing else {
        return Case {
            id: occurrence.id.clone(),
            key: occurrence.key.clone(),
            title: occurrence.title.clone(),
            failure_kind: occurrence.failure_kind.clone(),
            severity: occurrence.severity,
            severity_pinned: false,
            status: CaseStatus::Open,
            first_seen: occurrence.at,
            last_seen: occurrence.at,
            occurrences: 1,
            regressions: 0,
            first_batch_id: occurrence.batch_id.clone(),
            recent_batches: vec![occurrence.batch_id.clone()],
            sources: occurrence.source.iter().cloned().collect(),
            users: occurrence.user.iter().cloned().collect(),
            frames: occurrence.frames.clone(),
            resolved_at: None,
            history: vec![event(occurrence.at, "opened", Some(occurrence.severity))],
        };
    };

    case.occurrences += 1;
    if occurrence.at >= case.last_seen {
        case.last_seen = occurrence.at;
        case.title = occurrence.title.clone();
        if !occurrence.frames.is_empty() {
            case.frames = occurrence.frames.clone();
        }
    }
    case.first_seen = case.first_seen.min(occurrence.at);
    remember(&mut case.recent_batches, &occurrence.batch_id, RECENT_BATCHES);
    if let Some(source) = &occurrence.source {
        remember(&mut case.sources, source, SOURCES);
    }
    if let Some(user) = &occurrence.user {
        remember(&mut case.users, user, USERS);
    }

    let regressed = case.status == CaseStatus::Resolved
        && case.resolved_at.is_none_or(|resolved| occurrence.at > resolved);
    if regressed {
        case.status = CaseStatus::Open;
        case.regressions += 1;
        if !case.severity_pinned {
            case.severity = case.severity.max(occurrence.severity).raised();
        }
        let severity = case.severity;
        push_history(&mut case, event(occurrence.at, "regressed", Some(severity)));
        return case;
    }

    if !case.severity_pinned {
        let mut severity = case.severity.max(occurrence.severity);
        let crossed_count = case.occurrences == ESCALATE_OCCURRENCES;
        let crossed_users = occurrence.user.is_some() && case.users.len() == ESCALATE_USERS;
        if crossed_count || crossed_users {
            severity = severity.raised();
        }
        if severity != case.severity {
            case.severity = severity;
            push_history(&mut case, event(occurrence.at, "escalated", Some(severity)));
        }
    }
    case
}

/// What a person can do to a case.
#[derive(Debug, Clone, PartialEq)]
pub enum CaseAction {
    /// It is fixed.
    Resolve,
    /// It is not fixed after all.
    Reopen,
    /// It is known and accepted.
    Ignore,
    /// Set the severity, and keep it there.
    Severity(CaseSeverity),
}

/// `case` after `action`, with a history event carrying `note`.
pub fn act(mut case: Case, action: &CaseAction, note: Option<String>, at: DateTime<Utc>) -> Case {
    let record = |case: &mut Case, name: &str, severity: Option<CaseSeverity>| {
        push_history(case, CaseEvent { at, action: name.into(), severity, note: note.clone() });
    };
    match action {
        CaseAction::Resolve => {
            case.status = CaseStatus::Resolved;
            case.resolved_at = Some(at);
            record(&mut case, "resolved", None);
        }
        CaseAction::Reopen => {
            case.status = CaseStatus::Open;
            record(&mut case, "reopened", None);
        }
        CaseAction::Ignore => {
            case.status = CaseStatus::Ignored;
            record(&mut case, "ignored", None);
        }
        CaseAction::Severity(severity) => {
            case.severity = *severity;
            case.severity_pinned = true;
            record(&mut case, "severity", Some(*severity));
        }
    }
    case
}

/// Which cases to list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CaseQuery {
    /// Only this status.
    pub status: Option<CaseStatus>,
    /// Only this severity or worse.
    pub min_severity: Option<CaseSeverity>,
    /// Only cases whose title, key or sources contain this, ignoring case.
    pub search: Option<String>,
    /// At most this many; defaults to 50, capped at 200.
    pub limit: Option<usize>,
}

/// Answer `query` from `cases`: open before the rest, then most severe, then
/// most recently seen.
pub fn select_cases(cases: impl IntoIterator<Item = Case>, query: &CaseQuery) -> Vec<Case> {
    let needle = query.search.as_ref().map(|s| s.to_lowercase());
    let mut found: Vec<Case> = cases
        .into_iter()
        .filter(|c| query.status.is_none_or(|s| c.status == s))
        .filter(|c| query.min_severity.is_none_or(|s| c.severity >= s))
        .filter(|c| {
            needle.as_ref().is_none_or(|n| {
                c.title.to_lowercase().contains(n)
                    || c.key.to_lowercase().contains(n)
                    || c.sources.iter().any(|s| s.to_lowercase().contains(n))
            })
        })
        .collect();
    let rank = |s: CaseStatus| match s {
        CaseStatus::Open => 0,
        CaseStatus::Resolved => 1,
        CaseStatus::Ignored => 2,
    };
    found.sort_by(|a, b| {
        rank(a.status)
            .cmp(&rank(b.status))
            .then_with(|| b.severity.cmp(&a.severity))
            .then_with(|| b.last_seen.cmp(&a.last_seen))
            .then_with(|| a.id.cmp(&b.id))
    });
    found.truncate(query.limit.unwrap_or(50).min(200));
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(s: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(s, 0).unwrap()
    }

    fn occurrence(s: i64, severity: CaseSeverity, user: Option<&str>) -> Occurrence {
        Occurrence {
            id: case_id("Job|NotFound|post N"),
            key: "Job|NotFound|post N".into(),
            title: format!("NotFound: post {s}"),
            failure_kind: "JobAttempt".into(),
            severity,
            at: at(s),
            batch_id: format!("b{s}"),
            source: Some("increment.update".into()),
            user: user.map(str::to_string),
            frames: vec![],
        }
    }

    #[test]
    fn the_first_occurrence_opens_a_case_and_later_ones_add_to_it() {
        let case = observe(None, &occurrence(10, CaseSeverity::Medium, Some("1")));
        assert_eq!(case.status, CaseStatus::Open);
        assert_eq!(case.occurrences, 1);
        assert_eq!(case.history[0].action, "opened");
        assert_eq!(case.id.len(), 16);

        let case = observe(Some(case), &occurrence(20, CaseSeverity::High, Some("2")));
        assert_eq!(case.occurrences, 2);
        assert_eq!(case.last_seen, at(20));
        assert_eq!(case.first_seen, at(10));
        assert_eq!(case.title, "NotFound: post 20");
        assert_eq!(case.recent_batches, ["b20", "b10"]);
        assert_eq!(case.users, ["2", "1"]);
        assert_eq!(case.severity, CaseSeverity::High, "the worst occurrence wins");
        assert_eq!(case.history.last().unwrap().action, "escalated");

        // An older occurrence arriving late counts but does not retitle.
        let case = observe(Some(case), &occurrence(5, CaseSeverity::Low, None));
        assert_eq!(case.title, "NotFound: post 20");
        assert_eq!(case.first_seen, at(5));
    }

    #[test]
    fn many_occurrences_or_users_raise_the_severity_once_each() {
        let mut case = observe(None, &occurrence(0, CaseSeverity::Low, None));
        for i in 1..ESCALATE_OCCURRENCES as i64 {
            case = observe(Some(case), &occurrence(i, CaseSeverity::Low, None));
        }
        assert_eq!(case.severity, CaseSeverity::Medium);
        case = observe(Some(case), &occurrence(500, CaseSeverity::Low, None));
        assert_eq!(case.severity, CaseSeverity::Medium, "only on crossing");

        let mut case = observe(None, &occurrence(0, CaseSeverity::Medium, Some("u0")));
        for i in 1..ESCALATE_USERS as i64 {
            let user = format!("u{i}");
            case = observe(Some(case), &occurrence(i, CaseSeverity::Medium, Some(&user)));
        }
        assert_eq!(case.severity, CaseSeverity::High);
    }

    #[test]
    fn resolving_then_recurring_reopens_it_as_a_regression() {
        let case = observe(None, &occurrence(10, CaseSeverity::Medium, None));
        let case = act(case, &CaseAction::Resolve, Some("fixed in abc123".into()), at(100));
        assert_eq!(case.status, CaseStatus::Resolved);
        assert_eq!(case.history.last().unwrap().note.as_deref(), Some("fixed in abc123"));

        // Late delivery of an occurrence from before the fix changes nothing.
        let case = observe(Some(case), &occurrence(50, CaseSeverity::Medium, None));
        assert_eq!(case.status, CaseStatus::Resolved);

        let case = observe(Some(case), &occurrence(200, CaseSeverity::Medium, None));
        assert_eq!(case.status, CaseStatus::Open);
        assert_eq!(case.regressions, 1);
        assert_eq!(case.severity, CaseSeverity::High);
        assert_eq!(case.history.last().unwrap().action, "regressed");
    }

    #[test]
    fn an_ignored_case_counts_but_stays_ignored_and_a_pinned_severity_stays() {
        let case = observe(None, &occurrence(10, CaseSeverity::High, None));
        let case = act(case, &CaseAction::Ignore, None, at(20));
        let case = act(case, &CaseAction::Severity(CaseSeverity::Low), None, at(21));
        let case = observe(Some(case), &occurrence(30, CaseSeverity::Critical, None));
        assert_eq!(case.status, CaseStatus::Ignored);
        assert_eq!(case.severity, CaseSeverity::Low);
        assert_eq!(case.occurrences, 2);
    }

    #[test]
    fn severity_follows_what_failed() {
        let entry = |tags: &[&str], content: Value| Entry {
            id: "e".into(),
            batch_id: "b".into(),
            entry_type: EntryType::Exception,
            span_id: None,
            parent_span_id: None,
            family_hash: Some("k".into()),
            summary: "s".into(),
            occurred_at: at(0),
            duration_us: None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            content,
        };
        use serde_json::json;
        assert_eq!(base_severity(&entry(&["panic"], json!({}))), CaseSeverity::Critical);
        assert_eq!(
            base_severity(&entry(&[], json!({ "type": "JobAttempt", "final_attempt": false }))),
            CaseSeverity::Medium
        );
        assert_eq!(
            base_severity(&entry(&[], json!({ "type": "JobAttempt", "final_attempt": true }))),
            CaseSeverity::High
        );
        assert_eq!(base_severity(&entry(&["failed"], json!({}))), CaseSeverity::High);
        assert_eq!(
            base_severity(&entry(&[], json!({ "exception": { "status": 404 } }))),
            CaseSeverity::Low
        );
    }

    #[test]
    fn listing_puts_open_and_severe_first_and_filters() {
        let mut low = observe(None, &occurrence(10, CaseSeverity::Low, None));
        low.id = "a".into();
        let mut high = observe(None, &occurrence(5, CaseSeverity::High, None));
        high.id = "b".into();
        let resolved = act(
            {
                let mut c = observe(None, &occurrence(50, CaseSeverity::Critical, None));
                c.id = "c".into();
                c
            },
            &CaseAction::Resolve,
            None,
            at(60),
        );
        let all = vec![low.clone(), resolved.clone(), high.clone()];
        let ids = |q: &CaseQuery| {
            select_cases(all.clone(), q).into_iter().map(|c| c.id).collect::<Vec<_>>()
        };
        assert_eq!(ids(&CaseQuery::default()), ["b", "a", "c"]);
        assert_eq!(
            ids(&CaseQuery { status: Some(CaseStatus::Open), ..CaseQuery::default() }),
            ["b", "a"]
        );
        assert_eq!(
            ids(&CaseQuery { min_severity: Some(CaseSeverity::High), ..CaseQuery::default() }),
            ["b", "c"]
        );
        assert_eq!(
            ids(&CaseQuery {
                search: Some("INCREMENT".into()),
                limit: Some(1),
                ..CaseQuery::default()
            }),
            ["b"]
        );
    }
}
