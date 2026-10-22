mod common;

use std::fs;

use common::TestEnv;
use pai::setup::{self, Target, Tool};
use serde_json::Value;

fn target(t: &TestEnv, context: u32) -> Target {
    Target {
        api_url: "https://api.test".into(),
        pai_bin: t.env.pai_bin.to_string_lossy().into_owned(),
        context,
        plan_known: true,
    }
}

fn backups(dir: &std::path::Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".pai-backup-"))
        .collect()
}

#[test]
fn print_mode_writes_nothing() {
    let t = TestEnv::new();
    for tool in [Tool::ClaudeCode, Tool::Codex, Tool::Opencode, Tool::Omp] {
        let out = setup::run(&t.env, tool, &target(&t, 262144), false).unwrap();
        assert!(out.contains("api.test"), "{out}");
    }
    assert!(!t.home().exists());
}

#[test]
fn claude_code_merges_and_backs_up() {
    let t = TestEnv::new();
    let dir = t.home().join(".claude");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("settings.json"),
        r#"{"theme":"dark","env":{"FOO":"1","ANTHROPIC_API_KEY":"sk-old"},"permissions":{"allow":["Bash(ls)"]}}"#,
    )
    .unwrap();

    let out = setup::run(&t.env, Tool::ClaudeCode, &target(&t, 65536), true).unwrap();
    assert!(
        out.contains("Backed up") && out.contains("Removed env.ANTHROPIC_API_KEY"),
        "{out}"
    );

    let v: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("settings.json")).unwrap()).unwrap();
    assert_eq!(v["apiKeyHelper"], "/opt/pai/bin/pai token");
    assert_eq!(v["theme"], "dark");
    assert_eq!(v["permissions"]["allow"][0], "Bash(ls)");
    assert_eq!(v["env"]["FOO"], "1");
    assert!(v["env"].get("ANTHROPIC_API_KEY").is_none());
    assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "https://api.test");
    assert_eq!(v["env"]["ANTHROPIC_MODEL"], "qwen3.8-27b");
    assert_eq!(v["env"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "65536");
    assert_eq!(v["env"]["CLAUDE_CODE_MAX_OUTPUT_TOKENS"], "16384");

    let backup = backups(&dir);
    assert_eq!(backup.len(), 1);
    assert!(
        fs::read_to_string(dir.join(&backup[0]))
            .unwrap()
            .contains("sk-old")
    );
}

#[test]
fn claude_code_refuses_to_rewrite_jsonc() {
    let t = TestEnv::new();
    let dir = t.home().join(".claude");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("settings.json"), "{ // mine\n}").unwrap();
    assert!(setup::run(&t.env, Tool::ClaudeCode, &target(&t, 65536), true).is_err());
    assert_eq!(
        fs::read_to_string(dir.join("settings.json")).unwrap(),
        "{ // mine\n}"
    );
}

#[test]
fn codex_adds_provider_and_profile() {
    let t = TestEnv::new();
    let dir = t.home().join(".codex");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("config.toml"),
        "# my settings\nmodel = \"gpt-5\"\n\n[model_providers.other]\nname = \"Other\"\n\n[profiles.privacy-ai]\nmodel = \"old\"\n",
    )
    .unwrap();

    setup::run(&t.env, Tool::Codex, &target(&t, 262144), true).unwrap();

    let text = fs::read_to_string(dir.join("config.toml")).unwrap();
    assert!(text.contains("# my settings"), "{text}");
    let doc: toml_edit::DocumentMut = text.parse().unwrap();
    assert_eq!(doc["model"].as_str(), Some("gpt-5"));
    assert_eq!(
        doc["model_providers"]["other"]["name"].as_str(),
        Some("Other")
    );
    let p = &doc["model_providers"]["privacy-ai"];
    assert_eq!(p["base_url"].as_str(), Some("https://api.test/v1"));
    assert_eq!(p["wire_api"].as_str(), Some("responses"));
    assert_eq!(p["auth"]["command"].as_str(), Some("/opt/pai/bin/pai"));
    assert_eq!(
        p["auth"]["args"]
            .as_array()
            .unwrap()
            .get(0)
            .and_then(|v| v.as_str()),
        Some("token")
    );
    assert!(p["auth"]["refresh_interval_ms"].as_integer().is_some());
    assert!(
        doc.get("profiles")
            .and_then(|p| p.get("privacy-ai"))
            .is_none()
    );

    let profile: toml_edit::DocumentMut = fs::read_to_string(dir.join("privacy-ai.config.toml"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(profile["model_provider"].as_str(), Some("privacy-ai"));
    assert_eq!(profile["model"].as_str(), Some("qwen3.8-27b"));
    assert_eq!(profile["model_context_window"].as_integer(), Some(262144));
    assert_eq!(backups(&dir).len(), 1);

    setup::run(&t.env, Tool::Codex, &target(&t, 262144), true).unwrap();
    let again = fs::read_to_string(dir.join("config.toml")).unwrap();
    assert_eq!(
        again.matches("[model_providers.privacy-ai]").count(),
        1,
        "{again}"
    );
}

#[test]
fn opencode_writes_plugin_and_model() {
    let t = TestEnv::new();
    let dir = t.home().join(".config").join("opencode");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("opencode.json"), r#"{"theme":"tokyonight"}"#).unwrap();

    setup::run(&t.env, Tool::Opencode, &target(&t, 65536), true).unwrap();

    let plugin = fs::read_to_string(dir.join("plugins").join("privacy-ai.ts")).unwrap();
    assert!(plugin.contains("const DEFAULT_PAI_BIN = \"/opt/pai/bin/pai\"\n"));
    assert!(plugin.contains("const DEFAULT_API_URL = \"https://api.test\"\n"));
    assert!(plugin.contains("const CONTEXT_WINDOW = 65536\n"));
    let v: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("opencode.json")).unwrap()).unwrap();
    assert_eq!(v["model"], "privacy-ai/qwen3.8-27b");
    assert_eq!(v["theme"], "tokyonight");
}

#[test]
fn omp_writes_extension() {
    let t = TestEnv::new();
    setup::run(&t.env, Tool::Omp, &target(&t, 262144), true).unwrap();
    let ext = fs::read_to_string(t.home().join(".omp/agent/extensions/privacy-ai.ts")).unwrap();
    assert!(ext.contains("const DEFAULT_PAI_BIN = \"/opt/pai/bin/pai\";\n"));
    assert!(ext.contains("const CONTEXT_WINDOW = 262144;\n"));
    assert!(ext.contains("registerProvider"));
}
