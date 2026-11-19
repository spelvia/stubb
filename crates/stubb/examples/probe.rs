//! Prints what `stubb::probe` finds for every supported harness.

#[tokio::main(flavor = "current_thread")]
async fn main() {
    for report in stubb::probe::probe_all().await {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("serializable")
        );
    }
}
