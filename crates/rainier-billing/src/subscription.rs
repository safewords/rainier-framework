//! What a subscription means for access — [`SubscriptionStatus`] and
//! [`SubscriptionSnapshot`].
//!
//! # Store a snapshot; do not ask Stripe on every request
//!
//! "May this user use the paid feature?" is asked on every request, and the
//! answer must not be a network call to Stripe: it would be slow, it would be
//! rate-limited, and an outage there would lock every paying customer out
//! here. So the application keeps a **copy** — a row per subscription —
//! written from webhooks, and answers from the row.
//!
//! [`SubscriptionSnapshot`] is that row's content: the handful of fields the
//! questions need, built from a subscription object wherever one appears
//! (a webhook, a retrieve, a list). The questions themselves are methods
//! here, so they are answered the same way everywhere instead of being
//! re-derived from `status` strings in each controller.
//!
//! # The questions an application asks
//!
//! | | true when |
//! |---|---|
//! | [`on_trial`](SubscriptionSnapshot::on_trial) | `trialing` and the trial has not ended |
//! | [`canceled`](SubscriptionSnapshot::canceled) | it has been told to end — now or at the period's end |
//! | [`on_grace_period`](SubscriptionSnapshot::on_grace_period) | canceled, but the paid-for period is not over |
//! | [`ended`](SubscriptionSnapshot::ended) | canceled and past the grace period |
//! | [`active`](SubscriptionSnapshot::active) | not ended, and not in a state that means "not paid" |
//! | [`valid`](SubscriptionSnapshot::valid) | active, on trial, or on a grace period — **the one to gate features on** |
//!
//! Every question takes `now`, rather than reading the clock, so a test can
//! ask about next Tuesday without sleeping until then.

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A subscription's `status`, as Stripe defines it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionStatus {
    /// The first payment has not succeeded yet.
    Incomplete,
    /// The first payment did not succeed within 23 hours; it is dead.
    IncompleteExpired,
    /// In a free trial.
    Trialing,
    /// Paid up.
    Active,
    /// A renewal payment failed and Stripe is retrying.
    PastDue,
    /// Ended.
    Canceled,
    /// Retries exhausted; Stripe has stopped trying but has not canceled.
    Unpaid,
    /// A trial ended with no payment method.
    Paused,
    /// A status this version does not know — never treated as paid.
    #[serde(other)]
    Unknown,
}

impl SubscriptionStatus {
    /// Parse Stripe's string. Anything unrecognised is
    /// [`Unknown`](Self::Unknown), which grants nothing.
    pub fn parse(status: &str) -> Self {
        serde_json::from_value(Value::String(status.to_string())).unwrap_or(Self::Unknown)
    }

    /// Stripe's string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Incomplete => "incomplete",
            Self::IncompleteExpired => "incomplete_expired",
            Self::Trialing => "trialing",
            Self::Active => "active",
            Self::PastDue => "past_due",
            Self::Canceled => "canceled",
            Self::Unpaid => "unpaid",
            Self::Paused => "paused",
            Self::Unknown => "unknown",
        }
    }
}

/// The fields of a subscription that access decisions need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionSnapshot {
    /// `sub_…`.
    pub id: String,
    /// `cus_…`.
    pub customer: Option<String>,
    /// The status.
    pub status: SubscriptionStatus,
    /// Every price on it, in item order.
    pub prices: Vec<String>,
    /// The first item's quantity — seats, for a per-seat plan.
    pub quantity: Option<u64>,
    /// When the trial ends.
    pub trial_end: Option<DateTime<Utc>>,
    /// When the paid-for period ends.
    pub current_period_end: Option<DateTime<Utc>>,
    /// Whether it ends when the period does.
    pub cancel_at_period_end: bool,
    /// A specific time it is set to end, if one was given.
    pub cancel_at: Option<DateTime<Utc>>,
    /// When it actually ended.
    pub ended_at: Option<DateTime<Utc>>,
}

