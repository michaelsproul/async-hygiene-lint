pub fn transitive() { blocking::indirect(); }
pub fn callback<F: FnOnce()>(f: F) { blocking::invoke(f); }
