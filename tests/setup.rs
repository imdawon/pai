mod common;

use std::fs;

use common::TestEnv;
use pai::setup::{self, Target, Tool};
use serde_json::Value;

fn target(t: &TestEnv, context: u32) -> Target {
    Target {
        base_url: "http://127.0.0.1:47821".into(),
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
    for tool in [Tool::ClaudeCode, Tool::Codex, Tool::Opencode, Tool::Omp, Tool::Hermes] {
        let out = setup::run(&t.env, tool, &target(&t, 262144), false).unwrap();
        assert!(out.contains("127.0.0.1:47821"), "{out}");
    }
    assert!(!t.home().exists());
    assert!(t.env.keystore.get(pai::keystore::PROXY_KEY).unwrap().is_none(), "print mode makes no key");
    assert!(t.env.commands.recorded().is_empty());
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
    assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:47821");
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
    assert_eq!(p["base_url"].as_str(), Some("http://127.0.0.1:47821/v1"));
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
    assert!(plugin.contains("const DEFAULT_BASE_URL = \"http://127.0.0.1:47821\"\n"));
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

const HERMES_CONFIG: &str = "\
# Hermes config: my dev box
model:
  default: qwen3.8-27b # keep this
  provider: custom
  base_url: http://127.0.0.1:8080/v1
  api_key: dummy
  context_length: 262144

# Providers I run locally
providers:
  # llama.cpp on this box
  local-localhost:8080:
    api: http://127.0.0.1:8080/v1
    name: Local (localhost:8080)
    api_key: dummy
    default_model: glm5.3-flash
    models:
      glm5.3-flash:
        context_length: 262144 # long
      qwen3.8-27b:
        context_length: 262144
# fallbacks off for now
fallback_providers: []
";

fn hermes_path(t: &TestEnv) -> std::path::PathBuf {
    t.home().join(".hermes").join("config.yaml")
}

/// Our block, from its marker to its end marker, and the file with it cut out.
fn split_block(text: &str) -> (String, String) {
    let begin = text.find("# privacy-ai (managed by pai setup hermes)").expect("our block");
    let begin = text[..begin].rfind('\n').map_or(0, |i| i + 1);
    let end_marker = "# end privacy-ai\n";
    let end = text[begin..].find(end_marker).unwrap() + begin + end_marker.len();
    (text[begin..end].to_string(), format!("{}{}", &text[..begin], &text[end..]))
}

#[test]
fn hermes_adds_provider_and_keeps_everything_else() {
    let t = TestEnv::with_config();
    fs::create_dir_all(t.home().join(".hermes")).unwrap();
    fs::write(hermes_path(&t), HERMES_CONFIG).unwrap();

    let out = setup::run(&t.env, Tool::Hermes, &target(&t, 65536), true).unwrap();
    assert!(out.contains("Backed up") && out.contains("Added the privacy-ai provider"), "{out}");
    assert!(out.contains("/model custom:privacy-ai:qwen3.8-27b"), "{out}");
    assert_eq!(backups(&t.home().join(".hermes")).len(), 1);

    let text = fs::read_to_string(hermes_path(&t)).unwrap();
    let (block, rest) = split_block(&text);
    assert_eq!(rest, HERMES_CONFIG, "everything outside our block is untouched");
    assert!(text.contains("providers:\n  # privacy-ai (managed by pai setup hermes)\n  privacy-ai:\n"), "{text}");
    for line in [
        "    name: Privacy AI\n",
        "    api: \"http://127.0.0.1:47821/v1\"\n",
        "    default_model: qwen3.8-27b\n",
        "    models:\n      qwen3.8-27b:\n        context_length: 65536\n",
    ] {
        assert!(block.contains(line), "missing {line:?} in\n{block}");
    }

    assert!(block.contains("    key_cmd: \"/opt/pai/bin/pai token\"\n"), "{block}");
    assert!(block.contains("    api_mode: chat_completions\n"), "{block}");
    assert!(!text.contains("pai-local-"), "no key is written into the file");
    assert!(!text.contains("api_key: \"pai"), "{text}");
    assert!(t.env.commands.recorded().is_empty(), "Hermes starts pai itself, so no login service is installed");
}

#[test]
fn hermes_rerun_replaces_its_block() {
    let t = TestEnv::with_config();
    fs::create_dir_all(t.home().join(".hermes")).unwrap();
    fs::write(hermes_path(&t), HERMES_CONFIG).unwrap();

    setup::run(&t.env, Tool::Hermes, &target(&t, 65536), true).unwrap();
    let out = setup::run(&t.env, Tool::Hermes, &target(&t, 262144), true).unwrap();
    assert!(out.contains("Replaced the privacy-ai provider"), "{out}");
    let text = fs::read_to_string(hermes_path(&t)).unwrap();
    assert_eq!(text.matches("privacy-ai:").count(), 1, "{text}");
    assert_eq!(text.matches("# privacy-ai (managed by pai setup hermes)").count(), 1);
    assert!(text.contains("context_length: 262144\n  # end privacy-ai"), "{text}");
    assert_eq!(split_block(&text).1, HERMES_CONFIG);

    // An unmarked privacy-ai entry, as left by hand or a tool that dropped comments, is replaced too.
    let (block, _) = split_block(&text);
    let bare: String = block.lines().filter(|l| !l.trim_start().starts_with('#')).map(|l| format!("{l}\n")).collect();
    fs::write(hermes_path(&t), HERMES_CONFIG.replace("providers:\n", &format!("providers:\n{bare}"))).unwrap();
    setup::run(&t.env, Tool::Hermes, &target(&t, 262144), true).unwrap();
    let text = fs::read_to_string(hermes_path(&t)).unwrap();
    assert_eq!(text.matches("privacy-ai:").count(), 1, "{text}");
    assert_eq!(split_block(&text).1, HERMES_CONFIG);
}

#[test]
fn hermes_creates_providers_when_missing() {
    let t = TestEnv::with_config();
    let original = "# just a model\nmodel:\n  default: some-model\n  provider: auto";
    fs::create_dir_all(t.home().join(".hermes")).unwrap();
    fs::write(hermes_path(&t), original).unwrap();

    setup::run(&t.env, Tool::Hermes, &target(&t, 65536), true).unwrap();
    let text = fs::read_to_string(hermes_path(&t)).unwrap();
    assert!(text.starts_with(&format!("{original}\n\nproviders:\n  # privacy-ai (managed by pai setup hermes)\n")), "{text}");
    assert!(text.contains("  default: some-model\n"), "the default model is left alone");

    let empty = TestEnv::with_config();
    setup::run(&empty.env, Tool::Hermes, &target(&empty, 65536), true).unwrap();
    let text = fs::read_to_string(hermes_path(&empty)).unwrap();
    assert!(text.starts_with("providers:\n  # privacy-ai"), "{text}");
}

#[test]
fn hermes_handles_four_space_indent_crlf_and_empty_flow() {
    let t = TestEnv::with_config();
    fs::create_dir_all(t.home().join(".hermes")).unwrap();
    fs::write(hermes_path(&t), "providers:\r\n    other:\r\n        api: http://x/v1\r\n").unwrap();
    setup::run(&t.env, Tool::Hermes, &target(&t, 65536), true).unwrap();
    let text = fs::read_to_string(hermes_path(&t)).unwrap();
    assert!(text.contains("providers:\r\n    # privacy-ai (managed by pai setup hermes)\r\n    privacy-ai:\r\n        name: Privacy AI\r\n"), "{text:?}");
    assert!(text.ends_with("    # end privacy-ai\r\n    other:\r\n        api: http://x/v1\r\n"), "{text:?}");

    fs::write(hermes_path(&t), "providers: {} # none yet\n").unwrap();
    setup::run(&t.env, Tool::Hermes, &target(&t, 65536), true).unwrap();
    let text = fs::read_to_string(hermes_path(&t)).unwrap();
    assert!(text.starts_with("providers: # none yet\n  # privacy-ai"), "{text}");

    fs::write(hermes_path(&t), "providers: {a: {api: x}}\n").unwrap();
    assert!(setup::run(&t.env, Tool::Hermes, &target(&t, 65536), true).is_err());
    assert_eq!(fs::read_to_string(hermes_path(&t)).unwrap(), "providers: {a: {api: x}}\n");
}

#[test]
fn hermes_follows_hermes_home() {
    let mut t = TestEnv::with_config();
    let dir = t.dir.path().join("hermes-home");
    t.env.vars.insert("HERMES_HOME".into(), dir.to_string_lossy().into_owned());
    let out = setup::run(&t.env, Tool::Hermes, &target(&t, 65536), false).unwrap();
    assert!(out.contains(&dir.join("config.yaml").display().to_string()), "{out}");
    assert!(out.contains("key_cmd: \"/opt/pai/bin/pai token\""), "{out}");
    setup::run(&t.env, Tool::Hermes, &target(&t, 65536), true).unwrap();
    assert!(dir.join("config.yaml").exists());
    assert!(!t.home().join(".hermes").exists());
}
