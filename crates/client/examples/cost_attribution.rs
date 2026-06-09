//! Non-streaming chat that prints the TokenTrimmer cost attribution.
//!
//! Run: `TOKENTRIMMER_API_KEY=tt_... cargo run -p tt-client --example cost_attribution`

use tt_client::{user, Client};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key = std::env::var("TOKENTRIMMER_API_KEY").unwrap_or_else(|_| "tt_test_local".into());
    let base =
        std::env::var("TOKENTRIMMER_BASE_URL").unwrap_or_else(|_| "https://api.tokentrimmer.com".into());
    let client = Client::new(base, key);

    let outcome = client
        .chat()
        .model("claude-haiku-4-5")
        .message(user("Say hello in five words."))
        .tag("example=cost-attribution")
        .cost_limit(0.05)
        .send()
        .await?;

    println!("{}", outcome.text().unwrap_or(""));
    println!("cost     {:?}", outcome.cost.cost_usd);
    println!("saved    {:?}", outcome.cost.saved_usd);
    println!("savings% {:?}", outcome.savings_pct());
    Ok(())
}
