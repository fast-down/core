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
