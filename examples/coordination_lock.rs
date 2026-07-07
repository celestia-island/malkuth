//! Coordination locks: acquire a file lock and a lease lock.
//!
//! Run: `cargo run --example coordination_lock --features file-lock,lease`
//!
//! `FileLock` is POSIX `flock` based, so this example only runs on Unix.

#[cfg(all(unix, feature = "file-lock"))]
mod unix {
    use std::time::Duration;

    use malkuth::{lease::LeaseLock, lock::FileLock, traits::CoordinationLock};

    pub async fn run() {
        let dir = std::env::temp_dir().join("malkuth-lock-demo");
        std::fs::create_dir_all(&dir).unwrap();

        // --- FileLock (POSIX flock) ---
        let file_lock = FileLock::new(&dir);
        let mut guard = file_lock
            .acquire("shared-resource", Duration::from_secs(30))
            .await
            .expect("file lock failed");
        println!("FileLock acquired: holding for 2s…");
        tokio::time::sleep(Duration::from_secs(2)).await;
        guard.release().await;
        println!("FileLock released.");

        // --- LeaseLock (TTL auto-expiry) ---
        let lease_lock = LeaseLock::new(&dir);
        let mut lease = lease_lock
            .acquire("device-leader", Duration::from_secs(10))
            .await
            .expect("lease failed");
        println!("LeaseLock acquired (TTL=10s): the lease auto-renews in the background.");
        tokio::time::sleep(Duration::from_secs(3)).await;
        lease.release().await;
        println!("LeaseLock released.");
    }
}

#[tokio::main]
async fn main() {
    #[cfg(all(unix, feature = "file-lock"))]
    {
        unix::run().await;
    }
    #[cfg(not(all(unix, feature = "file-lock")))]
    {
        eprintln!(
            "coordination_lock example requires Unix + the `file-lock` feature; \
             nothing to do on this target."
        );
    }
}
