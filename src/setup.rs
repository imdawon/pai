use std::fs;
use std::path::{Path, PathBuf};

use pai_proto::Plan;
use serde_json::{Map, Value, json};

use crate::app::Env;
use crate::error::{Error, Result, io_err};

pub const MODEL: &str = "qwen3.8-27b";
pub const PROVIDER_ID: &str = "dawonai";
const OPENCODE_PLUGIN: &str = include_str!("../integrations/opencode/src/index.ts");
const OMP_EXTENSION: &str = include_str!("../integrations/omp/dawonai.ts");
pub const SKILL_NAME: &str = "dawonai";
const SKILL: &str = include_str!("../skill/dawonai/SKILL.md");

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Tool {
    ClaudeCode,
    Codex,
    Opencode,
    Omp,
    Hermes,
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
        (Tool::Hermes, false) => hermes_print(env, target),
        (Tool::Hermes, true) => hermes_apply(env, target)?,
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

/// JSON strings are YAML double-quoted scalars.
fn yaml_str(s: &str) -> String {
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

/// Where each harness loads personal (not project) skills from, as `<root>/<name>/SKILL.md`.
pub fn skill_path(env: &Env, tool: Tool) -> PathBuf {
    let root = match tool {
        // https://code.claude.com/docs/en/skills, /claude-directory
        Tool::ClaudeCode => claude_dir(env).join("skills"),
        // https://learn.chatgpt.com/docs/build-skills lists $HOME/.agents/skills for user skills.
        // CODEX_HOME doesn't move it; $CODEX_HOME/skills is still read but deprecated.
        Tool::Codex => env.paths.home.join(".agents").join("skills"),
        // https://opencode.ai/docs/skills/
        Tool::Opencode => opencode_dir(env).join("skills"),
        // oh-my-pi docs/skills.md
        Tool::Omp => omp_agent_dir(env).join("skills"),
        // https://hermes-agent.nousresearch.com/docs/user-guide/features/skills
        Tool::Hermes => hermes_dir(env).join("skills"),
    };
    root.join(SKILL_NAME).join("SKILL.md")
}

fn skill_print(env: &Env, tool: Tool) -> String {
    format!(
        "Skill: {} (written by --apply). The DawonAI skill tells the agent how pai works and how to fix it.\n\n",
        skill_path(env, tool).display()
    )
}

fn skill_apply(env: &Env, tool: Tool, log: &mut String) -> Result<()> {
    write_file(&skill_path(env, tool), SKILL, env.clock.now(), log)?;
    log.push_str("Installed the DawonAI skill, so the agent knows how pai works.\n");
    Ok(())
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

fn claude_dir(env: &Env) -> PathBuf {
    tool_dir(env, "CLAUDE_CONFIG_DIR", &[".claude"])
}

fn claude_settings_path(env: &Env) -> PathBuf {
    claude_dir(env).join("settings.json")
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
        "Merge into {}:\n\n{}\n{}Run `pai setup claude-code --apply` to write both. Claude Code runs `pai token` \
         at startup, every hour, and after a 401; it starts pai's local proxy and prints the key for it.\n{}",
        claude_settings_path(env).display(),
        pretty(&claude_settings(t)),
        skill_print(env, Tool::ClaudeCode),
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
    skill_apply(env, Tool::ClaudeCode, &mut log)?;
    log.push_str("Restart Claude Code to use DawonAI.\n");
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
         name = \"DawonAI\"\n\
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
        "Add to {}:\n\n{}\nWrite {}:\n\n{}\n{}Run `pai setup codex --apply` to write all three, then start Codex with \
         `codex --profile {PROVIDER_ID}`.\n",
        dir.join("config.toml").display(),
        codex_provider_toml(t),
        dir.join(format!("{PROVIDER_ID}.config.toml")).display(),
        codex_profile_toml(t),
        skill_print(env, Tool::Codex),
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
    skill_apply(env, Tool::Codex, &mut log)?;
    log.push_str(&format!(
        "Start Codex with `codex --profile {PROVIDER_ID}` to use DawonAI.\n"
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
         Set in {}:\n\n{}\n{}Run `pai setup opencode --apply` to write all three, then restart OpenCode.\n",
        dir.join("plugins").join("dawonai.ts").display(),
        t.pai_bin,
        t.base_url,
        dir.join("opencode.json").display(),
        pretty(&json!({ "model": format!("{PROVIDER_ID}/{MODEL}") })),
        skill_print(env, Tool::Opencode),
    )
}

fn opencode_apply(env: &Env, t: &Target) -> Result<String> {
    let dir = opencode_dir(env);
    let now = env.clock.now();
    let mut log = String::new();
    write_file(
        &dir.join("plugins").join("dawonai.ts"),
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
    skill_apply(env, Tool::Opencode, &mut log)?;
    log.push_str("Restart OpenCode to use DawonAI.\n");
    Ok(log)
}

// OMP: https://github.com/can1357/oh-my-pi docs/extensions.md, docs/models.md

/// PI_CODING_AGENT_DIR replaces OMP's agent directory (oh-my-pi packages/utils/src/dirs.ts).
fn omp_agent_dir(env: &Env) -> PathBuf {
    tool_dir(env, "PI_CODING_AGENT_DIR", &[".omp", "agent"])
}

fn omp_extension_path(env: &Env) -> PathBuf {
    omp_agent_dir(env).join("extensions").join("dawonai.ts")
}

fn omp_print(env: &Env, t: &Target) -> String {
    format!(
        "Extension: {} (written by --apply)\n\
         It registers the {PROVIDER_ID} provider at pai's local proxy, {}/v1, with the key command `{} token`.\n\n\
         {}Run `pai setup omp --apply` to write both, restart OMP, and pick {PROVIDER_ID}/{MODEL} with /model \
         (or start `omp --model {PROVIDER_ID}/{MODEL}`).\n",
        omp_extension_path(env).display(),
        t.base_url,
        t.pai_bin,
        skill_print(env, Tool::Omp),
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
    skill_apply(env, Tool::Omp, &mut log)?;
    log.push_str(&format!(
        "Restart OMP and pick {PROVIDER_ID}/{MODEL} with /model, or start `omp --model {PROVIDER_ID}/{MODEL}`.\n"
    ));
    Ok(log)
}

// Hermes Agent: https://github.com/NousResearch/hermes-agent cli-config.yaml.example,
// https://hermes-agent.nousresearch.com/docs/integrations/providers

const HERMES_BEGIN: &str = "# dawonai (managed by pai setup hermes)";
const HERMES_END: &str = "# end dawonai";

fn hermes_dir(env: &Env) -> PathBuf {
    if let Some(d) = env.var("HERMES_HOME") {
        return match d.strip_prefix("~/").or_else(|| d.strip_prefix("~\\")) {
            Some(rest) => env.paths.home.join(rest),
            None => PathBuf::from(d),
        };
    }
    if cfg!(windows)
        && let Some(local) = env.var("LOCALAPPDATA")
    {
        return PathBuf::from(local).join("hermes");
    }
    env.paths.home.join(".hermes")
}

fn hermes_config_path(env: &Env) -> PathBuf {
    hermes_dir(env).join("config.yaml")
}

/// Hermes runs `key_cmd` for its key, as Claude Code runs apiKeyHelper, so no key is written into
/// config.yaml and `pai token` starts the proxy along with Hermes.
fn hermes_key_cmd(t: &Target) -> String {
    format!("{} token", shell_word(&t.pai_bin))
}

fn hermes_block(t: &Target, indent: &str, step: &str, nl: &str) -> String {
    let i1 = format!("{indent}{step}");
    let i2 = format!("{i1}{step}");
    let i3 = format!("{i2}{step}");
    [
        format!("{indent}{HERMES_BEGIN}"),
        format!("{indent}{PROVIDER_ID}:"),
        format!("{i1}name: DawonAI"),
        format!("{i1}api: {}", yaml_str(&format!("{}/v1", t.base_url))),
        format!("{i1}key_cmd: {}", yaml_str(&hermes_key_cmd(t))),
        format!("{i1}api_mode: chat_completions"),
        format!("{i1}default_model: {MODEL}"),
        format!("{i1}models:"),
        format!("{i2}{MODEL}:"),
        format!("{i3}context_length: {}", t.context),
        format!("{indent}{HERMES_END}"),
    ]
    .iter()
    .map(|l| format!("{l}{nl}"))
    .collect()
}

fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

/// The end of the mapping entry whose key is on `lines[start]`: every following line that's
/// blank or indented deeper, minus trailing blank lines.
fn entry_end(lines: &[&str], start: usize, indent: usize) -> usize {
    let mut end = start + 1;
    let mut last = end;
    while end < lines.len() && (is_blank(lines[end]) || indent_of(lines[end]).len() > indent) {
        end += 1;
        if !is_blank(lines[end - 1]) {
            last = end;
        }
    }
    last
}

/// Puts our provider into a Hermes config.yaml by editing the text, so the user's comments and
/// layout survive: a YAML library would drop them. Replaces our marked block, or an unmarked
/// `dawonai` provider, or adds the block under `providers:`, creating that if needed.
fn hermes_merge(text: &str, path: &Path, t: &Target) -> Result<(String, &'static str)> {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let splice = |from: usize, to: usize, block: String| -> String {
        let mut out: String = lines[..from].concat();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(nl);
        }
        out.push_str(&block);
        out.push_str(&lines[to..].concat());
        out
    };

    if let Some(begin) = lines.iter().position(|l| l.trim() == HERMES_BEGIN) {
        let indent = indent_of(lines[begin]);
        let end = match lines[begin + 1..].iter().position(|l| l.trim() == HERMES_END) {
            Some(n) => begin + 1 + n + 1,
            None => match lines.get(begin + 1) {
                Some(l) if indent_of(l) == indent && !is_comment(l) => entry_end(&lines, begin + 1, indent.len()),
                _ => begin + 1,
            },
        };
        let step = if indent.is_empty() { "  " } else { indent };
        return Ok((splice(begin, end, hermes_block(t, indent, step, nl)), "Replaced"));
    }

    let Some(p) = lines.iter().position(|l| l.starts_with("providers:")) else {
        let mut out = text.to_string();
        if !out.is_empty() {
            if !out.ends_with('\n') {
                out.push_str(nl);
            }
            out.push_str(nl);
        }
        out.push_str(&format!("providers:{nl}"));
        out.push_str(&hermes_block(t, "  ", "  ", nl));
        return Ok((out, "Added"));
    };

    let rest = lines[p]["providers:".len()..].trim();
    let value = rest.split(" #").next().unwrap_or("").trim();
    let empty_flow = value == "{}" || value == "~" || value == "null";
    if !value.is_empty() && !value.starts_with('#') && !empty_flow {
        return Err(Error::Setup(format!(
            "`providers:` in {} is written on one line, so pai won't edit it. Run `pai setup hermes` without --apply \
             and add the provider by hand",
            path.display()
        )));
    }

    let children_end = (p + 1..lines.len())
        .find(|&i| !is_blank(lines[i]) && !is_comment(lines[i]) && indent_of(lines[i]).is_empty())
        .unwrap_or(lines.len());
    let first_child = (p + 1..children_end).find(|&i| !is_blank(lines[i]) && !is_comment(lines[i]));
    let indent = first_child.map(|i| indent_of(lines[i])).filter(|i| !i.is_empty()).unwrap_or("  ");

    let ours = (p + 1..children_end).find(|&i| {
        indent_of(lines[i]) == indent && {
            let k = lines[i].trim();
            [PROVIDER_ID.to_string(), format!("\"{PROVIDER_ID}\""), format!("'{PROVIDER_ID}'")]
                .iter()
                .any(|q| k.strip_prefix(q.as_str()).is_some_and(|r| r.trim_start().starts_with(':')))
        }
    });
    if let Some(start) = ours {
        let end = entry_end(&lines, start, indent.len());
        return Ok((splice(start, end, hermes_block(t, indent, indent, nl)), "Replaced"));
    }

    let header = if empty_flow {
        let comment = rest.find(" #").map(|i| &rest[i..]).unwrap_or("");
        format!("providers:{comment}{nl}")
    } else {
        lines[p].to_string()
    };
    let mut out: String = lines[..p].concat();
    out.push_str(&header);
    if !out.ends_with('\n') {
        out.push_str(nl);
    }
    out.push_str(&hermes_block(t, indent, indent, nl));
    out.push_str(&lines[p + 1..].concat());
    Ok((out, "Added"))
}

fn hermes_switch_hint() -> String {
    format!(
        "Your default model stays as it is. To use DawonAI, type `/model custom:{PROVIDER_ID}:{MODEL}` in Hermes \
         (add `--global` to make it the default), or run `hermes model` and pick DawonAI.\n"
    )
}

fn hermes_print(env: &Env, t: &Target) -> String {
    format!(
        "Add under `providers:` in {}:\n\n{}\nHermes runs `pai token` for its key (it needs a Hermes release from \
         August 2026 or later, which supports `key_cmd`).\n\n{}Run `pai setup hermes --apply` to write both.\n\n{}",
        hermes_config_path(env).display(),
        hermes_block(t, "  ", "  ", "\n"),
        skill_print(env, Tool::Hermes),
        hermes_switch_hint()
    )
}

fn hermes_apply(env: &Env, t: &Target) -> Result<String> {
    let path = hermes_config_path(env);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(io_err("read", &path, e)),
    };
    let (merged, what) = hermes_merge(&text, &path, t)?;
    let mut log = String::new();
    write_file(&path, &merged, env.clock.now(), &mut log)?;
    log.push_str(&format!("{what} the {PROVIDER_ID} provider. Hermes runs `pai token` for its key.\n"));
    skill_apply(env, Tool::Hermes, &mut log)?;
    log.push_str(&hermes_switch_hint());
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
