#![allow(unconditional_recursion)]

fn expand<T>() { expand::<(T, T)>(); }
pub async fn unbounded_instances() { expand::<()>(); } //~ incomplete
