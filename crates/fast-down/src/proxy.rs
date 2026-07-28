//! Proxy selection for outgoing HTTP requests.
//!
//! This module defines the [`Proxy`] enum, which tells a `FastDownPuller`
//! how to route its connections: no proxy, the system proxy (honoring
//! platform/Environment settings), or a caller-supplied custom proxy URL.

use std::ops::Deref;

/// Proxy configuration for outgoing HTTP requests.
///
/// Supports no proxy, system-configured proxy, or a custom proxy URL.
/// The `Custom` variant is unconstrained but is typically used with string-like types.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Proxy<T> {
    No,
    #[default]
    System,
    Custom(T),
}

impl<T> Proxy<T> {
    /// Transform the inner custom value, leaving [`Proxy::No`] and
    /// [`Proxy::System`] unchanged.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Proxy<U> {
        match self {
            Self::No => Proxy::No,
            Self::System => Proxy::System,
            Self::Custom(t) => Proxy::Custom(f(t)),
        }
    }

    /// Borrow the inner custom value as a reference, leaving [`Proxy::No`] and
    /// [`Proxy::System`] unchanged. Requires `T: Deref`.
    pub fn as_deref(&self) -> Proxy<&T::Target>
    where
        T: Deref,
    {
        match self {
            Self::No => Proxy::No,
            Self::System => Proxy::System,
            Self::Custom(t) => Proxy::Custom(&**t),
        }
    }

    /// Borrow the inner custom value as a shared reference, leaving
    /// [`Proxy::No`] and [`Proxy::System`] unchanged.
    pub const fn as_ref(&self) -> Proxy<&T> {
        match self {
            Self::No => Proxy::No,
            Self::System => Proxy::System,
            Self::Custom(t) => Proxy::Custom(t),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_transforms_custom_only() {
        assert_eq!(Proxy::<i32>::No.map(|x| x + 1), Proxy::No);
        assert_eq!(Proxy::<i32>::System.map(|x| x + 1), Proxy::System);
        assert_eq!(Proxy::Custom(5).map(|x| x + 1), Proxy::Custom(6));
    }

    #[test]
    fn as_deref_borrows_custom() {
        let p: Proxy<String> = Proxy::Custom("http".to_string());
        assert_eq!(p.as_deref(), Proxy::Custom("http"));
        assert_eq!(Proxy::<String>::No.as_deref(), Proxy::No);
        assert_eq!(Proxy::<String>::System.as_deref(), Proxy::System);
    }

    #[test]
    fn as_ref_borrows_custom() {
        let p: Proxy<i32> = Proxy::Custom(7);
        assert_eq!(p.as_ref(), Proxy::Custom(&7));
        assert_eq!(Proxy::<i32>::No.as_ref(), Proxy::No);
        assert_eq!(Proxy::<i32>::System.as_ref(), Proxy::System);
    }

    #[test]
    fn default_is_system() {
        assert_eq!(Proxy::<String>::default(), Proxy::System);
    }

    #[test]
    fn copy_and_equality() {
        let p = Proxy::Custom(3);
        let q = p; // Proxy is Copy
        assert_eq!(p, q);
    }
}
