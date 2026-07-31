//! The session, as authentication sees it.
//!
//! The bag itself lives in [`rainier_session`] — one session concept for the
//! whole framework rather than a second, narrower one here. This module is the
//! seam: the key the authenticated user is stored under, and the re-exports
//! that keep `rainier_auth::…` imports working.

pub use rainier_session::{
    generate_session_id, MemorySessionStore, Session, SessionData, SessionStore,
};

/// The session key holding the authenticated user's identifier.
///
/// Underscore-prefixed by convention, marking it as the framework's rather
/// than the application's — an application storing its own `user_id` for
/// unrelated reasons must not collide with the guard.
pub const AUTH_KEY: &str = "_auth_id";
