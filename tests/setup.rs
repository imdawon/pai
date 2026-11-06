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
        "# my settings\nmodel = \"gpt-5\"\n\n[model_providers.other]\nname = \"Other\"\n\n[profiles.dawonai]\nmodel = \"old\"\n",
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
    let p = &doc["model_providers"]["dawonai"];
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
            .and_then(|p| p.get("dawonai"))
            .is_none()
    );

    let profile: toml_edit::DocumentMut = fs::read_to_string(dir.join("dawonai.config.toml"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(profile["model_provider"].as_str(), Some("dawonai"));
    assert_eq!(profile["model"].as_str(), Some("qwen3.8-27b"));
    assert_eq!(profile["model_context_window"].as_integer(), Some(262144));
    assert_eq!(backups(&dir).len(), 1);

    setup::run(&t.env, Tool::Codex, &target(&t, 262144), true).unwrap();
    let again = fs::read_to_string(dir.join("config.toml")).unwrap();
    assert_eq!(
        again.matches("[model_providers.dawonai]").count(),
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

    let plugin = fs::read_to_string(dir.join("plugins").join("dawonai.ts")).unwrap();
    assert!(plugin.contains("const DEFAULT_PAI_BIN = \"/opt/pai/bin/pai\"\n"));
    assert!(plugin.contains("const DEFAULT_BASE_URL = \"http://127.0.0.1:47821\"\n"));
    assert!(plugin.contains("const CONTEXT_WINDOW = 65536\n"));
    let v: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("opencode.json")).unwrap()).unwrap();
    assert_eq!(v["model"], "dawonai/qwen3.8-27b");
    assert_eq!(v["theme"], "tokyonight");
}

#[test]
fn opencode_sets_the_model_in_jsonc_and_keeps_comments() {
    let t = TestEnv::new();
    let dir = t.home().join(".config").join("opencode");
    fs::create_dir_all(&dir).unwrap();
    let before = "{\n  // my theme\n  \"theme\": \"tokyonight\",\n}\n";
    fs::write(dir.join("opencode.jsonc"), before).unwrap();

    let log = setup::run(&t.env, Tool::Opencode, &target(&t, 65536), true).unwrap();

    let after = fs::read_to_string(dir.join("opencode.jsonc")).unwrap();
    assert_eq!(after, "{\n  \"model\": \"dawonai/qwen3.8-27b\",\n  // my theme\n  \"theme\": \"tokyonight\",\n}\n");
    assert!(!dir.join("opencode.json").exists(), "no second config file");
    assert!(!log.contains("Left"), "{log}");
    assert_eq!(backups(&dir).len(), 1);
    assert_eq!(setup::set_up_tools(&t.env), vec![Tool::Opencode]);

    // Running it again changes nothing, and undo puts the file back.
    setup::run(&t.env, Tool::Opencode, &target(&t, 65536), true).unwrap();
    assert_eq!(backups(&dir).len(), 1);
    setup::undo(&t.env, Tool::Opencode).unwrap();
    assert_eq!(fs::read_to_string(dir.join("opencode.jsonc")).unwrap(), before);
}

#[test]
fn omp_writes_extension() {
    let t = TestEnv::new();
    setup::run(&t.env, Tool::Omp, &target(&t, 262144), true).unwrap();
    let ext = fs::read_to_string(t.home().join(".omp/agent/extensions/dawonai.ts")).unwrap();
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
    let begin = text.find("# dawonai (managed by pai setup hermes)").expect("our block");
    let begin = text[..begin].rfind('\n').map_or(0, |i| i + 1);
    let end_marker = "# end dawonai\n";
    let end = text[begin..].find(end_marker).unwrap() + begin + end_marker.len();
    (text[begin..end].to_string(), format!("{}{}", &text[..begin], &text[end..]))
}

