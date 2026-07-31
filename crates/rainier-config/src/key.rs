//! Typed configuration keys — [`Key`] and [`config_keys!`](crate::config_keys).
//!
//! `config.set("cache.driver", "redis")` has two strings in it and the compiler
//! checks neither. Misspell the path and the write lands somewhere nothing
//! reads; misspell the value and it reads back as a driver that does not exist.
//! Both failures surface far from the line that caused them.
//!
//! A [`Key<T>`] fixes both ends. It is a dotted path that knows what type lives
//! there:
//!
//! ```
//! use rainier_config::{config_keys, Config};
//!
//! config_keys! {
//!     /// How many posts a listing shows.
//!     pub POSTS_PER_PAGE: u64 = "posts.per_page";
//! }
//!
//! let config = Config::new();
//! config.set(POSTS_PER_PAGE, 15).unwrap();
//!
//! // No turbofish: the key says what comes back.
//! let per_page = config.get(POSTS_PER_PAGE).unwrap();
//! assert_eq!(per_page, 15_u64);
//! ```
//!
//! And the mistakes stop compiling:
//!
//! ```compile_fail
//! # use rainier_config::{config_keys, Config};
//! # config_keys! { pub POSTS_PER_PAGE: u64 = "posts.per_page"; }
//! # let config = Config::new();
//! // error: expected `u64`, found `&str`
//! config.set(POSTS_PER_PAGE, "fifteen").unwrap();
//! ```
//!
//! ```compile_fail
//! # use rainier_config::{config_keys, Config};
//! # config_keys! { pub POSTS_PER_PAGE: u64 = "posts.per_page"; }
//! # let config = Config::new();
//! // error: the trait bound `Key<u64>: ConfigKey<String>` is not satisfied
//! let name: Option<String> = config.get(POSTS_PER_PAGE);
//! ```
//!
//! ## Plain strings still work
//!
//! [`ConfigKey<T>`] is implemented for `&str` and `String` for *every* `T`, so
//! `config.get::<u16>("server.port")` reads exactly as it did. That is not
//! grandfathering: a dotted path built at runtime — from a driver name, from a
//! console argument — cannot be a `Key`, and pretending otherwise would just
//! push callers into `format!` plus a cast.
//!
//! The typed form is for the keys an application names in its own source, which
//! is nearly all of them.

use std::marker::PhantomData;

/// A dotted path, and the type stored at it.
///
/// Zero-sized beyond the `&'static str`, and `const`-constructible, so a module
/// of these costs nothing at runtime.
pub struct Key<T> {
    path: &'static str,
    /// `fn() -> T` rather than `T`, so a `Key<T>` is `Send + Sync + Copy`
    /// whatever `T` is — a key is a name, and a name does not inherit the
    /// thread-safety of the value it names.
    marker: PhantomData<fn() -> T>,
}

impl<T> Key<T> {
    /// A key at `path`.
    ///
    /// Prefer [`config_keys!`](crate::config_keys), which keeps the path, the type and the doc
    /// comment on one line.
    pub const fn new(path: &'static str) -> Self {
        Self { path, marker: PhantomData }
    }

    /// The dotted path.
    pub const fn path(&self) -> &'static str {
        self.path
    }

    /// The same path, read as a different type.
    ///
    /// The escape hatch for the cases a single type cannot cover — reading a
    /// key as raw [`Value`](serde_json::Value) to dump it, say. Rare enough
    /// that it should look deliberate at the call site.
    pub const fn as_type<U>(&self) -> Key<U> {
        Key::new(self.path)
    }
}

// Derived impls would demand `T: Clone`/`T: Copy`, which is wrong: a key holds
// no `T`.
impl<T> Clone for Key<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Key<T> {}

impl<T> std::fmt::Debug for Key<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Key({})", self.path)
    }
}

impl<T> std::fmt::Display for Key<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.path)
    }
}

