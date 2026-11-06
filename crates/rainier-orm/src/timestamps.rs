//! `created_at` and `updated_at`, stamped on insert when the caller left them
//! empty — Eloquent's `$timestamps`, which every application here was ported
//! from expecting.
//!
//! # Why the ORM does it rather than each caller
//!
//! Applications build rows with `..Default::default()`, and `Default` leaves an
//! `Option<DateTime<Utc>>` as `None`. Without this, every such insert wrote a
//! NULL `created_at`: in lewd.net that was 21 tables, including chats, uploads,
//! media assets, reports and payment events, found on 2026-11-06 when a growth
//! chart could not be rebuilt because no upload knew when it was made. A rule
//! that every call site must remember is a rule that most call sites forgot.
//!
//! # What it touches
//!
//! Only a column named exactly `created_at` or `updated_at`, declared
//! [`ColumnType::Timestamp`], whose value in this insert is NULL. An explicit
//! value is never replaced, so a backfill or an import that sets its own time
//! keeps it. Both columns get the same instant.
//!
//! It runs inside the derived [`Entity::insert_values`](crate::Entity::insert_values),
//! which is what every insert path builds from: `repo::insert`, upserts, bulk
//! inserts and `insert_select`. An upsert's update half therefore stamps too:
//! `updated_at` gets the time of the write, which is what it means.

use sea_query::Value;

use crate::{ColumnType, Entity};

/// The timestamp columns this fills.
const STAMPED: [&str; 2] = ["created_at", "updated_at"];

/// Replace a NULL `created_at` / `updated_at` in `pairs` with now. Called by
/// the `Entity` derive; not normally called directly.
pub fn stamp_insert_timestamps<E: Entity>(pairs: &mut [(&'static str, Value)]) {
    let mut now = None;
    for (column, value) in pairs.iter_mut() {
        if !STAMPED.contains(column) || !matches!(value, Value::ChronoDateTimeUtc(None)) {
            continue;
        }
        let declared = E::columns().iter().find(|c| c.name == *column).map(|c| c.ty);
        if declared != Some(ColumnType::Timestamp) {
            continue;
        }
        let at = *now.get_or_insert_with(chrono::Utc::now);
        *value = at.into();
    }
}
