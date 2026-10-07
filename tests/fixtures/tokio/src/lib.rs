#![allow(dead_code, unused_variables)]
use tokio::runtime::{Handle, Runtime};

fn helper(handle: &Handle) { handle.block_on(async {}); }
pub async fn direct(handle: &Handle) { handle.block_on(async {}); } //~ prohibited
pub async fn transitive(handle: &Handle) { helper(handle); } //~ prohibited
pub async fn runtime(runtime: &Runtime) { runtime.block_on(async {}); } //~ prohibited
pub async fn futures() { futures::executor::block_on(async {}); } //~ prohibited
pub async fn sleep() { std::thread::sleep(std::time::Duration::ZERO); } //~ prohibited

pub async fn safe(handle: Handle) { //~ incomplete
    tokio::task::spawn_blocking(move || helper(&handle));
}
pub async fn safe_handle(handle: Handle) { //~ incomplete
    handle.clone().spawn_blocking(move || helper(&handle));
}
pub async fn safe_runtime(runtime: &Runtime, handle: Handle) { //~ incomplete
    runtime.spawn_blocking(move || helper(&handle));
}
pub async fn in_place(handle: &Handle) {
    tokio::task::block_in_place(|| helper(handle));
}
pub async fn eager(handle: Handle) { tokio::task::spawn_blocking({ helper(&handle); move || () }); } //~ prohibited //~ incomplete