#[test]
fn hermes_adds_provider_and_keeps_everything_else() {
    let t = TestEnv::with_config();
    fs::create_dir_all(t.home().join(".hermes")).unwrap();
    fs::write(hermes_path(&t), HERMES_CONFIG).unwrap();

    let out = setup::run(&t.env, Tool::Hermes, &target(&t, 65536), true).unwrap();
    assert!(out.contains("Backed up") && out.contains("Added the dawonai provider"), "{out}");
    assert!(out.contains("/model custom:dawonai:qwen3.8-27b"), "{out}");
    assert_eq!(backups(&t.home().join(".hermes")).len(), 1);

    let text = fs::read_to_string(hermes_path(&t)).unwrap();
    let (block, rest) = split_block(&text);
    assert_eq!(rest, HERMES_CONFIG, "everything outside our block is untouched");
    assert!(text.contains("providers:\n  # dawonai (managed by pai setup hermes)\n  dawonai:\n"), "{text}");
    for line in [
        "    name: DawonAI\n",
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
    assert!(out.contains("Replaced the dawonai provider"), "{out}");
    let text = fs::read_to_string(hermes_path(&t)).unwrap();
    assert_eq!(text.matches("dawonai:").count(), 1, "{text}");
    assert_eq!(text.matches("# dawonai (managed by pai setup hermes)").count(), 1);
    assert!(text.contains("context_length: 262144\n  # end dawonai"), "{text}");
    assert_eq!(split_block(&text).1, HERMES_CONFIG);

    // An unmarked dawonai entry, as left by hand or a tool that dropped comments, is replaced too.
    let (block, _) = split_block(&text);
    let bare: String = block.lines().filter(|l| !l.trim_start().starts_with('#')).map(|l| format!("{l}\n")).collect();
    fs::write(hermes_path(&t), HERMES_CONFIG.replace("providers:\n", &format!("providers:\n{bare}"))).unwrap();
    setup::run(&t.env, Tool::Hermes, &target(&t, 262144), true).unwrap();
    let text = fs::read_to_string(hermes_path(&t)).unwrap();
    assert_eq!(text.matches("dawonai:").count(), 1, "{text}");
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
    assert!(text.starts_with(&format!("{original}\n\nproviders:\n  # dawonai (managed by pai setup hermes)\n")), "{text}");
    assert!(text.contains("  default: some-model\n"), "the default model is left alone");

    let empty = TestEnv::with_config();
    setup::run(&empty.env, Tool::Hermes, &target(&empty, 65536), true).unwrap();
    let text = fs::read_to_string(hermes_path(&empty)).unwrap();
    assert!(text.starts_with("providers:\n  # dawonai"), "{text}");
}

#[test]
fn hermes_handles_four_space_indent_crlf_and_empty_flow() {
    let t = TestEnv::with_config();
    fs::create_dir_all(t.home().join(".hermes")).unwrap();
    fs::write(hermes_path(&t), "providers:\r\n    other:\r\n        api: http://x/v1\r\n").unwrap();
    setup::run(&t.env, Tool::Hermes, &target(&t, 65536), true).unwrap();
    let text = fs::read_to_string(hermes_path(&t)).unwrap();
    assert!(text.contains("providers:\r\n    # dawonai (managed by pai setup hermes)\r\n    dawonai:\r\n        name: DawonAI\r\n"), "{text:?}");
    assert!(text.ends_with("    # end dawonai\r\n    other:\r\n        api: http://x/v1\r\n"), "{text:?}");

    fs::write(hermes_path(&t), "providers: {} # none yet\n").unwrap();
    setup::run(&t.env, Tool::Hermes, &target(&t, 65536), true).unwrap();
    let text = fs::read_to_string(hermes_path(&t)).unwrap();
    assert!(text.starts_with("providers: # none yet\n  # dawonai"), "{text}");

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

const SKILL: &str = include_str!("../skill/dawonai/SKILL.md");

/// The skill's file under the test home, for each harness with default settings.
fn skill_file(t: &TestEnv, tool: Tool) -> std::path::PathBuf {
    let root = match tool {
        Tool::ClaudeCode => t.home().join(".claude/skills"),
        Tool::Codex => t.home().join(".agents/skills"),
        Tool::Opencode => t.home().join(".config/opencode/skills"),
        Tool::Omp => t.home().join(".omp/agent/skills"),
        Tool::Hermes => t.home().join(".hermes/skills"),
        Tool::Other => unreachable!("pai writes no skill for other harnesses"),
    };
    root.join("dawonai").join("SKILL.md")
}

const TOOLS: [Tool; 5] = [Tool::ClaudeCode, Tool::Codex, Tool::Opencode, Tool::Omp, Tool::Hermes];

#[test]
fn skill_is_named_dawonai() {
    assert!(SKILL.starts_with("---\nname: dawonai\ndescription: "), "{SKILL}");
    let front = SKILL.split("\n---\n").next().unwrap();
    let description = front.lines().find_map(|l| l.strip_prefix("description: ")).unwrap();
    assert!(description.len() <= 1024, "the Agent Skills spec caps descriptions at 1024 characters");
    assert!(!SKILL.contains("Privacy AI") && !SKILL.contains("privacy-ai"));
    assert!(!SKILL.to_lowercase().contains("test mode") && !SKILL.contains("--test-"));
}

#[test]
fn print_mode_says_where_the_skill_goes() {
    let t = TestEnv::new();
    for tool in TOOLS {
        let out = setup::run(&t.env, tool, &target(&t, 65536), false).unwrap();
        assert!(out.contains(&skill_file(&t, tool).display().to_string()), "{tool:?}: {out}");
    }
    assert!(!t.home().exists());
}

#[test]
fn apply_installs_the_skill_for_each_harness() {
    for tool in TOOLS {
        let t = TestEnv::with_config();
        let out = setup::run(&t.env, tool, &target(&t, 65536), true).unwrap();
        let path = skill_file(&t, tool);
        assert_eq!(setup::skill_path(&t.env, tool), path);
        assert_eq!(fs::read_to_string(&path).unwrap(), SKILL, "{tool:?}");
        assert!(out.contains(&format!("Wrote {}", path.display())), "{tool:?}: {out}");
        assert!(out.contains("Installed the DawonAI skill"), "{tool:?}: {out}");
        assert!(backups(path.parent().unwrap()).is_empty(), "nothing to back up the first time");
    }
}

#[test]
fn rerun_backs_up_and_replaces_the_skill() {
    for tool in TOOLS {
        let mut t = TestEnv::with_config();
        t.env.clock = pai::app::Clock::Fixed(1_800_000_000);
        let path = skill_file(&t, tool);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "---\nname: dawonai\ndescription: an old copy\n---\n").unwrap();

        let out = setup::run(&t.env, tool, &target(&t, 65536), true).unwrap();
        assert!(out.contains(&format!("Backed up {}", path.display())), "{tool:?}: {out}");
        assert_eq!(fs::read_to_string(&path).unwrap(), SKILL);
        let dir = path.parent().unwrap();
        assert_eq!(backups(dir), vec!["SKILL.md.pai-backup-1800000000".to_string()]);
        assert!(fs::read_to_string(dir.join("SKILL.md.pai-backup-1800000000")).unwrap().contains("an old copy"));

        t.env.clock = pai::app::Clock::Fixed(1_800_000_060);
        setup::run(&t.env, tool, &target(&t, 65536), true).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), SKILL);
        assert_eq!(backups(dir).len(), 1, "an unchanged file isn't backed up again: {tool:?}");
        let names: Vec<String> = fs::read_dir(dir.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["dawonai".to_string()], "only our skill's directory");
    }
}

