use std::future::Future;

mod shared_ref;
use shared_ref::*;

pub trait FutureExtra: Future {
    fn shared_ref(self) -> SharedRefOwner<Self> where Self: Sized;
}

impl<T: Future> FutureExtra for T {
    fn shared_ref(self) -> SharedRefOwner<Self> where Self: Sized {
        SharedRefOwner::new(self)
    }
}
