use fast_down::ProgressEntry;
use std::fmt::Debug;

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    PrefetchError(String),
    Pulling(usize),
    PullError(usize, String),
    PullTimeout(usize),
    PullProgress(usize, ProgressEntry),
    Pushing(usize, ProgressEntry),
    PushError(usize, ProgressEntry, String),
    PushProgress(usize, ProgressEntry),
    Flushing,
    FlushError(String),
    Finished(usize),
}

impl<RE: Debug, WE: Debug> From<&fast_down::Event<RE, WE>> for Event {
    fn from(event: &fast_down::Event<RE, WE>) -> Self {
        match event {
            fast_down::Event::Pulling(id) => Self::Pulling(*id),
            fast_down::Event::PullError(id, e) => Self::PullError(*id, format!("{e:?}")),
            fast_down::Event::PullTimeout(id) => Self::PullTimeout(*id),
            fast_down::Event::PullProgress(id, range) => Self::PullProgress(*id, range.clone()),
            fast_down::Event::Pushing(id, range) => Self::Pushing(*id, range.clone()),
            fast_down::Event::PushError(id, range, e) => {
                Self::PushError(*id, range.clone(), format!("{e:?}"))
            }
            fast_down::Event::PushProgress(id, range) => Self::PushProgress(*id, range.clone()),
            fast_down::Event::Flushing => Self::Flushing,
            fast_down::Event::FlushError(e) => Self::FlushError(format!("{e:?}")),
            fast_down::Event::Finished(id) => Self::Finished(*id),
        }
    }
}
