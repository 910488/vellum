//! `fake-acp-agent`: runs [`FakeAcpAgent`] over real stdio so the process
//! supervisor and adapters can be exercised against a spawned child.
//!
//! stdout carries protocol only. Diagnostics go to stderr, matching the
//! constraint Vellum places on every native harness.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use vellum_harness_testkit::{FakeAcpAgent, FakeAgentScript, Reply};

#[tokio::main]
async fn main() {
    let mut agent = FakeAcpAgent::new(FakeAgentScript::from_process_args());
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(frame) = serde_json::from_str(&line) else {
            eprintln!("fake-acp-agent: ignoring malformed frame");
            continue;
        };
        let Reply(frames) = agent.handle(&frame);
        for frame in frames {
            if stdout
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }
        }
        if stdout.flush().await.is_err() {
            return;
        }
    }
}
