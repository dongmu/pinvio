use std::pin::Pin;

pub fn pin_from_ref<T>(val: &mut T) -> Pin<&mut T> {
    unsafe { Pin::new_unchecked(val) }
}
