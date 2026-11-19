//! Prints what `stubb::discovery::probe` finds for every built-in agent.

#[tokio::main(flavor = "current_thread")]
async fn main() {
    for report in stubb::discovery::probe_all().await {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("serializable")
        );
    }
}
