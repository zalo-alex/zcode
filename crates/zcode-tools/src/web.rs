use super::s;
use anyhow::{Result, bail};
use regex::Regex;
use serde_json::Value;

pub async fn fetch(a: &Value) -> Result<String> {
    let url = s(a, "url")?;
    if !url.starts_with("http") { bail!("url must be http(s)"); }
    let r = reqwest::Client::builder().timeout(std::time::Duration::from_secs(20)).user_agent("zcode/0.1").build()?.get(url).send().await?;
    if !r.status().is_success() { bail!("HTTP {}", r.status()); }
    let ct = r.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body = r.text().await?;
    if !ct.contains("html") { return Ok(body); }
    html_to_text(&body)
}

const EXA_URL: &str = "https://mcp.exa.ai/mcp";
const NO_RESULTS: &str = "No search results found. Try a different query.";

pub async fn search(a: &Value) -> Result<String> {
    let query = s(a, "query")?;
    let mut args = serde_json::json!({
        "query": query,
        "type": a["type"].as_str().unwrap_or("auto"),
        "numResults": a["numResults"].as_u64().unwrap_or(8),
        "livecrawl": a["livecrawl"].as_str().unwrap_or("fallback"),
    });
    if let Some(n) = a["contextMaxCharacters"].as_u64() { args["contextMaxCharacters"] = n.into(); }
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "web_search_exa", "arguments": args},
    });
    let url = std::env::var("ZCODE_WEBSEARCH_URL").unwrap_or_else(|_| EXA_URL.into());
    let r = reqwest::Client::builder().timeout(std::time::Duration::from_secs(25)).user_agent("zcode/0.1").build()?
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .json(&body)
        .send().await?;
    if !r.status().is_success() { bail!("HTTP {}", r.status()); }
    parse_response(&r.text().await?)
}

/// Exa answers JSON-RPC either as SSE (`data: {...}` lines) or plain JSON.
pub fn parse_response(body: &str) -> Result<String> {
    let mut msgs: Vec<Value> = body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str(d.trim()).ok())
        .collect();
    if msgs.is_empty() {
        if let Ok(v) = serde_json::from_str::<Value>(body.trim()) { msgs.push(v); }
    }
    for m in &msgs {
        if let Some(e) = m["error"]["message"].as_str() { bail!("search error: {e}"); }
        if let Some(t) = m["result"]["content"][0]["text"].as_str() {
            return Ok(if t.trim().is_empty() { NO_RESULTS.into() } else { t.to_string() });
        }
    }
    Ok(NO_RESULTS.into())
}

pub fn html_to_text(body: &str) -> Result<String> {
    let strip = Regex::new(r"(?is)<script[^>]*>.*?</script>|<style[^>]*>.*?</style>|<!--.*?-->")?;
    let tags = Regex::new(r"(?s)<[^>]+>")?;
    let ws = Regex::new(r"\n\s*\n+")?;
    let s1 = strip.replace_all(body, "").into_owned();
    let t = tags.replace_all(&s1, "");
    let t = t.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&nbsp;", " ");
    Ok(ws.replace_all(&t, "\n\n").trim().to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_sse() {
        let b = "event: message\ndata: {\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"hit\"}]},\"jsonrpc\":\"2.0\",\"id\":1}\n\n";
        assert_eq!(super::parse_response(b).unwrap(), "hit");
    }
    #[test]
    fn parses_json_error_empty() {
        assert_eq!(super::parse_response("{\"result\":{\"content\":[{\"text\":\"x\"}]}}").unwrap(), "x");
        assert!(super::parse_response("{\"error\":{\"message\":\"boom\"}}").unwrap_err().to_string().contains("boom"));
        assert!(super::parse_response("").unwrap().starts_with("No search results"));
        assert!(super::parse_response("{\"result\":{\"content\":[{\"text\":\" \"}]}}").unwrap().starts_with("No search results"));
    }
    #[test]
    fn strips_html() {
        let t = super::html_to_text("<html><style>p{}</style><script>var x=1;</script><p>Hello &amp; <b>world</b></p><!-- c --></html>").unwrap();
        assert_eq!(t, "Hello & world");
    }
}