#[test]
fn skill_follows_each_harness_config_dir() {
    let mut t = TestEnv::with_config();
    let base = t.dir.path().join("elsewhere");
    for (var, dir) in [
        ("CLAUDE_CONFIG_DIR", "claude"),
        ("XDG_CONFIG_HOME", "xdg"),
        ("PI_CODING_AGENT_DIR", "omp-agent"),
        ("HERMES_HOME", "hermes"),
        ("CODEX_HOME", "codex"),
    ] {
        t.env.vars.insert(var.into(), base.join(dir).to_string_lossy().into_owned());
    }
    let expected = [
        (Tool::ClaudeCode, base.join("claude/skills")),
        (Tool::Opencode, base.join("xdg/opencode/skills")),
        (Tool::Omp, base.join("omp-agent/skills")),
        (Tool::Hermes, base.join("hermes/skills")),
        // Codex reads user skills from ~/.agents/skills whatever CODEX_HOME says.
        (Tool::Codex, t.home().join(".agents/skills")),
    ];
    fs::create_dir_all(base.join("codex")).unwrap();
    for (tool, root) in expected {
        setup::run(&t.env, tool, &target(&t, 65536), true).unwrap();
        let path = root.join("dawonai").join("SKILL.md");
        assert_eq!(fs::read_to_string(&path).unwrap(), SKILL, "{tool:?}");
    }
    assert!(base.join("omp-agent/extensions/dawonai.ts").exists());
    assert!(!t.home().join(".claude").exists() && !t.home().join(".omp").exists());
}

