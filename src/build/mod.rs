//! Builds of Dockerfiles: the instructions read as the reference frontend reads them,
//! each step run in a root of its own and turned into a layer, the result installed as
//! a local image.

use std::cell::Cell;

pub mod cache;
pub mod copy;
pub mod dispatch;
pub mod dockerfile;
pub mod exec;
pub mod ignore;
pub mod image;
pub mod layer;
pub mod plan;
pub mod root;
pub mod sandbox;
pub mod walk;

/// The stack of `deep`: trees and texts nest as deep as an image or a Dockerfile makes
/// them, and a thread of the runtime has 2 MiB, which an overflow ends the whole
/// service with.
const DEEP_STACK: usize = 256 << 20;

thread_local! {
    static DEEP: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` on a thread with a large stack, or right here on one already.
pub fn deep<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    if DEEP.get() {
        return f();
    }
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .name("nspawn-deep".to_string())
            .stack_size(DEEP_STACK)
            .spawn_scoped(s, || {
                DEEP.set(true);
                f()
            })
            .expect("starting a thread")
            .join()
            .unwrap_or_else(|e| std::panic::resume_unwind(e))
    })
}
