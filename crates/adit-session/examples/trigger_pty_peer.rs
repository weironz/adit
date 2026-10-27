//! Synthetic PTY peer for the explicitly invoked local trigger acceptance test.
use std::io::{self, BufRead, Write};

fn main() -> io::Result<()> {
    println!("ADIT_READY");
    io::stdout().flush()?;
    for line in io::stdin().lock().lines() {
        match line?.trim() {
            "adit-trigger-submit" => {
                println!("ADIT_DONE");
                io::stdout().flush()?;
            }
            "exit" => break,
            _ => {}
        }
    }
    Ok(())
}
