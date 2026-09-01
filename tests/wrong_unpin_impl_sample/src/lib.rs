use std::pin::Pin;

pub struct Wrapper<T, U> {
    pinned: T,
    data: U,
}

impl<T, U> Wrapper<T, U> {
    pub fn project(self: Pin<&mut Self>) -> Pin<&mut T> {
        unsafe { Pin::new_unchecked(&mut self.get_unchecked_mut().pinned) }
    }
}

impl<T, U: Unpin> Unpin for Wrapper<T, U> {}
