#[inline(never)]
pub fn bad() { std::hint::black_box(()); }

pub fn invoke<F: FnOnce()>(f: F) { f(); }

#[inline(never)]
pub fn indirect() { bad(); }
