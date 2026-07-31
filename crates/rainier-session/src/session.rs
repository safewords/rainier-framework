//! The session bag — [`Session`] and the [`SessionData`] that is persisted.

use std::sync::{Arc, Mutex};

use rainier_support::Result;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The key the CSRF token is kept under.
pub const TOKEN_KEY: &str = "_token";

/// Generate a session id: 256 bits of randomness, hex-encoded.
///
/// A session id is a bearer credential, so it comes from the OS CSPRNG rather
/// than a counter or a timestamp — anything guessable here is an account
/// takeover.
pub fn generate_session_id() -> String {
    use rand::RngCore;

    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// What a [`SessionStore`](crate::SessionStore) holds for one session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionData {
    /// The values themselves.
    #[serde(default)]
    pub values: Map<String, Value>,

    /// Keys flashed during the request that just ended, and therefore
    /// readable during the next one and deleted at the end of it.
    #[serde(default)]
    pub flash: Vec<String>,
}

#[derive(Debug)]
struct State {
    id: String,
    values: Map<String, Value>,
    /// Flashed by a previous request: readable now, deleted when this one ends
    /// unless [`Session::keep`] rescues it.
    flash_old: Vec<String>,
    /// Flashed by this request: readable in the next one.
    flash_new: Vec<String>,
    dirty: bool,
    /// Ids superseded by [`Session::regenerate`] or
    /// [`Session::invalidate`], for the middleware to delete.
    superseded: Vec<String>,
}

/// A request's session.
///
/// Cheap to clone — every clone is the same underlying bag, which is what lets
/// the middleware hold one while the handler writes through another.
///
/// Reached from a handler with
/// [`request.session()`](crate::SessionRequestExt::session), which is `Some`
/// exactly when the [`StartSession`](crate::StartSession) middleware ran.
#[derive(Clone)]
pub struct Session {
    state: Arc<Mutex<State>>,
}

impl Session {
    /// A fresh, empty session with a new id.
    pub fn new() -> Self {
        Self::restore(generate_session_id(), SessionData::default())
    }

    /// A session loaded from a store.
    pub fn restore(id: impl Into<String>, data: SessionData) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                id: id.into(),
                values: data.values,
                flash_old: data.flash,
                flash_new: Vec::new(),
                dirty: false,
                superseded: Vec::new(),
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A panic in a handler must not make every later request fail on a
        // poisoned lock; the bag's invariants do not depend on the panicking
        // section having finished.
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The session id, as it appears in the cookie.
    pub fn id(&self) -> String {
        self.lock().id.clone()
    }

    /// Whether anything has changed and the session needs writing back.
    pub fn is_dirty(&self) -> bool {
        self.lock().dirty
    }

    // --- reading -----------------------------------------------------------

