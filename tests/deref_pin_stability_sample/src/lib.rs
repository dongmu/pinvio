use std::ops::{Deref, DerefMut};
use std::pin::Pin;

pub struct SwappingPtr<T> {
    value: T,
    alt: T,
    use_alt: bool,
}

impl<T> SwappingPtr<T> {
    pub fn new(a: T, b: T) -> Self {
        SwappingPtr { value: a, alt: b, use_alt: false }
    }
}

impl<T> Deref for SwappingPtr<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for SwappingPtr<T> {
    fn deref_mut(&mut self) -> &mut T {
        std::mem::swap(&mut self.value, &mut self.alt);
        self.use_alt = !self.use_alt;
        &mut self.value
    }
}

pub fn pin_it<T>(p: &mut SwappingPtr<T>) -> Pin<&mut SwappingPtr<T>> {
    unsafe { Pin::new_unchecked(p) }
}