impl<T> PartialEq for Key<T> {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl<T> Eq for Key<T> {}

/// Anything that names a place in the configuration tree.
///
/// The bound on the methods that do not care about the value's type —
/// [`has`](crate::Config::has), [`forget`](crate::Config::forget),
/// [`value`](crate::Config::value).
pub trait ConfigPath {
    /// The dotted path.
    fn path(&self) -> &str;
}

impl ConfigPath for &str {
    fn path(&self) -> &str {
        self
    }
}

impl ConfigPath for String {
    fn path(&self) -> &str {
        self
    }
}

impl ConfigPath for &String {
    fn path(&self) -> &str {
        self
    }
}

impl<T> ConfigPath for Key<T> {
    fn path(&self) -> &str {
        self.path
    }
}

impl<T> ConfigPath for &Key<T> {
    fn path(&self) -> &str {
        self.path
    }
}

/// A path that may hold a `T`.
///
/// The bound on [`get`](crate::Config::get), [`set`](crate::Config::set) and
/// friends. A plain `&str` satisfies it for every `T` — it makes no claim about
/// what is there — while a [`Key<T>`] satisfies it for exactly one, which is
/// what turns a wrong type into a compile error.
pub trait ConfigKey<T>: ConfigPath {}

impl<T> ConfigKey<T> for &str {}
impl<T> ConfigKey<T> for String {}
impl<T> ConfigKey<T> for &String {}
impl<T> ConfigKey<T> for Key<T> {}
impl<T> ConfigKey<T> for &Key<T> {}

/// Declare typed configuration keys.
///
/// One line per key: the name, the type stored there, and the dotted path.
///
/// ```
/// use rainier_config::config_keys;
///
/// config_keys! {
///     /// The application's display name.
///     pub APP_NAME: String = "app.name";
///     /// Whether to render a stack trace on a 500.
///     pub APP_DEBUG: bool = "app.debug";
/// }
///
/// assert_eq!(APP_NAME.path(), "app.name");
/// ```
///
/// Grouping them in a `keys` module next to the section that writes them keeps
/// the path and its one writer in the same file — which is the property that
/// makes a rename a compile error everywhere instead of a silent no-op.
#[macro_export]
macro_rules! config_keys {
    (
        $(
            $(#[$meta:meta])*
            $vis:vis $name:ident: $type:ty = $path:literal;
        )*
    ) => {
        $(
            $(#[$meta])*
            ///
            #[doc = concat!("Reads and writes `", $path, "`.")]
            $vis const $name: $crate::Key<$type> = $crate::Key::new($path);
        )*
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    config_keys! {
        /// A doc comment survives onto the constant.
        pub CACHE_PREFIX: String = "cache.prefix";
        pub POSTS_PER_PAGE: u64 = "posts.per_page";
    }

    #[test]
    fn a_key_carries_its_path() {
        assert_eq!(CACHE_PREFIX.path(), "cache.prefix");
        assert_eq!(POSTS_PER_PAGE.to_string(), "posts.per_page");
        assert_eq!(format!("{POSTS_PER_PAGE:?}"), "Key(posts.per_page)");
    }

    #[test]
    fn a_key_is_copy_and_thread_safe_whatever_it_names() {
        // `PhantomData<fn() -> T>` rather than `PhantomData<T>` is what buys
        // this: a `Key<Rc<()>>` is still `Send`, because it holds no `Rc`.
        fn assert_send_sync_copy<T: Send + Sync + Copy>(_: T) {}

        assert_send_sync_copy(CACHE_PREFIX);
        assert_send_sync_copy(Key::<std::rc::Rc<()>>::new("a.b"));
    }

    #[test]
    fn two_keys_at_the_same_path_are_equal() {
        assert_eq!(CACHE_PREFIX, Key::<String>::new("cache.prefix"));
        assert_ne!(CACHE_PREFIX, Key::<String>::new("cache.other"));
    }

    #[test]
    fn as_type_reuses_the_path() {
        let raw: Key<serde_json::Value> = POSTS_PER_PAGE.as_type();
        assert_eq!(raw.path(), "posts.per_page");
    }

    #[test]
    fn every_spelling_of_a_path_is_accepted() {
        let owned = String::from("a.b");
        assert_eq!(ConfigPath::path(&"a.b"), "a.b");
        assert_eq!(ConfigPath::path(&owned), "a.b");
        assert_eq!(ConfigPath::path(&&owned), "a.b");
        assert_eq!(ConfigPath::path(&CACHE_PREFIX), "cache.prefix");
        assert_eq!(ConfigPath::path(&&CACHE_PREFIX), "cache.prefix");
    }
}
