use std::ops::Deref;

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Proxy<T> {
    No,
    #[default]
    System,
    Custom(T),
}

impl<T> Proxy<T> {
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Proxy<U> {
        match self {
            Self::No => Proxy::No,
            Self::System => Proxy::System,
            Self::Custom(t) => Proxy::Custom(f(t)),
        }
    }

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

    pub const fn as_ref(&self) -> Proxy<&T> {
        match self {
            Self::No => Proxy::No,
            Self::System => Proxy::System,
            Self::Custom(t) => Proxy::Custom(t),
        }
    }
}
