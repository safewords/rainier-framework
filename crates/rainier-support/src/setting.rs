//! Closed-set settings — [`Setting`] and [`setting_enum!`](crate::setting_enum).
//!
//! A driver name is not a string. It is one of a handful of values the code
//! actually knows how to build, and every other string is a mistake. Spelling
//! it as a `String` moves that mistake from the compiler to production:
//!
//! ```text
//! CACHE_DRIVER=redys        # boots fine, caches in-process, forever
//! ```
//!
//! `setting_enum!` declares the closed set once and derives everything that
//! follows from it — the wire spelling, `Display`, `FromStr`, serde, and a
//! parse error that lists what was expected:
//!
//! ```
//! use rainier_support::{setting_enum, Setting};
//!
//! setting_enum! {
//!     /// Where cached values live.
//!     pub enum CacheDriver: "cache driver" {
//!         /// This process only.
//!         #[default]
//!         Memory = "memory",
//!         /// One Redis server.
//!         Redis = "redis",
//!         /// A sharded Redis Cluster.
//!         RedisCluster = "redis-cluster",
//!     }
//! }
//!
//! assert_eq!(CacheDriver::parse("redis-cluster").unwrap(), CacheDriver::RedisCluster);
//! assert_eq!(CacheDriver::default(), CacheDriver::Memory);
//!
//! let err = CacheDriver::parse("redys").unwrap_err();
//! assert_eq!(
//!     err.message(),
//!     "`redys` is not a valid cache driver; expected one of `memory`, `redis`, `redis-cluster`",
//! );
//! ```
//!
//! ## Why parsing fails rather than falling back
//!
//! A default is for a value nobody set. It is not for a value somebody set
//! *wrong* — silently substituting one there means the deployment that typed
//! `redys` runs on an in-process cache, and the first symptom is a rate limiter
//! that lets through `N ×` its limit across `N` instances.
//!
//! Failing at boot puts the error where the mistake was made, with the list of
//! valid values in the message.
//!
//! ## Every setting has a default
//!
//! `setting_enum!` always derives [`Default`], so exactly one variant must
//! carry `#[default]` and the macro will not compile otherwise. That is
//! deliberate: a setting with no default is a setting that has to be named in
//! every environment, and if that is genuinely what you want, it belongs in
//! `Env::require` rather than here.

use crate::{Error, Result};

