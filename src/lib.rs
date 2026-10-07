#![feature(rustc_private)]

extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_lint;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

mod analysis;
mod config;

use rustc_lint::{LateContext, LateLintPass};
use rustc_session::{declare_lint, impl_lint_pass};

dylint_linting::dylint_library!();

declare_lint! {
    pub DISALLOWED_FROM_ASYNC,
    Warn,
    "calls to blocking or otherwise prohibited functions from an async context"
}
declare_lint! {
    pub ASYNC_HYGIENE_INCOMPLETE,
    Warn,
    "async hygiene could not analyze part of a reachable call graph"
}

struct AsyncHygiene;
impl_lint_pass!(AsyncHygiene => [DISALLOWED_FROM_ASYNC, ASYNC_HYGIENE_INCOMPLETE]);

#[unsafe(no_mangle)]
pub fn register_lints(sess: &rustc_session::Session, store: &mut rustc_lint::LintStore) {
    dylint_linting::init_config(sess);
    store.register_lints(&[DISALLOWED_FROM_ASYNC, ASYNC_HYGIENE_INCOMPLETE]);
    store.register_late_lint_pass(Box::new(|_| Box::new(AsyncHygiene)));
}

impl<'tcx> LateLintPass<'tcx> for AsyncHygiene {
    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let config = match dylint_linting::config::<config::Config>("async_hygiene") {
            Ok(config) => config.unwrap_or_default(),
            Err(error) => {
                cx.tcx
                    .dcx()
                    .err(format!("invalid async_hygiene configuration: {error}"));
                return;
            }
        };
        if let Err(error) = config.validate() {
            cx.tcx
                .dcx()
                .err(format!("invalid async_hygiene configuration: {error}"));
            return;
        }
        analysis::check(cx, &config);
    }
}
