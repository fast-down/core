use std::pin::Pin;
use std::task::{Context, Poll};

pub struct ForceSend<F>(pub F);

#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl<F> Send for ForceSend<F> {}

impl<F: Future> Future for ForceSend<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe { self.map_unchecked_mut(|s| &mut s.0).poll(cx) }
    }
}

pub trait ForceSendExt {
    fn force_send(self) -> ForceSend<Self>
    where
        Self: Sized,
    {
        ForceSend(self)
    }
}

impl<F> ForceSendExt for F {
    fn force_send(self) -> ForceSend<Self> {
        ForceSend(self)
    }
}
