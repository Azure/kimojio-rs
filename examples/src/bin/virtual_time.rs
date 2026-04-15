//! Virtual Clock Example
//!
//! Demonstrates how to use kimojio's virtual clock for deterministic
//! timing tests. A 60-second sleep completes instantly when virtual
//! time is advanced.

use kimojio::configuration::Configuration;
use kimojio::{Runtime, operations};
use std::future::Future;
use std::time::Duration;

fn main() {
    let mut runtime = Runtime::new(0, Configuration::new());
    let wall_start = std::time::Instant::now();

    let result = runtime.block_on(async move {
        use std::pin::pin;
        use std::task::Poll;

        // Enable the virtual clock for this runtime
        operations::virtual_clock_enable(true);

        println!("Creating a 60-second virtual sleep...");

        let mut sleep = pin!(operations::sleep(Duration::from_secs(60)));

        // Poll once to register the timer with the virtual clock
        futures::future::poll_fn(|cx| {
            let _ = sleep.as_mut().poll(cx);
            Poll::Ready(())
        })
        .await;

        println!(
            "Virtual time: {:?} (timer registered, {} pending)",
            operations::virtual_clock_now(),
            operations::virtual_clock_pending_timers()
        );

        // Advance virtual time by 60 seconds — no real time passes!
        let fired = operations::virtual_clock_advance(Duration::from_secs(60));
        println!(
            "Advanced 60s — {} timer(s) fired, virtual time: {:?}",
            fired,
            operations::virtual_clock_now()
        );

        // The sleep now completes immediately
        sleep.await.unwrap();
        println!("Sleep completed!");
    });

    if let Some(Err(payload)) = result {
        std::panic::resume_unwind(payload);
    }

    println!(
        "\nWall-clock time elapsed: {:?} (for a 60s virtual sleep!)",
        wall_start.elapsed()
    );
}