#[test]
fn refresh_rewrites_only_what_a_new_version_changes() {
    let t = TestEnv::new();
    setup::run(&t.env, Tool::ClaudeCode, &target(&t, 65536), true).unwrap();
    setup::run(&t.env, Tool::Codex, &target(&t, 65536), true).unwrap();
    let claude = t.home().join(".claude");
    assert!(backups(&claude).is_empty());

    // The same settings again: nothing written, nothing backed up.
    let again = setup::run(&t.env, Tool::ClaudeCode, &target(&t, 65536), true).unwrap();
    assert!(!again.contains("Wrote"), "{again}");
    assert!(backups(&claude).is_empty());

    // An unknown plan never rewrites anything, so an Agent plan's context can't shrink.
    assert!(setup::refresh(&t.env, None).is_empty());

    let done = setup::refresh(&t.env, Some(pai_proto::Plan::Agent));
    let changed: Vec<Tool> = done.iter().filter(|(_, r)| matches!(r, Ok(true))).map(|(t, _)| *t).collect();
    assert_eq!(changed, vec![Tool::ClaudeCode, Tool::Codex], "only the tools pai set up");
    let v: Value = serde_json::from_str(&fs::read_to_string(claude.join("settings.json")).unwrap()).unwrap();
    assert_eq!(v["env"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "131072");
    assert_eq!(backups(&claude).len(), 1);
    assert!(!t.home().join(".config/opencode").exists(), "a tool pai never set up stays untouched");
}

#[test]
fn the_limit_the_model_api_named_wins_over_the_built_in_one() {
    let t = TestEnv::new();
    assert_eq!(setup::context_for(&t.env, Some(pai_proto::Plan::Agent)), (131072, true));
    assert_eq!(setup::context_for(&t.env, None), (131072, false));
    setup::run(&t.env, Tool::Omp, &target(&t, 131072), true).unwrap();

    setup::save_learned_context(&t.env, 65536).unwrap();
    assert_eq!(setup::learned_context(&t.env), Some(65536));
    assert_eq!(setup::context_for(&t.env, Some(pai_proto::Plan::Agent)), (65536, true));
    // With a named limit, even an unknown plan rewrites the settings pai wrote.
    let done = setup::refresh(&t.env, None);
    assert!(matches!(done.as_slice(), [(Tool::Omp, Ok(true))]), "{done:?}");
    let ext = fs::read_to_string(t.home().join(".omp/agent/extensions/dawonai.ts")).unwrap();
    assert!(ext.contains("const CONTEXT_WINDOW = 65536;\n"), "{ext}");

    // Values outside 8K to 1M are never taken.
    for bad in ["100", "8191", "1048577", "-5", "lots", ""] {
        assert_eq!(setup::parse_context(bad), None, "{bad}");
    }
    assert_eq!(setup::parse_context(" 8192 "), Some(8192));
    assert_eq!(setup::parse_context("1048576"), Some(1048576));
}

#[test]
fn other_prints_every_setting_and_writes_nothing() {
    let t = TestEnv::new();
    for apply in [false, true] {
        let out = setup::run(&t.env, Tool::Other, &target(&t, 131072), apply).unwrap();
        for want in ["http://127.0.0.1:47821/v1", "qwen3.8-27b", "131072 tokens", "32000 tokens", "/opt/pai/bin/pai token"] {
            assert!(out.contains(want), "{want} in {out}");
        }
        assert_eq!(out.contains("nothing was changed"), apply, "{out}");
        assert!(!out.contains("--undo"), "{out}");
    }
    assert!(!t.home().exists(), "writes nothing");
    assert!(setup::undo(&t.env, Tool::Other).unwrap().contains("Nothing to undo"));
}

#[test]
fn other_config_files_carry_this_machines_values() {
    let t = TestEnv::new();
    let tg = target(&t, 131072);
    let models: Value = serde_json::from_str(&setup::other_config(&tg, setup::ConfigFormat::ModelsJson)).unwrap();
    let p = &models["providers"]["dawonai"];
    assert_eq!(p["baseUrl"], "http://127.0.0.1:47821/v1");
    assert_eq!(p["api"], "openai-completions");
    assert_eq!(p["apiKey"], "!/opt/pai/bin/pai token");
    let m = &p["models"][0];
    assert_eq!(m["id"], "qwen3.8-27b");
    assert_eq!(m["reasoning"], true);
    assert_eq!(m["input"], serde_json::json!(["text", "image"]));
    assert_eq!(m["contextWindow"], 131072);
    assert_eq!(m["maxTokens"], 32000);

    let openai: Value = serde_json::from_str(&setup::other_config(&tg, setup::ConfigFormat::Openai)).unwrap();
    assert_eq!(openai["base_url"], "http://127.0.0.1:47821/v1");
    assert_eq!(openai["models"][0]["max_output_tokens"], 32000);
    let anthropic: Value = serde_json::from_str(&setup::other_config(&tg, setup::ConfigFormat::Anthropic)).unwrap();
    assert_eq!(anthropic["base_url"], "http://127.0.0.1:47821");
    assert_eq!(anthropic["api_key_header"], "x-api-key");
    let claude: Value = serde_json::from_str(&setup::other_config(&tg, setup::ConfigFormat::ClaudeCode)).unwrap();
    assert_eq!(claude["apiKeyHelper"], "/opt/pai/bin/pai token");

    // A smaller context shrinks the output limit with it.
    let small: Value = serde_json::from_str(&setup::other_config(&target(&t, 65536), setup::ConfigFormat::ModelsJson)).unwrap();
    assert_eq!(small["providers"]["dawonai"]["models"][0]["maxTokens"], 16384);
}

#[test]
fn other_out_writes_the_file_and_backs_up_what_was_there() {
    let t = TestEnv::new();
    let tg = target(&t, 131072);
    let dir = t.home().join("harness");
    let path = dir.join("models.json");
    let out = setup::other_write(&t.env, &tg, setup::ConfigFormat::ModelsJson, &path).unwrap();
    assert!(out.contains("Wrote"), "{out}");
    assert_eq!(fs::read_to_string(&path).unwrap(), setup::other_config(&tg, setup::ConfigFormat::ModelsJson));
    assert!(backups(&dir).is_empty());

    let again = setup::other_write(&t.env, &tg, setup::ConfigFormat::ModelsJson, &path).unwrap();
    assert!(again.contains("already has these settings"), "{again}");
    assert!(backups(&dir).is_empty());

    fs::write(&path, "{\"mine\":true}").unwrap();
    let out = setup::other_write(&t.env, &tg, setup::ConfigFormat::Openai, &path).unwrap();
    assert!(out.contains("Backed up"), "{out}");
    assert_eq!(backups(&dir).len(), 1);
}