/// A setting whose values are a closed set of wire spellings.
///
/// Implemented by [`setting_enum!`](crate::setting_enum). Hand-implementing is possible but there is
/// no reason to: everything here follows mechanically from the variant list.
pub trait Setting: Copy + Eq + Sized + 'static {
    /// What this setting is called in an error message — `"cache driver"`.
    ///
    /// Lower case and unqualified, because it is interpolated mid-sentence.
    const SETTING: &'static str;

    /// Every variant, in the order they should be listed to a human.
    const ALL: &'static [Self];

    /// The wire spelling — what appears in `.env`, in the config tree, and in
    /// serialised output.
    fn as_str(&self) -> &'static str;

    /// Parse a wire spelling.
    ///
    /// Tolerant in the ways that are unambiguous and no further: surrounding
    /// whitespace, letter case, and `_` where the canonical spelling uses `-`.
    /// `Redis_Cluster` is what someone means by `redis-cluster`; `redys` is
    /// not, and no amount of guessing makes it so.
    ///
    /// # The exact spelling is tried first, and that ordering is the point
    ///
    /// The `_`-to-`-` tolerance exists for environment variables, where nobody
    /// should have to remember which separator a driver name uses. Applied
    /// *before* an exact match, it silently breaks every variant whose own wire
    /// value contains an underscore: `parse("all_posts")` rewrote its input to
    /// `all-posts` and then failed to find `all_posts`, producing an error that
    /// listed the value it had just rejected among the valid ones.
    ///
    /// That is not only confusing, it is unrecoverable from the caller's side —
    /// and it reaches much further than configuration. Anything decoding an
    /// enum column out of a database goes through here, so a row storing
    /// `all_posts` could be written and never read back. The failure appears at
    /// hydration, long after the write that looked fine.
    ///
    /// So: exact first, tolerance second. Both spellings still work and no
    /// value can be shadowed by the normalisation of another.
    fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();

        if let Some(variant) =
            Self::ALL.iter().copied().find(|variant| variant.as_str().eq_ignore_ascii_case(trimmed))
        {
            return Ok(variant);
        }

        let wanted = trimmed.replace('_', "-");
        Self::ALL
            .iter()
            .copied()
            .find(|variant| variant.as_str().eq_ignore_ascii_case(&wanted))
            .ok_or_else(|| {
                Error::internal(format!(
                    "`{}` is not a valid {}; expected one of {}",
                    raw.trim(),
                    Self::SETTING,
                    Self::options()
                ))
            })
    }

    /// The valid spellings, backtick-quoted and comma-separated.
    ///
    /// For an error message, a `--help` line, or a config-dump command.
    fn options() -> String {
        Self::ALL
            .iter()
            .map(|variant| format!("`{}`", variant.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Declare a closed-set setting: an enum plus everything that follows from it.
///
/// See the [module docs](self) for the shape and the reasoning. The expansion
/// derives `Debug`, `Clone`, `Copy`, `PartialEq`, `Eq`, `Hash`, `Default`,
/// `Serialize` and `Deserialize`, and implements [`Setting`], [`Display`],
/// [`FromStr`] and `AsRef<str>`.
///
/// [`Display`]: std::fmt::Display
/// [`FromStr`]: std::str::FromStr
///
/// The serde impls are written by hand rather than derived, for two reasons.
/// The caller does not need `serde` in its own dependencies — the macro reaches
/// it through this crate — and, more usefully, deserialising goes through
/// [`Setting::parse`], so **the wire format cannot drift from the parser** and
/// a bad value in a config file gets the same message as a bad value in `.env`.
#[macro_export]
macro_rules! setting_enum {
    (
        $(#[$enum_meta:meta])*
        $vis:vis enum $name:ident: $setting:literal {
            $(
                $(#[$variant_meta:meta])*
                $variant:ident = $wire:literal
            ),+ $(,)?
        }
    ) => {
        $(#[$enum_meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
        $vis enum $name {
            $(
                $(#[$variant_meta])*
                $variant,
            )+
        }

        impl $crate::__private::serde::Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> ::std::result::Result<S::Ok, S::Error>
            where
                S: $crate::__private::serde::Serializer,
            {
                serializer.serialize_str(<Self as $crate::Setting>::as_str(self))
            }
        }

        impl<'de> $crate::__private::serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> ::std::result::Result<Self, D::Error>
            where
                D: $crate::__private::serde::Deserializer<'de>,
            {
                use $crate::__private::serde::de::Error as _;

                let raw = <::std::string::String as $crate::__private::serde::Deserialize>::deserialize(
                    deserializer,
                )?;
                <Self as $crate::Setting>::parse(&raw)
                    .map_err(|e| D::Error::custom(e.message()))
            }
        }

        impl $crate::Setting for $name {
            const SETTING: &'static str = $setting;
            const ALL: &'static [Self] = &[$(Self::$variant),+];

            fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $wire,)+
                }
            }
        }

        impl $name {
            /// The wire spelling — what appears in `.env` and in the config
            /// tree.
            ///
            /// Inherent as well as on the trait, so callers do not have to
            /// import `Setting` to print one.
            $vis fn as_str(&self) -> &'static str {
                <Self as $crate::Setting>::as_str(self)
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(<Self as $crate::Setting>::as_str(self))
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::Error;

            fn from_str(raw: &str) -> ::std::result::Result<Self, Self::Err> {
                <Self as $crate::Setting>::parse(raw)
            }
        }

        impl ::std::convert::AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                <Self as $crate::Setting>::as_str(self)
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    setting_enum! {
        /// A stand-in for the real driver enums, which live in the crates that
        /// own the concept.
        pub enum Driver: "cache driver" {
            #[default]
            Memory = "memory",
            Redis = "redis",
            RedisCluster = "redis-cluster",
        }
    }

    setting_enum! {
        /// An enum whose own wire values carry underscores — the shape a
        /// database column holds, as opposed to the hyphenated shape an
        /// environment variable tends to use.
        pub enum Scope: "access scope" {
            #[default]
            AllPosts = "all_posts",
            NewPostsOnly = "new_posts_only",
        }
    }

    #[test]
    fn a_wire_value_containing_an_underscore_parses_as_itself() {
        // The regression this ordering exists for. Normalising `_` to `-`
        // before looking for an exact match meant `all_posts` was rewritten to
        // `all-posts` and then not found — and the error listed `all_posts`
        // among the valid values, having just rejected it.
        //
        // It is not only a configuration concern. Enum columns decode through
        // here, so a row could be written and never read back, and the failure
        // surfaces at hydration rather than at the write that caused it.
        assert_eq!(Scope::parse("all_posts").unwrap(), Scope::AllPosts);
        assert_eq!(Scope::parse("new_posts_only").unwrap(), Scope::NewPostsOnly);
    }

    #[test]
    fn the_underscore_tolerance_still_applies_where_nothing_exact_matches() {
        // The ergonomics the normalisation was added for, unchanged: nobody
        // should have to remember which separator a driver name uses.
        assert_eq!(Driver::parse("redis_cluster").unwrap(), Driver::RedisCluster);
        assert_eq!(Driver::parse("Redis_Cluster").unwrap(), Driver::RedisCluster);
    }

    #[test]
    fn a_round_trip_holds_for_every_variant_of_both_shapes() {
        // The general property, rather than the two cases above: whatever
        // `as_str` writes, `parse` must read. A column round-trips or it does
        // not, and this is the assertion that would have caught the original
        // bug for any enum, not just the ones somebody thought to test.
        for variant in Scope::ALL {
            assert_eq!(Scope::parse(variant.as_str()).unwrap(), *variant);
        }
        for variant in Driver::ALL {
            assert_eq!(Driver::parse(variant.as_str()).unwrap(), *variant);
        }
    }

    #[test]
    fn round_trips_through_its_wire_spelling() {
        for driver in Driver::ALL {
            assert_eq!(Driver::parse(driver.as_str()).unwrap(), *driver);
            assert_eq!(driver.to_string(), driver.as_str());
        }
    }

    #[test]
    fn parsing_is_forgiving_about_case_whitespace_and_underscores() {
        // The three ways a human writes the same value.
        assert_eq!(Driver::parse("  redis-cluster \n").unwrap(), Driver::RedisCluster);
        assert_eq!(Driver::parse("Redis-Cluster").unwrap(), Driver::RedisCluster);
        assert_eq!(Driver::parse("REDIS_CLUSTER").unwrap(), Driver::RedisCluster);
    }

    #[test]
    fn an_unknown_value_names_itself_and_lists_the_alternatives() {
        // The whole point: this is a boot failure, not a silent fallback.
        let err = Driver::parse("redys").unwrap_err();

        assert_eq!(
            err.message(),
            "`redys` is not a valid cache driver; expected one of `memory`, `redis`, `redis-cluster`"
        );
    }

    #[test]
    fn the_reported_value_is_what_was_written_not_what_was_normalised() {
        // Echoing back `REDYS` when the operator typed `  REDYS  ` is fine;
        // echoing back `redys` would send them looking for the wrong line.
        let err = Driver::parse(" REDYS ").unwrap_err();
        assert!(err.message().starts_with("`REDYS` is not"), "{}", err.message());
    }

    #[test]
    fn an_empty_value_is_an_error_rather_than_the_default() {
        // `CACHE_DRIVER=` in a `.env` is a half-finished edit. Treating it as
        // "unset" would hide it; the caller supplies the default by not calling
        // this at all.
        assert!(Driver::parse("").is_err());
    }

    #[test]
    fn serde_uses_the_same_spelling_as_the_parser() {
        // If these ever diverged, a value written by `Config::set` would not
        // read back through `Config::get`.
        let json = serde_json::to_string(&Driver::RedisCluster).unwrap();
        assert_eq!(json, "\"redis-cluster\"");

        let back: Driver = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Driver::RedisCluster);
    }

    #[test]
    fn the_default_is_the_marked_variant() {
        assert_eq!(Driver::default(), Driver::Memory);
    }

    #[test]
    fn from_str_is_the_same_parser() {
        assert_eq!("redis".parse::<Driver>().unwrap(), Driver::Redis);
        assert!("nope".parse::<Driver>().is_err());
    }

    #[test]
    fn options_lists_every_variant_in_declaration_order() {
        assert_eq!(Driver::options(), "`memory`, `redis`, `redis-cluster`");
    }
}