    /// A value, deserialised.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        serde_json::from_value(self.lock().values.get(key)?.clone()).ok()
    }

    /// A value as a string, if it is one.
    pub fn string(&self, key: &str) -> Option<String> {
        match self.lock().values.get(key)? {
            Value::String(value) => Some(value.clone()),
            other => Some(other.to_string()),
        }
    }

    /// The raw value.
    pub fn value(&self, key: &str) -> Option<Value> {
        self.lock().values.get(key).cloned()
    }

    /// Whether the key is present and not null.
    pub fn has(&self, key: &str) -> bool {
        matches!(self.lock().values.get(key), Some(value) if !value.is_null())
    }

    /// Every value.
    pub fn all(&self) -> Map<String, Value> {
        self.lock().values.clone()
    }

    /// How many values are held.
    pub fn len(&self) -> usize {
        self.lock().values.len()
    }

    /// Whether the session holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // --- writing -----------------------------------------------------------

    /// Store a value.
    pub fn put(&self, key: impl Into<String>, value: impl Serialize) -> Result<()> {
        let value = serde_json::to_value(value)?;
        let mut state = self.lock();
        state.values.insert(key.into(), value);
        state.dirty = true;
        Ok(())
    }

    /// Read a value and remove it in one step.
    pub fn pull<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let mut state = self.lock();
        let value = state.values.remove(key)?;
        state.dirty = true;
        serde_json::from_value(value).ok()
    }

    /// Remove a value.
    pub fn forget(&self, key: &str) {
        let mut state = self.lock();
        if state.values.remove(key).is_some() {
            state.dirty = true;
        }
    }

    /// Remove every value, keeping the id.
    pub fn flush(&self) {
        let mut state = self.lock();
        state.values.clear();
        state.flash_new.clear();
        state.flash_old.clear();
        state.dirty = true;
    }

    // --- flash -------------------------------------------------------------

    /// Store a value for the **next** request only.
    ///
    /// The redirect-then-show-a-message pattern: the value survives exactly
    /// one further request and is then deleted, without anything having to
    /// remember to clean it up.
    pub fn flash(&self, key: impl Into<String>, value: impl Serialize) -> Result<()> {
        let key = key.into();
        let value = serde_json::to_value(value)?;

        let mut state = self.lock();
        state.values.insert(key.clone(), value);
        if !state.flash_new.contains(&key) {
            state.flash_new.push(key);
        }
        state.dirty = true;
        Ok(())
    }

    /// Keep the named flashed values for one further request.
    ///
    /// What a redirect chain needs: without it, a value flashed before a
    /// redirect that itself redirects is gone before anything renders it.
    pub fn keep(&self, keys: &[&str]) {
        let mut state = self.lock();
        for key in keys {
            let key = (*key).to_string();
            if state.flash_old.contains(&key) && !state.flash_new.contains(&key) {
                state.flash_new.push(key);
                state.dirty = true;
            }
        }
    }

    /// Keep every flashed value for one further request.
    pub fn reflash(&self) {
        let mut state = self.lock();
        let carried: Vec<String> =
            state.flash_old.iter().filter(|key| !state.flash_new.contains(key)).cloned().collect();

        if !carried.is_empty() {
            state.flash_new.extend(carried);
            state.dirty = true;
        }
    }

    // --- identity ----------------------------------------------------------

    /// The CSRF token, minted on first use.
    pub fn token(&self) -> String {
        if let Some(Value::String(token)) = self.lock().values.get(TOKEN_KEY) {
            return token.clone();
        }

        let token = generate_session_id();
        let mut state = self.lock();
        state.values.insert(TOKEN_KEY.to_string(), Value::String(token.clone()));
        state.dirty = true;
        token
    }

    /// Give the session a new id, keeping its contents.
    ///
    /// **Call this on login.** Without it, an attacker who can set a victim's
    /// session cookie before they authenticate ends up holding a cookie for
    /// their authenticated session — session fixation, and it is invisible in
    /// testing because everything works.
    pub fn regenerate(&self) {
        let mut state = self.lock();
        let previous = std::mem::replace(&mut state.id, generate_session_id());
        state.superseded.push(previous);
        state.dirty = true;
    }

    /// Give the session a new id and throw its contents away. Logging out.
    pub fn invalidate(&self) {
        self.flush();
        self.regenerate();
    }

    // --- persistence -------------------------------------------------------

    /// Ids this session has left behind, for the store to delete.
    pub fn superseded_ids(&self) -> Vec<String> {
        self.lock().superseded.clone()
    }

    /// Age the flash data and produce what should be written back.
    ///
    /// Called once by the middleware, after the handler has run: values
    /// flashed by a *previous* request are dropped unless they were kept, and
    /// values flashed by *this* one become the next request's.
    pub fn age_and_take(&self) -> SessionData {
        let mut state = self.lock();

        let expired: Vec<String> =
            state.flash_old.iter().filter(|key| !state.flash_new.contains(key)).cloned().collect();

        for key in expired {
            state.values.remove(&key);
        }

        let flash = std::mem::take(&mut state.flash_new);
        state.flash_old = flash.clone();

        SessionData { values: state.values.clone(), flash }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

/// Deliberately opaque: a session holds whatever the application put in it,
/// which in practice includes user ids and CSRF tokens.
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("Session")
            .field("id", &"<redacted>")
            .field("values", &state.values.len())
            .field("dirty", &state.dirty)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_long_and_unpredictable() {
        let a = generate_session_id();
        let b = generate_session_id();

        assert_eq!(a.len(), 64, "256 bits, hex-encoded");
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn values_round_trip() {
        let session = Session::new();
        session.put("user_id", 42u64).unwrap();
        session.put("name", "Ada").unwrap();

        assert_eq!(session.get::<u64>("user_id"), Some(42));
        assert_eq!(session.string("name").as_deref(), Some("Ada"));
        assert!(session.has("user_id"));
        assert_eq!(session.len(), 2);
    }

    #[test]
    fn a_fresh_session_is_not_dirty() {
        assert!(!Session::new().is_dirty(), "nothing to write back yet");
    }

    #[test]
    fn writing_marks_it_dirty_and_reading_does_not() {
        let session = Session::restore("id", SessionData::default());
        assert!(!session.is_dirty());

        session.get::<u64>("absent");
        session.has("absent");
        assert!(!session.is_dirty(), "reads must not force a write");

        session.put("x", 1).unwrap();
        assert!(session.is_dirty());
    }

    #[test]
    fn forgetting_an_absent_key_is_not_a_change() {
        let session = Session::restore("id", SessionData::default());
        session.forget("absent");

        assert!(!session.is_dirty());
    }

    #[test]
    fn pull_reads_and_removes() {
        let session = Session::new();
        session.put("once", "value").unwrap();

        assert_eq!(session.pull::<String>("once").as_deref(), Some("value"));
        assert!(!session.has("once"));
    }

    #[test]
    fn flushing_empties_it_but_keeps_the_id() {
        let session = Session::new();
        let id = session.id();
        session.put("a", 1).unwrap();

        session.flush();

        assert!(session.is_empty());
        assert_eq!(session.id(), id);
    }

    #[test]
    fn a_flashed_value_survives_exactly_one_more_request() {
        // Request 1 flashes it.
        let session = Session::new();
        session.flash("status", "Saved.").unwrap();
        assert_eq!(session.string("status").as_deref(), Some("Saved."), "readable immediately");
        let after_one = session.age_and_take();

        // Request 2 reads it.
        let session = Session::restore("id", after_one);
        assert_eq!(session.string("status").as_deref(), Some("Saved."));
        let after_two = session.age_and_take();

        // Request 3 does not.
        let session = Session::restore("id", after_two);
        assert!(!session.has("status"), "flash data must not linger");
    }

    #[test]
    fn keep_rescues_a_flashed_value_for_one_more_request() {
        let session = Session::new();
        session.flash("status", "Saved.").unwrap();
        let data = session.age_and_take();

        // The next request redirects again, so it keeps the message.
        let session = Session::restore("id", data);
        session.keep(&["status"]);
        let data = session.age_and_take();

        let session = Session::restore("id", data);
        assert_eq!(session.string("status").as_deref(), Some("Saved."));
    }

    #[test]
    fn reflash_keeps_everything() {
        let session = Session::new();
        session.flash("a", 1).unwrap();
        session.flash("b", 2).unwrap();
        let data = session.age_and_take();

        let session = Session::restore("id", data);
        session.reflash();
        let data = session.age_and_take();

        let session = Session::restore("id", data);
        assert!(session.has("a") && session.has("b"));
    }

    #[test]
    fn keeping_an_unflashed_key_does_nothing() {
        let session = Session::new();
        session.put("permanent", 1).unwrap();
        session.keep(&["permanent"]);

        let data = session.age_and_take();
        assert!(data.values.contains_key("permanent"));
        assert!(data.flash.is_empty(), "a normal value is not flash data");
    }

    #[test]
    fn a_permanent_value_is_not_aged_out() {
        let session = Session::new();
        session.put("user_id", 42u64).unwrap();
        session.flash("status", "hi").unwrap();

        let data = session.age_and_take();
        let session = Session::restore("id", data);
        let data = session.age_and_take();

        assert!(data.values.contains_key("user_id"));
        assert!(!data.values.contains_key("status"));
    }

    #[test]
    fn the_token_is_minted_once_and_then_stable() {
        let session = Session::new();
        let first = session.token();

        assert_eq!(session.token(), first, "a second read must not rotate it");
        assert_eq!(first.len(), 64);
        assert!(session.is_dirty(), "minting it needs writing back");
    }

    #[test]
    fn regenerating_changes_the_id_and_keeps_the_data() {
        let session = Session::new();
        let before = session.id();
        session.put("user_id", 42u64).unwrap();

        session.regenerate();

        assert_ne!(session.id(), before);
        assert_eq!(session.get::<u64>("user_id"), Some(42), "login must not lose the session");
        assert_eq!(session.superseded_ids(), vec![before], "the old row should be deleted");
    }

    #[test]
    fn invalidating_changes_the_id_and_drops_the_data() {
        let session = Session::new();
        let before = session.id();
        session.put("user_id", 42u64).unwrap();

        session.invalidate();

        assert_ne!(session.id(), before);
        assert!(session.is_empty());
        assert_eq!(session.superseded_ids(), vec![before]);
    }

    #[test]
    fn clones_share_one_bag() {
        let session = Session::new();
        let handle = session.clone();

        handle.put("written-through-the-clone", true).unwrap();

        assert!(session.has("written-through-the-clone"));
    }

    #[test]
    fn debug_does_not_disclose_the_id_or_the_values() {
        let session = Session::new();
        session.put("secret", "hunter2").unwrap();

        let rendered = format!("{session:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(!rendered.contains(&session.id()), "{rendered}");
    }

    #[test]
    fn data_round_trips_through_json() {
        let session = Session::new();
        session.put("user_id", 42u64).unwrap();
        session.flash("status", "Saved.").unwrap();

        let data = session.age_and_take();
        let encoded = serde_json::to_string(&data).unwrap();
        let decoded: SessionData = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, data);
    }
}
