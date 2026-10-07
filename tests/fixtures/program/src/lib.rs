#![allow(dead_code, unused_variables)]

fn bad() { std::hint::black_box(()); }
fn other_bad() { std::hint::black_box(()); }
fn wrapper() { bad(); }
fn offload<F: FnOnce()>(f: F) { f(); }
fn invoke<F: FnOnce()>(f: F) { f(); }
fn recursive(n: u32) { if n == 0 { bad(); } else { recursive(n - 1); } }
fn pointer() -> fn() { bad }

pub async fn direct() { bad(); } //~ prohibited
pub async fn indirect() { wrapper(); } //~ prohibited
pub async fn recursion() { recursive(4); } //~ prohibited
pub async fn closure() { let f = || bad(); f(); } //~ prohibited
pub async fn generic() { invoke(|| bad()); } //~ prohibited
pub async fn function_pointer() { let f: fn() = bad; f(); } //~ prohibited
pub async fn returned_pointer() { pointer()(); } //~ prohibited
pub async fn cross_crate() { bridge::transitive(); } //~ prohibited
pub async fn cross_crate_callback() { bridge::callback(|| bad()); } //~ prohibited
pub async fn two_sinks() { bad(); other_bad(); } //~ prohibited //~ prohibited
pub async fn insulated() { offload(|| bad()); }
pub async fn insulated_named() { offload(wrapper); }
pub async fn eager_argument() { offload({ bad(); || () }); } //~ prohibited
pub async fn unused_closure() { let f = || bad(); }
pub async fn unused_pointer() { let f: fn() = bad; }
pub async fn unused_future() { let f = async {}; }
pub fn sync_is_fine() { bad(); }

pub trait Work { fn work(&self); }
pub struct Worker;
impl Work for Worker { fn work(&self) { bad(); } }
fn generic_work<T: Work>(x: &T) { x.work(); }
pub async fn trait_call() { generic_work(&Worker); } //~ prohibited

pub struct BlockingDrop;
impl Drop for BlockingDrop { fn drop(&mut self) { bad(); } }
pub async fn destructor() { let _guard = BlockingDrop; } //~ prohibited
