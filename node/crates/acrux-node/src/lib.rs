pub mod facts;
pub mod cli;
pub mod gateway;
pub mod genesis;
pub mod lab;
pub mod limits;

/// Validator `i`'s identity: from `ACRUX_IDENTITY_SEED` on a public net (a secret, since these
/// identities hold the net's Hub Registry memberships), else the published development seed. Every
/// run on one genesis, Fault Lab sandboxes included, has the validators its registry names.
pub fn identity_key(i: usize) -> ed25519_dalek::SigningKey {
    let seed = std::env::var("ACRUX_IDENTITY_SEED").unwrap_or_else(|_| "20261116".into());
    ed25519_dalek::SigningKey::from_bytes(&crypto::hash(format!("acrux-local-identity-{seed}-{i}").as_bytes()))
}

/// Raises the calling thread above the machine's normal work: slot timing is in milliseconds.
pub fn raise_thread_priority() {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetCurrentThread() -> *mut std::ffi::c_void;
            fn SetThreadPriority(thread: *mut std::ffi::c_void, priority: i32) -> i32;
        }
        const THREAD_PRIORITY_HIGHEST: i32 = 2;
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
        }
    }
}
pub mod projection;
pub mod protocol;
pub mod storage;
pub mod types;
pub mod view;
pub mod exchange;
pub mod agreement;
pub mod submission;
pub mod observer;

// Crates by concern, re-exported under the module paths the node uses.
pub use acrux_consensus as consensus;
pub use acrux_crypto as crypto;
pub use acrux_execution::{execution, selection, threshold};
pub use acrux_schedule as schedule;
pub use acrux_transport::{profiling, transport};
