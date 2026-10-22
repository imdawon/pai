use std::fs;
use std::path::{Path, PathBuf};

use pai_proto::Plan;
use serde_json::{Map, Value, json};

use crate::app::Env;
use crate::error::{Error, Result, io_err};

pub const MODEL: &str = "qwen3.8-27b";
pub const PROVIDER_ID: &str = "privacy-ai";
const OPENCODE_PLUGIN: &str = include_str!("../integrations/opencode/src/index.ts");
const OMP_EXTENSION: &str = include_str!("../integrations/omp/privacy-ai.ts");

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Tool {
    ClaudeCode,
    Codex,
    Opencode,
    Omp,
}

/// What every tool's config is built from.
pub struct Target {
    /// pai's local proxy, which every tool talks to.
    pub base_url: String,
    pub pai_bin: String,
    pub context: u32,
    pub plan_known: bool,
}

impl Target {
    pub fn resolve(env: &Env, plan: Option<Plan>) -> Target {
        let base_url = crate::proxy::url(env);
        let context = match plan {
            Some(p) => p.max_context() as u32,
            None => Plan::Chat.max_context() as u32,
        };
        Target {
            base_url,
            pai_bin: env.pai_bin.to_string_lossy().into_owned(),
            context,
            plan_known: plan.is_some(),
        }
    }

    fn max_output(&self) -> u32 {
        (self.context / 4).min(32_000)
    }
}

pub fn run(env: &Env, tool: Tool, target: &Target, apply: bool) -> Result<String> {
    let mut out = String::new();
    if !target.plan_known {
        out.push_str(&format!(
            "Note: couldn't read your plan, so the context window is set to {} tokens. \
             On the Agent plan, run this again after `pai pair` to get 262144.\n\n",
            target.context
        ));
    }
    let body = match (tool, apply) {
        (Tool::ClaudeCode, false) => claude_print(env, target),
        (Tool::ClaudeCode, true) => claude_apply(env, target)?,
        (Tool::Codex, false) => codex_print(env, target),
        (Tool::Codex, true) => codex_apply(env, target)?,
        (Tool::Opencode, false) => opencode_print(env, target),
        (Tool::Opencode, true) => opencode_apply(env, target)?,
        (Tool::Omp, false) => omp_print(env, target),
        (Tool::Omp, true) => omp_apply(env, target)?,
    };
    out.push_str(&body);
    Ok(out)
}

fn backup(path: &Path, now: i64) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dest = path.with_file_name(format!("{name}.pai-backup-{now}"));
    fs::copy(path, &dest).map_err(|e| io_err("back up", path, e))?;
    Ok(Some(dest))
}

fn write_file(path: &Path, content: &str, now: i64, log: &mut String) -> Result<()> {
    if let Some(b) = backup(path, now)? {
        log.push_str(&format!(
            "Backed up {} to {}\n",
            path.display(),
            b.display()
        ));
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| io_err("create", dir, e))?;
    }
    fs::write(path, content).map_err(|e| io_err("write", path, e))?;
    log.push_str(&format!("Wrote {}\n", path.display()));
    Ok(())
}

fn read_json_object(path: &Path) -> Result<Map<String, Value>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(io_err("read", path, e)),
    };
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(m)) => Ok(m),
        _ => Err(Error::Setup(format!(
            "{} isn't a plain JSON object (it may have comments), so pai won't rewrite it. \
             Run this command without --apply and add the settings by hand",
            path.display()
        ))),
    }
}

fn pretty(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).expect("json serializes");
    s.push('\n');
    s
}

