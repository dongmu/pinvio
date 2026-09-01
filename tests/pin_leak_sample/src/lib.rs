use std::marker::PhantomPinned;

pub struct IoHandle {
    fd: i32,
    _pin: PhantomPinned,
}

impl Drop for IoHandle {
    fn drop(&mut self) {
        unsafe { close(self.fd) };
    }
}

extern "C" {
    fn close(fd: i32) -> i32;
}

pub fn new_handle(fd: i32) -> IoHandle {
    IoHandle { fd, _pin: PhantomPinned }
}
