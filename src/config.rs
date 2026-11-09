//! Layered configuration. Later layers win (objects are merged recursively):
//! ~/.config/opencode/opencode.json < ~/.config/zcode/config.json < $OPENCODE_CONFIG_CONTENT < $ZCODE_CONFIG_CONTENT
use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub context: usize,
    pub max_output: u32,
}

pub struct Settings {
    pub main: Config,
    /// cheap model for summaries/compaction (`small_model`)
    pub small: Option<Config>,
    /// config says permission "*" = allow
    pub auto: bool,
    /// config `thinking` (show the model's reasoning); None = unset
    pub thinking: Option<bool>,
    /// merged config (for sandbox/profile keys)
    pub raw: Value,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn read_file(p: PathBuf) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn read_env(name: &str) -> Option<Value> {
    serde_json::from_str(&std::env::var(name).ok()?).ok()
}

pub fn deep_merge(a: &mut Value, b: Value) {
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                deep_merge(a.entry(k).or_insert(Value::Null), v);
            }
        }
        (a, b) => *a = b,
    }
}

pub fn layers() -> Value {
    let mut m = Value::Object(Default::default());
    let xdg = std::env::var("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|_| home().join(".config"));
    for v in [
        read_file(home().join(".config/opencode/opencode.json")),
        read_file(xdg.join("zcode/config.json")),
        read_env("OPENCODE_CONFIG_CONTENT"),
        read_env("ZCODE_CONFIG_CONTENT"),
    ].into_iter().flatten() {
        deep_merge(&mut m, v);
    }
    m
}

#[cfg(test)]
fn resolve(cfg: &Value, model_ref: &str) -> Result<Config> { resolve_with(cfg, model_ref, true) }

/// `use_env`: honor ZCODE_BASE_URL / ZCODE_API_KEY (false for configs supplied by a daemon client).
fn resolve_with(cfg: &Value, model_ref: &str, use_env: bool) -> Result<Config> {
    let env = |k: &str| if use_env { std::env::var(k).ok() } else { None };
    let providers = cfg.get("provider").and_then(|p| p.as_object());
    let names: Vec<&String> = providers.map(|p| p.keys().collect()).unwrap_or_default();
    let (prov_id, model_id) = match model_ref.split_once('/') {
        Some((p, m)) if providers.is_some_and(|ps| ps.contains_key(p)) => (p.to_string(), m.to_string()),
        _ if names.len() == 1 => (names[0].clone(), model_ref.to_string()),
        _ => bail!("provider for model `{model_ref}` not found (configured providers: {})", if names.is_empty() { "none".into() } else { names.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ") }),
    };
    let prov = &providers.unwrap()[&prov_id];
    let opts = prov.get("options").unwrap_or(&Value::Null);
    let limit = prov.get("models").and_then(|m| m.get(&model_id)).and_then(|m| m.get("limit"));
    Ok(Config {
        base_url: env("ZCODE_BASE_URL")
            .or_else(|| opts.get("baseURL").and_then(|v| v.as_str()).map(String::from))
            .ok_or_else(|| anyhow!("provider `{prov_id}` has no options.baseURL"))?
            .trim_end_matches('/').to_string(),
        api_key: env("ZCODE_API_KEY")
            .or_else(|| opts.get("apiKey").and_then(|v| v.as_str()).map(String::from))
            .unwrap_or_default(),
        model: model_id,
        context: limit.and_then(|l| l.get("context")).and_then(|v| v.as_u64()).unwrap_or(128_000) as usize,
        max_output: limit.and_then(|l| l.get("output")).and_then(|v| v.as_u64()).unwrap_or(8192) as u32,
    })
}

fn is_auto(cfg: &Value) -> bool {
    match cfg.get("permission") {
        Some(Value::String(s)) => s == "allow",
        Some(Value::Object(o)) => o.get("*").and_then(|v| v.as_str()) == Some("allow"),
        _ => false,
    }
}

impl Settings {
    pub fn load(model_override: Option<&str>) -> Result<Settings> { Self::from_cfg(layers(), model_override, true) }

    /// Build settings from a config object that is not ours (a daemon client's): the process environment is never consulted.
    pub fn from_client(cfg: Value, model_override: Option<&str>) -> Result<Settings> { Self::from_cfg(cfg, model_override, false) }

    fn from_cfg(cfg: Value, model_override: Option<&str>, use_env: bool) -> Result<Settings> {
        let s = |k: &str| cfg.get(k).and_then(|v| v.as_str()).map(String::from);
        let env = |k: &str| if use_env { std::env::var(k).ok() } else { None };
        let model_ref = model_override.map(String::from)
            .or_else(|| env("ZCODE_MODEL"))
            .or_else(|| env("OPENCODE_MODEL"))
            .or_else(|| s("model"))
            .ok_or_else(|| anyhow!("no model configured: set `model` in config, --model provider/model, or ZCODE_MODEL"))?;
        let main = resolve_with(&cfg, &model_ref, use_env)?;
        let small = s("small_model").filter(|m| *m != model_ref).and_then(|m| resolve_with(&cfg, &m, use_env).ok());
        Ok(Settings { main, small, auto: is_auto(&cfg), thinking: cfg.get("thinking").and_then(|v| v.as_bool()), raw: cfg })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> Value {
        json!({"provider":{
            "a":{"options":{"baseURL":"http://a/v1/","apiKey":"ka"},"models":{"m1":{"limit":{"context":1000,"output":50}}}},
            "b":{"options":{"baseURL":"http://b/v1"},"models":{}}}})
    }
    #[test]
    fn merge_objects_recursively() {
        let mut a = json!({"provider":{"a":{"options":{"baseURL":"x","apiKey":"k"}}},"model":"a/m"});
        deep_merge(&mut a, json!({"provider":{"a":{"options":{"baseURL":"y"}},"b":{}}}));
        assert_eq!(a["provider"]["a"]["options"]["apiKey"], "k");
        assert_eq!(a["provider"]["a"]["options"]["baseURL"], "y");
        assert!(a["provider"]["b"].is_object() && a["model"] == "a/m");
    }
    #[test]
    fn resolves_provider_model_and_limits() {
        let c = resolve(&cfg(), "a/m1").unwrap();
        assert_eq!((c.base_url.as_str(), c.api_key.as_str(), c.model.as_str(), c.context, c.max_output), ("http://a/v1", "ka", "m1", 1000, 50));
        let c = resolve(&cfg(), "b/zzz").unwrap();
        assert_eq!((c.context, c.api_key.as_str()), (128_000, ""));
    }
    #[test]
    fn unknown_provider_is_error_with_several() {
        assert!(resolve(&cfg(), "nope/m").is_err());
        let one = json!({"provider":{"only":{"options":{"baseURL":"http://x"}}}});
        assert_eq!(resolve(&one, "just-a-model").unwrap().model, "just-a-model");
        assert_eq!(resolve(&one, "only/org/model").unwrap().model, "org/model");
    }
    #[test]
    fn permission_allow() {
        assert!(is_auto(&json!({"permission":{"*":"allow"}})));
        assert!(is_auto(&json!({"permission":"allow"})));
        assert!(!is_auto(&json!({"permission":{"bash":"ask"}})));
        assert!(!is_auto(&json!({})));
    }
}