/// Quotes a path for tools that run their helper through a shell.
fn shell_word(s: &str) -> String {
    let plain = s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_-./:\\@%+=,".contains(c));
    if plain {
        s.to_string()
    } else if cfg!(windows) {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// TOML basic strings use JSON's escapes.
fn toml_str(s: &str) -> String {
    serde_json::to_string(s).expect("string serializes")
}

fn js_rewrite(source: &str, target: &Target) -> String {
    let base = serde_json::to_string(&target.base_url).expect("string serializes");
    let bin = serde_json::to_string(&target.pai_bin).expect("string serializes");
    source
        .lines()
        .map(|line| {
            let semi = if line.trim_end().ends_with(';') {
                ";"
            } else {
                ""
            };
            if line.starts_with("const DEFAULT_BASE_URL = ") {
                format!("const DEFAULT_BASE_URL = {base}{semi}")
            } else if line.starts_with("const DEFAULT_PAI_BIN = ") {
                format!("const DEFAULT_PAI_BIN = {bin}{semi}")
            } else if line.starts_with("const CONTEXT_WINDOW = ") {
                format!("const CONTEXT_WINDOW = {}{semi}", target.context)
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn tool_dir(env: &Env, var: &str, default: &[&str]) -> PathBuf {
    match env.var(var) {
        Some(d) => PathBuf::from(d),
        None => default
            .iter()
            .fold(env.paths.home.clone(), |p, part| p.join(part)),
    }
}

// Claude Code: https://code.claude.com/docs/en/settings, /env-vars, /llm-gateway, /model-config

fn claude_settings_path(env: &Env) -> PathBuf {
    tool_dir(env, "CLAUDE_CONFIG_DIR", &[".claude"]).join("settings.json")
}

fn claude_settings(t: &Target) -> Value {
    json!({
        "apiKeyHelper": format!("{} token", shell_word(&t.pai_bin)),
        "skipWebFetchPreflight": true,
        "env": {
            "ANTHROPIC_BASE_URL": t.base_url,
            "ANTHROPIC_MODEL": MODEL,
            "ANTHROPIC_DEFAULT_FABLE_MODEL": MODEL,
            "ANTHROPIC_DEFAULT_OPUS_MODEL": MODEL,
            "ANTHROPIC_DEFAULT_SONNET_MODEL": MODEL,
            "ANTHROPIC_DEFAULT_HAIKU_MODEL": MODEL,
            "CLAUDE_CODE_SUBAGENT_MODEL": MODEL,
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS": t.context.to_string(),
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS": t.max_output().to_string(),
            "CLAUDE_CODE_API_KEY_HELPER_TTL_MS": "3600000",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
            "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS": "1"
        }
    })
}

/// These beat apiKeyHelper, so a leftover one silently sends the wrong key.
const CLAUDE_OVERRIDING_VARS: [&str; 2] = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];

fn claude_warnings(env: &Env, t: &Target) -> String {
    let mut out = String::new();
    let set: Vec<&str> = CLAUDE_OVERRIDING_VARS
        .iter()
        .copied()
        .filter(|v| env.var(v).is_some())
        .collect();
    if !set.is_empty() {
        out.push_str(&format!(
            "\nWarning: {} is set in your shell and takes precedence over apiKeyHelper. Unset it before starting Claude Code.\n",
            set.join(" and ")
        ));
    }
    // Claude Code ignores env.ANTHROPIC_BASE_URL from settings when its launch environment sets one.
    if let Some(url) = env
        .var("ANTHROPIC_BASE_URL")
        .filter(|u| crate::config::trim_url(u) != t.base_url)
    {
        out.push_str(&format!(
            "\nWarning: ANTHROPIC_BASE_URL={url} is set in the environment Claude Code starts in, and it wins over \
             settings.json. Unset it, or start Claude Code from a shell without it.\n"
        ));
    }
    out
}

fn claude_print(env: &Env, t: &Target) -> String {
    format!(
        "Merge into {}:\n\n{}\nRun `pai setup claude-code --apply` to write it. Claude Code runs `pai token` \
         at startup, every hour, and after a 401; it starts pai's local proxy and prints the key for it.\n{}",
        claude_settings_path(env).display(),
        pretty(&claude_settings(t)),
        claude_warnings(env, t)
    )
}

fn claude_apply(env: &Env, t: &Target) -> Result<String> {
    let path = claude_settings_path(env);
    let mut settings = read_json_object(&path)?;
    let mut log = String::new();
    let Value::Object(ours) = claude_settings(t) else {
        unreachable!()
    };
    for (k, v) in ours {
        if k == "env" {
            let env_obj = settings.entry("env").or_insert_with(|| json!({}));
            if !env_obj.is_object() {
                *env_obj = json!({});
            }
            let env_map = env_obj.as_object_mut().expect("object");
            for var in CLAUDE_OVERRIDING_VARS {
                if env_map.remove(var).is_some() {
                    log.push_str(&format!(
                        "Removed env.{var}, which would override apiKeyHelper\n"
                    ));
                }
            }
            if let Value::Object(vars) = v {
                env_map.extend(vars);
            }
        } else {
            settings.insert(k, v);
        }
    }
    if !settings.contains_key("$schema") {
        let mut with_schema = Map::new();
        with_schema.insert(
            "$schema".into(),
            json!("https://json.schemastore.org/claude-code-settings.json"),
        );
        with_schema.extend(settings);
        settings = with_schema;
    }
    write_file(
        &path,
        &pretty(&Value::Object(settings)),
        env.clock.now(),
        &mut log,
    )?;
    log.push_str("Restart Claude Code to use Privacy AI.\n");
    log.push_str(&claude_warnings(env, t));
    Ok(log)
}

// Codex: https://developers.openai.com/codex/config-reference, /config-advanced

fn codex_dir(env: &Env) -> PathBuf {
    tool_dir(env, "CODEX_HOME", &[".codex"])
}

fn codex_provider_toml(t: &Target) -> String {
    format!(
        "[model_providers.{PROVIDER_ID}]\n\
         name = \"Privacy AI\"\n\
         base_url = {}\n\
         wire_api = \"responses\"\n\
         \n\
         [model_providers.{PROVIDER_ID}.auth]\n\
         command = {}\n\
         args = [\"token\"]\n\
         timeout_ms = 30000\n\
         refresh_interval_ms = 3600000\n",
        toml_str(&format!("{}/v1", t.base_url)),
        toml_str(&t.pai_bin),
    )
}

fn codex_profile_toml(t: &Target) -> String {
    format!(
        "model = \"{MODEL}\"\n\
         model_provider = \"{PROVIDER_ID}\"\n\
         model_context_window = {}\n\
         model_reasoning_summary = \"none\"\n\
         web_search = \"disabled\"\n\
         \n\
         [analytics]\n\
         enabled = false\n",
        t.context
    )
}

fn codex_print(env: &Env, t: &Target) -> String {
    let dir = codex_dir(env);
    format!(
        "Add to {}:\n\n{}\nWrite {}:\n\n{}\nRun `pai setup codex --apply` to write both, then start Codex with \
         `codex --profile {PROVIDER_ID}`.\n",
        dir.join("config.toml").display(),
        codex_provider_toml(t),
        dir.join(format!("{PROVIDER_ID}.config.toml")).display(),
        codex_profile_toml(t),
    )
}

fn codex_apply(env: &Env, t: &Target) -> Result<String> {
    use toml_edit::{DocumentMut, Item, Table};
    let dir = codex_dir(env);
    let path = dir.join("config.toml");
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(io_err("read", &path, e)),
    };
    let mut doc: DocumentMut = text.parse().map_err(|e| {
        Error::Setup(format!(
            "{} isn't valid TOML, so pai won't rewrite it: {e}",
            path.display()
        ))
    })?;
    let snippet: DocumentMut = codex_provider_toml(t)
        .parse()
        .expect("provider TOML parses");
    let mut log = String::new();

    // Since Codex 0.134 a profile is its own file, and a leftover inline one with the same name is a hard error.
    if let Some(profiles) = doc.get_mut("profiles").and_then(Item::as_table_like_mut)
        && profiles.remove(PROVIDER_ID).is_some()
    {
        log.push_str(&format!("Removed the old [profiles.{PROVIDER_ID}] table\n"));
    }
    if doc.get("profile").and_then(Item::as_str) == Some(PROVIDER_ID) {
        doc.remove("profile");
        log.push_str(&format!(
            "Removed `profile = \"{PROVIDER_ID}\"`, which Codex no longer supports\n"
        ));
    }

    if !doc.contains_key("model_providers") {
        let mut table = Table::new();
        table.set_implicit(true);
        doc.insert("model_providers", Item::Table(table));
    }
    let providers = doc["model_providers"].as_table_like_mut().ok_or_else(|| {
        Error::Setup(format!(
            "model_providers in {} isn't a table",
            path.display()
        ))
    })?;
    providers.insert(PROVIDER_ID, snippet["model_providers"][PROVIDER_ID].clone());

    let now = env.clock.now();
    write_file(&path, &doc.to_string(), now, &mut log)?;
    write_file(
        &dir.join(format!("{PROVIDER_ID}.config.toml")),
        &codex_profile_toml(t),
        now,
        &mut log,
    )?;
    log.push_str(&format!(
        "Start Codex with `codex --profile {PROVIDER_ID}` to use Privacy AI.\n"
    ));
    Ok(log)
}

// OpenCode: https://opencode.ai/docs/plugins/, /providers, /config

fn opencode_dir(env: &Env) -> PathBuf {
    match env.var("XDG_CONFIG_HOME") {
        Some(d) => PathBuf::from(d).join("opencode"),
        None => env.paths.home.join(".config").join("opencode"),
    }
}

fn opencode_print(env: &Env, t: &Target) -> String {
    let dir = opencode_dir(env);
    format!(
        "Plugin: {} (written by --apply; OpenCode loads every file in plugins/)\n\
         It runs `{} token` to start pai's local proxy, and talks to it at {}/v1.\n\n\
         Set in {}:\n\n{}\nRun `pai setup opencode --apply` to write both, then restart OpenCode.\n",
        dir.join("plugins").join("privacy-ai.ts").display(),
        t.pai_bin,
        t.base_url,
        dir.join("opencode.json").display(),
        pretty(&json!({ "model": format!("{PROVIDER_ID}/{MODEL}") })),
    )
}

fn opencode_apply(env: &Env, t: &Target) -> Result<String> {
    let dir = opencode_dir(env);
    let now = env.clock.now();
    let mut log = String::new();
    write_file(
        &dir.join("plugins").join("privacy-ai.ts"),
        &js_rewrite(OPENCODE_PLUGIN, t),
        now,
        &mut log,
    )?;

    let json_path = dir.join("opencode.json");
    if !json_path.exists() && dir.join("opencode.jsonc").exists() {
        log.push_str(&format!(
            "Left {} alone (pai doesn't rewrite JSONC). Add \"model\": \"{PROVIDER_ID}/{MODEL}\" to it, or pick the model with /models.\n",
            dir.join("opencode.jsonc").display()
        ));
    } else {
        let mut cfg = read_json_object(&json_path)?;
        if cfg.is_empty() {
            cfg.insert("$schema".into(), json!("https://opencode.ai/config.json"));
        }
        cfg.insert("model".into(), json!(format!("{PROVIDER_ID}/{MODEL}")));
        write_file(&json_path, &pretty(&Value::Object(cfg)), now, &mut log)?;
    }
    log.push_str("Restart OpenCode to use Privacy AI.\n");
    Ok(log)
}

// OMP: https://github.com/can1357/oh-my-pi docs/extensions.md, docs/models.md

fn omp_extension_path(env: &Env) -> PathBuf {
    env.paths
        .home
        .join(".omp")
        .join("agent")
        .join("extensions")
        .join("privacy-ai.ts")
}

fn omp_print(env: &Env, t: &Target) -> String {
    format!(
        "Extension: {} (written by --apply)\n\
         It registers the {PROVIDER_ID} provider at pai's local proxy, {}/v1, with the key command `{} token`.\n\n\
         Run `pai setup omp --apply` to write it, restart OMP, and pick {PROVIDER_ID}/{MODEL} with /model \
         (or start `omp --model {PROVIDER_ID}/{MODEL}`).\n",
        omp_extension_path(env).display(),
        t.base_url,
        t.pai_bin,
    )
}

fn omp_apply(env: &Env, t: &Target) -> Result<String> {
    let mut log = String::new();
    write_file(
        &omp_extension_path(env),
        &js_rewrite(OMP_EXTENSION, t),
        env.clock.now(),
        &mut log,
    )?;
    log.push_str(&format!(
        "Restart OMP and pick {PROVIDER_ID}/{MODEL} with /model, or start `omp --model {PROVIDER_ID}/{MODEL}`.\n"
    ));
    Ok(log)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target {
            base_url: "http://127.0.0.1:47821".into(),
            pai_bin: "/opt/pai bin/pai".into(),
            context: 65536,
            plan_known: true,
        }
    }

    #[test]
    fn rewrites_every_constant_in_both_integrations() {
        for (src, semi) in [(OPENCODE_PLUGIN, ""), (OMP_EXTENSION, ";")] {
            let out = js_rewrite(src, &target());
            assert!(
                out.contains(&format!(
                    "const DEFAULT_BASE_URL = \"http://127.0.0.1:47821\"{semi}\n"
                )),
                "{out}"
            );
            assert!(out.contains(&format!(
                "const DEFAULT_PAI_BIN = \"/opt/pai bin/pai\"{semi}\n"
            )));
            assert!(out.contains(&format!("const CONTEXT_WINDOW = 65536{semi}\n")));
            assert!(!out.contains("api.privacyai.example"));
        }
    }

    #[test]
    fn quotes_helper_paths_with_spaces() {
        assert_eq!(shell_word("/usr/local/bin/pai"), "/usr/local/bin/pai");
        if !cfg!(windows) {
            assert_eq!(shell_word("/a b/it's/pai"), "'/a b/it'\\''s/pai'");
        }
    }
}
