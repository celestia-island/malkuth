//! Worker supervision: spawn N child processes, restart on crash.
//!
//! Run: `cargo run --example supervised_workers --features worker`

use std::time::Duration;

use malkuth::{
    DrainController, RestartPolicy, ShutdownKind,
    worker::{Supervisor, WorkerSpec},
};

#[tokio::main]
async fn main() {
    // Each worker is `echo hello` — crashes if you pass "crash" as arg.
    let specs = vec![
        WorkerSpec::new("greeter", "test", "echo")
            .args(["hello from worker-0"])
            .policy(RestartPolicy::Permanent),
        WorkerSpec::new("greeter-2", "test", "echo")
            .args(["hello from worker-1"])
            .policy(RestartPolicy::Transient),
    ];

    let drain = DrainController::new();
    let supervisor = Supervisor::new(specs)
        .rate_limit(5, Duration::from_secs(60))
        .cooldown(Duration::from_secs(10));

    // Simulate a shutdown after 3 seconds.
    let drain_clone = drain.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        println!("--- triggering shutdown ---");
        drain_clone.begin_drain(ShutdownKind::Graceful);
    });

    let final_status = supervisor.run(drain).await;
    for w in &final_status {
        println!(
            "worker {}: status={:?} restarts={}",
            w.id, w.status, w.restart_count
        );
    }
}
