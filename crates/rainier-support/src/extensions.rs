//! A type-keyed bag of values — [`Extensions`].
//!
//! The framework needs several "attach an arbitrary value and get it back with
//! its type intact" maps: per-request attributes (the authenticated user, the
//! matched route, resolved route-model bindings), and the container's binding
//! table. Rust has no reflection, so the key is the value's [`TypeId`] and the
//! value is a `Box<dyn Any>` — which is exactly enough, because the caller
//! always knows the type it wants back.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;

/// A map from a type to one value of that type.
///
/// Only one value per type can be stored; inserting again replaces it. Wrap in
/// a newtype when you need two values of the same underlying type with
/// different meanings (e.g. `struct RequestId(String)` vs `struct TraceId(String)`).
#[derive(Default)]
pub struct Extensions {
    map: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl Extensions {
    /// An empty bag.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store `value`, returning whatever value of the same type it replaced.
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) -> Option<T> {
        self.map
            .insert(TypeId::of::<T>(), Box::new(value))
            .and_then(|prev| prev.downcast().ok().map(|boxed| *boxed))
    }

    /// Borrow the stored `T`, if there is one.
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.map.get(&TypeId::of::<T>()).and_then(|v| v.downcast_ref())
    }

    /// Mutably borrow the stored `T`, if there is one.
    pub fn get_mut<T: Send + Sync + 'static>(&mut self) -> Option<&mut T> {
        self.map.get_mut(&TypeId::of::<T>()).and_then(|v| v.downcast_mut())
    }

    /// Remove and return the stored `T`.
    pub fn remove<T: Send + Sync + 'static>(&mut self) -> Option<T> {
        self.map.remove(&TypeId::of::<T>()).and_then(|v| v.downcast().ok().map(|boxed| *boxed))
    }

    /// Whether a value of type `T` is present.
    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<T>())
    }

    /// Borrow the stored `T`, inserting the value from `default` first if it
    /// is missing.
    pub fn get_or_insert_with<T: Send + Sync + 'static>(
        &mut self,
        default: impl FnOnce() -> T,
    ) -> &mut T {
        self.map
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::new(default()))
            .downcast_mut()
            .expect("extensions entry is keyed by its own TypeId")
    }

    /// How many values are stored.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the bag holds nothing.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Drop everything.
    pub fn clear(&mut self) {
        self.map.clear();
    }
}

impl fmt::Debug for Extensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The values are opaque `dyn Any`, so the count is all we can show.
        f.debug_struct("Extensions").field("len", &self.map.len()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct UserId(u64);
    #[derive(Debug, PartialEq)]
    struct TraceId(String);

    #[test]
    fn round_trips_by_type() {
        let mut ext = Extensions::new();
        ext.insert(UserId(7));
        ext.insert(TraceId("abc".into()));

        assert_eq!(ext.get::<UserId>(), Some(&UserId(7)));
        assert_eq!(ext.get::<TraceId>(), Some(&TraceId("abc".into())));
        assert_eq!(ext.len(), 2);
    }

    #[test]
    fn inserting_the_same_type_replaces_and_returns_the_old_value() {
        let mut ext = Extensions::new();
        ext.insert(UserId(1));
        assert_eq!(ext.insert(UserId(2)), Some(UserId(1)));
        assert_eq!(ext.get::<UserId>(), Some(&UserId(2)));
    }

    #[test]
    fn missing_types_read_as_none() {
        let ext = Extensions::new();
        assert!(ext.get::<UserId>().is_none());
        assert!(!ext.contains::<UserId>());
        assert!(ext.is_empty());
    }

    #[test]
    fn remove_hands_the_value_back() {
        let mut ext = Extensions::new();
        ext.insert(UserId(3));
        assert_eq!(ext.remove::<UserId>(), Some(UserId(3)));
        assert!(ext.get::<UserId>().is_none());
    }

    #[test]
    fn get_or_insert_with_only_builds_once() {
        let mut ext = Extensions::new();
        assert_eq!(*ext.get_or_insert_with(|| UserId(1)), UserId(1));
        assert_eq!(*ext.get_or_insert_with(|| UserId(99)), UserId(1));
    }
}
