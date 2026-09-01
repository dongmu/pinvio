use std::pin::Pin;

pub struct SubWrapper<T> {
    inner: T,
}

impl<T> SubWrapper<T> {
    pub fn pin_project(self: Pin<&mut Self>) -> Pin<&mut T> {
        unsafe { Pin::new_unchecked(&mut self.get_unchecked_mut().inner) }
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}
