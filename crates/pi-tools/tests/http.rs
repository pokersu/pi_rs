//! HTTP 工具工厂的测试：配置解析、URL 填充、目录加载。

use pi_tools::http::{
    AuthConfig, HttpToolConfig, extract_placeholders, fill_url, load_tools_from_dir,
};

fn sample_config_json() -> String {
    r#"{
        "name": "get_user",
        "description": "Get a user by id",
        "parameters": {
            "type": "object",
            "properties": { "id": { "type": "integer", "description": "User id" } },
            "required": ["id"]
        },
        "request": {
            "method": "GET",
            "url": "https://api.example.com/users/{id}"
        },
        "auth": { "type": "bearer", "token_env": "EXAMPLE_TOKEN" }
    }"#
    .to_string()
}

#[test]
fn parses_http_tool_config() {
    let config: HttpToolConfig = serde_json::from_str(&sample_config_json()).unwrap();
    assert_eq!(config.name, "get_user");
    assert_eq!(config.request.method, "GET");
    assert_eq!(config.request.url, "https://api.example.com/users/{id}");
    assert!(matches!(config.auth, Some(AuthConfig::Bearer { .. })));
}

#[test]
fn extracts_and_fills_url_placeholders() {
    let url = "https://api.example.com/repos/{owner}/{repo}/issues";
    assert_eq!(
        extract_placeholders(url),
        vec!["owner".to_string(), "repo".to_string()]
    );

    let params = serde_json::json!({ "owner": "a b", "repo": "r/1" })
        .as_object()
        .unwrap()
        .clone();
    let filled = fill_url(url, &params).unwrap();
    assert_eq!(filled, "https://api.example.com/repos/a%20b/r%2F1/issues");
}

#[test]
fn missing_path_param_is_an_error() {
    let params = serde_json::Map::new();
    let err = fill_url("https://x/{id}", &params).unwrap_err();
    assert!(err.contains("id"));
}

#[test]
fn loads_tools_from_dir() {
    let dir = std::env::temp_dir().join(format!("pi-tools-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("get_user.json"), sample_config_json()).unwrap();
    std::fs::write(dir.join("ignore.txt"), "not json").unwrap();

    let tools = load_tools_from_dir(&dir).unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "get_user");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn duplicate_names_are_rejected() {
    let dir = std::env::temp_dir().join(format!("pi-tools-dup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.json"), sample_config_json()).unwrap();
    std::fs::write(dir.join("b.json"), sample_config_json()).unwrap();

    let err = load_tools_from_dir(&dir).unwrap_err();
    assert!(err.to_string().contains("重复的工具名"));

    std::fs::remove_dir_all(&dir).ok();
}
