//! Deterministic native stdio peers. No commands or model calls are executed.
use serde_json::Value;
use std::io::{self, Write};
mod claude;
mod codex;

fn emit(value: Value) {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "{value}").unwrap();
    out.flush().unwrap();
}
fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("2.1.289 (fixture)");
        return;
    }
    if std::env::args().any(|a| a == "--input-format") {
        claude::run();
    } else {
        codex::run();
    }
}
