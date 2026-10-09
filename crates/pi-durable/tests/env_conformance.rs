//! `testing::env_conformance` 对 `NodeExecutionEnv` 的参数化运行。

use pi_durable::chord::context::{BACKGROUND_CONTEXT, Context};
use pi_durable::env::NodeExecutionEnv;
use pi_durable::testing::env_conformance_cases;

fn context() -> &'static dyn Context {
    BACKGROUND_CONTEXT.as_ref()
}

#[tokio::test]
async fn node_env_conformance() {
    for (name, run) in env_conformance_cases() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = NodeExecutionEnv::new(dir.path().to_string_lossy().into_owned());
        println!("  [node] {name}");
        run(&env, context()).await;
    }
}
