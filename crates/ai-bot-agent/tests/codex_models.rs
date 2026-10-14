use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;

use ai_bot_agent::AgentBackend;
use ai_bot_agent::codex::{CodexBackend, CodexConfig};
use serde_json::json;

#[tokio::test]
async fn model_queries_refresh_the_cli_catalog_and_keep_five_visible_choices() {
    let root = tempfile::tempdir().expect("目录");
    let program = root.path().join("codex");
    let catalog = root.path().join("catalog.json");
    let capture = root.path().join("args");
    std::fs::write(
        &program,
        r#"#!/bin/sh
printf '%s\n' "$@" > "$CAPTURE"
/bin/cat "$CATALOG"
"#,
    )
    .expect("CLI");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).expect("权限");
    let backend = CodexBackend::new(CodexConfig {
        program,
        profile: Some("ai-bot".into()),
        env: vec![
            ("CATALOG".into(), catalog.as_os_str().into()),
            ("CAPTURE".into(), capture.as_os_str().into()),
        ],
        stop_grace: Duration::from_secs(1),
    });
    let entries: Vec<_> = (1..=8)
        .map(|n| {
            json!({
                "slug": format!("model-{n}"), "display_name": format!("Model {n}"),
                "visibility": if n == 2 { "hide" } else { "list" },
                "base_instructions": "不属于选择卡的内容",
            })
        })
        .collect();
    std::fs::write(&catalog, json!({"models": entries}).to_string()).expect("目录");
    let models = backend.models().await.expect("查询");
    assert_eq!(
        models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["model-1", "model-3", "model-4", "model-5", "model-6"]
    );
    assert_eq!(models[0].name, "Model 1");
    assert_eq!(
        std::fs::read_to_string(&capture).expect("参数"),
        "debug\nmodels\n"
    );
    std::fs::write(
        &catalog,
        json!({"models": [{
            "slug": "new-model", "display_name": "New model", "visibility": "list",
        }]})
        .to_string(),
    )
    .expect("刷新目录");
    assert_eq!(backend.models().await.expect("重新查询")[0].id, "new-model");
    for raw in [
        "not JSON",
        "{\"models\":[]}",
        "{\"models\":[{\"slug\":\"hidden\",\"display_name\":\"Hidden\",\"visibility\":\"hide\"}]}",
    ] {
        std::fs::write(&catalog, raw).expect("错误目录");
        assert!(
            backend.models().await.is_err(),
            "失败时不能继续使用上次的选择"
        );
    }
}