impl SubscriptionSnapshot {
    /// Build one from a subscription object — `data.object` of a
    /// `customer.subscription.*` event, or a retrieved subscription.
    ///
    /// `current_period_end` is read from the subscription and, failing that,
    /// from its first item: API versions from 2025-03-31 moved it onto the
    /// items, and a snapshot that read only one place would lose it on the
    /// other side of an upgrade.
    pub fn from_object(object: &Value) -> Self {
        let first_item = &object["items"]["data"][0];

        let prices = object["items"]["data"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| match &item["price"] {
                        Value::String(id) => Some(id.clone()),
                        price => price["id"].as_str().map(String::from),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let customer = match &object["customer"] {
            Value::String(id) => Some(id.clone()),
            customer => customer["id"].as_str().map(String::from),
        };

        Self {
            id: object["id"].as_str().unwrap_or_default().to_string(),
            customer,
            status: object["status"]
                .as_str()
                .map(SubscriptionStatus::parse)
                .unwrap_or(SubscriptionStatus::Unknown),
            prices,
            quantity: first_item["quantity"].as_u64(),
            trial_end: timestamp(&object["trial_end"]),
            current_period_end: timestamp(&object["current_period_end"])
                .or_else(|| timestamp(&first_item["current_period_end"])),
            cancel_at_period_end: object["cancel_at_period_end"].as_bool().unwrap_or(false),
            cancel_at: timestamp(&object["cancel_at"]),
            ended_at: timestamp(&object["ended_at"]),
        }
    }

    /// Whether `price` is on it.
    pub fn has_price(&self, price: &str) -> bool {
        self.prices.iter().any(|p| p == price)
    }

    /// When access ends, if it has been told to end.
    pub fn ends_at(&self) -> Option<DateTime<Utc>> {
        if self.status == SubscriptionStatus::Canceled {
            return self.ended_at.or(self.cancel_at).or(self.current_period_end);
        }
        if self.cancel_at_period_end {
            return self.cancel_at.or(self.current_period_end);
        }
        self.cancel_at
    }

    /// In a trial that has not run out.
    pub fn on_trial(&self, now: DateTime<Utc>) -> bool {
        self.status == SubscriptionStatus::Trialing && self.trial_end.is_none_or(|end| end > now)
    }

    /// Told to end — now, or when the period does.
    pub fn canceled(&self) -> bool {
        self.status == SubscriptionStatus::Canceled
            || self.cancel_at_period_end
            || self.cancel_at.is_some()
            || self.ended_at.is_some()
    }

    /// Canceled, but still inside the period the customer paid for.
    pub fn on_grace_period(&self, now: DateTime<Utc>) -> bool {
        self.canceled()
            && self.status != SubscriptionStatus::Canceled
            && self.ends_at().is_some_and(|end| end > now)
    }

    /// Canceled and past its grace period.
    pub fn ended(&self, now: DateTime<Utc>) -> bool {
        self.canceled() && !self.on_grace_period(now)
    }

    /// A renewal failed and Stripe is retrying.
    pub fn past_due(&self) -> bool {
        self.status == SubscriptionStatus::PastDue
    }

    /// The first payment has not gone through — typically waiting on 3-D
    /// Secure.
    pub fn incomplete(&self) -> bool {
        matches!(
            self.status,
            SubscriptionStatus::Incomplete | SubscriptionStatus::IncompleteExpired
        )
    }

    /// Paying, and not ended.
    ///
    /// `past_due` counts as **not** active. Being generous to a customer
    /// whose card failed is a product decision; if you want it, ask
    /// `active() || past_due()` explicitly where you mean it.
    pub fn active(&self, now: DateTime<Utc>) -> bool {
        !self.ended(now)
            && matches!(self.status, SubscriptionStatus::Active | SubscriptionStatus::Trialing)
    }

    /// Whether to grant what the subscription pays for.
    pub fn valid(&self, now: DateTime<Utc>) -> bool {
        self.active(now) || self.on_trial(now) || self.on_grace_period(now)
    }
}

/// A Unix timestamp field, or `None` for `null` and absent.
fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(value.as_i64()?, 0).single()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_800_000_000;

    fn now() -> DateTime<Utc> {
        Utc.timestamp_opt(NOW, 0).unwrap()
    }

    fn snapshot(fields: Value) -> SubscriptionSnapshot {
        let mut object = json!({
            "id": "sub_1",
            "customer": "cus_1",
            "status": "active",
            "cancel_at_period_end": false,
            "items": {"data": [{"price": {"id": "price_pro"}, "quantity": 3, "current_period_end": NOW + 86_400}]},
        });
        for (key, value) in fields.as_object().unwrap() {
            object[key] = value.clone();
        }
        SubscriptionSnapshot::from_object(&object)
    }

    #[test]
    fn a_snapshot_reads_the_fields_wherever_the_api_version_put_them() {
        let s = snapshot(json!({}));
        assert_eq!(s.id, "sub_1");
        assert_eq!(s.customer.as_deref(), Some("cus_1"));
        assert_eq!(s.prices, vec!["price_pro"]);
        assert_eq!(s.quantity, Some(3));
        assert_eq!(s.current_period_end.unwrap().timestamp(), NOW + 86_400, "from the item");

        let older = snapshot(json!({"current_period_end": NOW + 10, "customer": {"id": "cus_2"}}));
        assert_eq!(
            older.current_period_end.unwrap().timestamp(),
            NOW + 10,
            "from the subscription"
        );
        assert_eq!(older.customer.as_deref(), Some("cus_2"));
    }

    #[test]
    fn statuses_parse_and_an_unknown_one_grants_nothing() {
        assert_eq!(SubscriptionStatus::parse("past_due"), SubscriptionStatus::PastDue);
        assert_eq!(
            SubscriptionStatus::parse("incomplete_expired"),
            SubscriptionStatus::IncompleteExpired
        );
        assert_eq!(SubscriptionStatus::parse("something_new"), SubscriptionStatus::Unknown);
        assert_eq!(SubscriptionStatus::Paused.as_str(), "paused");

        let s = snapshot(json!({"status": "something_new"}));
        assert!(!s.valid(now()));
    }

    #[test]
    fn an_active_subscription_is_valid() {
        let s = snapshot(json!({}));
        assert!(s.active(now()));
        assert!(s.valid(now()));
        assert!(!s.canceled());
        assert!(!s.on_trial(now()));
        assert!(!s.on_grace_period(now()));
        assert!(!s.ended(now()));
    }

    #[test]
    fn a_trial_is_valid_until_it_ends() {
        let s = snapshot(json!({"status": "trialing", "trial_end": NOW + 60}));
        assert!(s.on_trial(now()));
        assert!(s.valid(now()));

        let later = Utc.timestamp_opt(NOW + 120, 0).unwrap();
        assert!(!s.on_trial(later));
    }

    #[test]
    fn canceling_at_period_end_is_a_grace_period_and_then_the_end() {
        let s = snapshot(json!({"cancel_at_period_end": true}));
        assert!(s.canceled());
        assert!(s.on_grace_period(now()));
        assert!(s.valid(now()));
        assert!(!s.ended(now()));

        let after = Utc.timestamp_opt(NOW + 86_401, 0).unwrap();
        assert!(!s.on_grace_period(after));
        assert!(s.ended(after));
        assert!(!s.valid(after));
    }

    #[test]
    fn a_canceled_subscription_has_ended() {
        let s = snapshot(json!({"status": "canceled", "ended_at": NOW - 10}));
        assert!(s.canceled());
        assert!(s.ended(now()));
        assert!(!s.on_grace_period(now()));
        assert!(!s.active(now()));
        assert!(!s.valid(now()));
        assert_eq!(s.ends_at().unwrap().timestamp(), NOW - 10);
    }

    #[test]
    fn unpaid_states_are_not_active() {
        for status in ["past_due", "unpaid", "incomplete", "incomplete_expired", "paused"] {
            let s = snapshot(json!({ "status": status }));
            assert!(!s.active(now()), "{status}");
            assert!(!s.valid(now()), "{status}");
        }
        assert!(snapshot(json!({"status": "past_due"})).past_due());
        assert!(snapshot(json!({"status": "incomplete"})).incomplete());
        assert!(snapshot(json!({"status": "incomplete_expired"})).incomplete());
    }

    #[test]
    fn a_snapshot_survives_a_round_trip_through_storage() {
        let s = snapshot(json!({"status": "trialing", "trial_end": NOW + 60}));
        let stored = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<SubscriptionSnapshot>(&stored).unwrap(), s);
        assert!(s.has_price("price_pro"));
    }
}
