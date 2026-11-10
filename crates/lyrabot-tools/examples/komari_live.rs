//! 手动验证：cargo run -p lyrabot-tools --example komari_live -- <komari.toml>
use lyrabot_agent::{Tool, ToolCtx};
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let path = std::env::args().nth(1).expect("需要 komari.toml 路径");
    let t = lyrabot_tools::KomariReport::new(path.into());
    let ctx = ToolCtx { session: 0, call_id: "x".into(), cancel: Default::default() };
    let out = t.call(ctx, serde_json::json!({})).await;
    println!("is_error={}\n{}", out.is_error, out.text);
}
