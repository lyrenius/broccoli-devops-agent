//! Minimal binary entry point for Broccoli DevOps Agent.
//!
//! The current entry point only wires the in-memory Store to the Top Scheduler. It proves that the
//! asynchronous runtime and dependency boundaries can initialize successfully; it does not start
//! collection, call a model, or execute machine operations.

#![forbid(unsafe_code)]

use std::sync::Arc;

use broccoli_devops_agent::scheduler::TopScheduler;
use broccoli_devops_agent::store::memory::InMemoryStateStore;

/// Initializes the first architecture scaffold and prints the current scheduler mode.
///
/// This function only creates in-process objects. It does not read configuration, access the
/// network, or cause external side effects. A later startup flow will wire concrete Collector,
/// Snapshot View Builder, Agent Team, Agents Platform, and Scheduler Policy implementations here
/// through the `TopScheduler::with_*` builder methods.
#[tokio::main]
async fn main() {
    let store = Arc::new(InMemoryStateStore::new());
    let scheduler = TopScheduler::new(store);

    println!(
        "Broccoli DevOps Agent architecture scaffold loaded: {:?}",
        scheduler.mode().await
    );
}
