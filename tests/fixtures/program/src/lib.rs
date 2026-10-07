#![allow(dead_code, unused_variables, unknown_lints)]

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

fn good() {}
fn identity(f: fn()) -> fn() { f }
pub async fn separate_arguments() { let _unused = identity(bad); identity(good)(); }
pub async fn separate_fields() { let pair: (fn(), fn()) = (bad, good); pair.1(); }
pub async fn field_called() { let pair: (fn(), fn()) = (bad, good); pair.0(); } //~ prohibited
pub async fn captured_pointer() { let f: fn() = bad; let closure = move || f(); closure(); } //~ prohibited
pub async fn closure_pointer() { let f: fn() = || bad(); f(); } //~ prohibited
pub fn future_capture() { let f: fn() = bad; let _future = async move { f(); }; } //~ prohibited
pub fn future_capture_safe() { let pair: (fn(), fn()) = (bad, good); let _future = async move { pair.1(); }; }

pub struct CustomFuture;
impl std::future::Future for CustomFuture {
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<()> { bad(); std::task::Poll::Ready(()) } //~ prohibited
}

#[allow(disallowed_from_async)]
pub async fn explicitly_allowed() { bad(); }

pub async fn unknown_pointer(f: fn()) { f(); } //~ incomplete
pub async fn dynamic_dispatch(x: &dyn Work) { x.work(); } //~ incomplete

pub async fn vector_direct() -> Vec<u64> { Vec::new() } //~ prohibited
pub async fn vector_callback() -> Vec<u64> { std::iter::repeat(()).take(5).flat_map(|()| Vec::<u64>::new()).collect() } //~ prohibited //~ incomplete
pub async fn async_closure() { let f = async || { bad(); }; f().await; } //~ prohibited
pub async fn nested_async() { offload(|| { let _f = async { bad(); }; }); } //~ prohibited
pub async fn nested_captured() { let f: fn() = bad; offload(move || { let _f = async move { f(); }; }); } //~ prohibited
pub async fn repeated_pointer() { let functions = [bad as fn(); 3]; functions[1](); } //~ prohibited
pub async fn partially_unknown(f: fn(), choose: bool) { let f = if choose { f } else { good }; f(); } //~ incomplete
pub fn future_factory_capture() { let f = pointer(); let _future = async move { f(); }; } //~ prohibited

fn split_execution<A: FnOnce(), B: FnOnce()>(a: A, b: B) { a(); b(); }
pub async fn selected_argument_safe() { split_execution(|| bad(), || good()); }
pub async fn other_argument_unsafe() { split_execution(|| good(), || bad()); } //~ prohibited
impl Worker { fn offload<F: FnOnce()>(&self, f: F) { f(); } }
fn receiver() -> Worker { bad(); Worker }
pub async fn method_callback() { Worker.offload(|| bad()); }
pub async fn eager_receiver() { receiver().offload(|| good()); } //~ prohibited
#[expect(disallowed_from_async)]
pub async fn expected_warning() { bad(); }
