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
    fn strips_html() {
        let t = super::html_to_text("<html><style>p{}</style><script>var x=1;</script><p>Hello &amp; <b>world</b></p><!-- c --></html>").unwrap();
        assert_eq!(t, "Hello & world");
    }
}
