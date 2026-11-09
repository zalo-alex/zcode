//! In-process OCI registry client (cargo feature `oci`): `zcode image pull <ref>` and `zcode image gc`.
//!
//! Registry v2 API with anonymous bearer-token auth, multi-arch index with platform selection, sha256 verification of
//! every manifest and blob, a content-addressed blob cache (`~/.cache/zcode/blobs/sha256/<hex>`), and layers unpacked in
//! order (whiteouts, opaque dirs, path-traversal and symlink-escape protection, device nodes skipped) into the same
//! `~/.cache/zcode/images/<name>/rootfs` that `image import` fills.
use super::image::{images_dir, untar_stream, valid_name};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const MAX_MANIFEST: usize = 4 << 20;
const MAX_BLOB: u64 = 16 << 30;
const PARALLEL: usize = 3;
const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, \
    application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

pub fn blobs_dir() -> PathBuf { crate::local::cache_dir().join("blobs").join("sha256") }

// ---------------------------------------------------------------- references

#[derive(Debug, PartialEq, Eq)]
pub struct Ref {
    /// where to connect (`host[:port]`)
    pub endpoint: String,
    pub repo: String,
    /// a tag, or `sha256:<hex>`
    pub reference: String,
}

fn is_digest(s: &str) -> bool { s.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))) }

pub fn parse_ref(s: &str) -> Result<Ref, String> {
    let (rest, digest) = match s.split_once('@') { Some((r, d)) => (r, Some(d)), None => (s, None) };
    let (path, tag) = match rest.rsplit_once(':') { Some((p, t)) if !t.contains('/') => (p, Some(t)), _ => (rest, None) };
    let (host, repo) = match path.split_once('/') {
        Some((h, r)) if h.contains('.') || h.contains(':') || h == "localhost" => (h.to_string(), r.to_string()),
        _ => ("docker.io".to_string(), path.to_string()),
    };
    let endpoint = if host == "docker.io" || host == "index.docker.io" { "registry-1.docker.io".to_string() } else { host.clone() };
    let repo = if endpoint == "registry-1.docker.io" && !repo.contains('/') { format!("library/{repo}") } else { repo };
    if repo.is_empty() || repo.split('/').any(|c| c.is_empty() || c == "." || c == ".." || !c.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))) {
        return Err(format!("invalid image reference `{s}`: bad repository name"));
    }
    if endpoint.is_empty() || !endpoint.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']')) {
        return Err(format!("invalid image reference `{s}`: bad registry host"));
    }
    let reference = match (digest, tag) {
        (Some(d), _) if is_digest(d) => d.to_string(),
        (Some(_), _) => return Err(format!("invalid image reference `{s}`: only sha256 digests are supported")),
        (None, Some(t)) if !t.is_empty() && t.len() <= 128 && !t.starts_with(['.', '-']) && t.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')) => t.to_string(),
        (None, Some(_)) => return Err(format!("invalid image reference `{s}`: bad tag")),
        (None, None) => "latest".to_string(),
    };
    Ok(Ref { endpoint, repo, reference })
}

fn loopback(endpoint: &str) -> bool {
    let h = endpoint.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map(|(h, _)| h).unwrap_or(endpoint);
    h == "localhost" || h.starts_with("127.") || h == "[::1]"
}

// ---------------------------------------------------------------- platform

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Platform { pub os: String, pub arch: String, pub variant: Option<String> }

impl Platform {
    pub fn host() -> Platform {
        let arch = match std::env::consts::ARCH { "x86_64" => "amd64", "aarch64" => "arm64", "x86" => "386", "arm" => "arm", o => o };
        Platform { os: "linux".into(), arch: arch.into(), variant: None }
    }
    /// `os/arch[/variant]` or just `arch`
    pub fn parse(s: &str) -> Result<Platform, String> {
        let p: Vec<&str> = s.split('/').collect();
        match p.as_slice() {
            [a] if !a.is_empty() => Ok(Platform { os: "linux".into(), arch: a.to_string(), variant: None }),
            [o, a] if !o.is_empty() && !a.is_empty() => Ok(Platform { os: o.to_string(), arch: a.to_string(), variant: None }),
            [o, a, v] if !o.is_empty() && !a.is_empty() && !v.is_empty() => Ok(Platform { os: o.to_string(), arch: a.to_string(), variant: Some(v.to_string()) }),
            _ => Err(format!("invalid platform `{s}` (expected os/arch[/variant], e.g. linux/arm64)")),
        }
    }
    fn matches(&self, m: &Value) -> bool {
        let p = &m["platform"];
        p["os"].as_str() == Some(&self.os) && p["architecture"].as_str() == Some(&self.arch)
            && self.variant.as_deref().is_none_or(|v| p["variant"].as_str() == Some(v))
    }
    fn show(&self) -> String { format!("{}/{}{}", self.os, self.arch, self.variant.as_ref().map(|v| format!("/{v}")).unwrap_or_default()) }
}

// ---------------------------------------------------------------- the client

struct Reg {
    http: reqwest::Client,
    base: String,
    repo: String,
    token: tokio::sync::Mutex<Option<String>>,
}

/// `Bearer realm="...",service="...",scope="..."` -> pairs
fn parse_challenge(h: &str) -> Option<Vec<(String, String)>> {
    let rest = h.trim().strip_prefix("Bearer ").or_else(|| h.trim().strip_prefix("bearer "))?;
    let (mut out, mut key, mut val) = (vec![], String::new(), String::new());
    let (mut in_val, mut quoted) = (false, false);
    for c in rest.chars() {
        match (c, in_val, quoted) {
            ('=', false, _) => in_val = true,
            ('"', true, _) => quoted = !quoted,
            (',', true, false) => { out.push((key.trim().to_string(), std::mem::take(&mut val))); key.clear(); in_val = false; }
            (c, true, _) => val.push(c),
            (c, false, _) => key.push(c),
        }
    }
    if in_val { out.push((key.trim().to_string(), val)); }
    Some(out)
}

impl Reg {
    fn new(r: &Ref) -> Result<Reg, String> {
        let http = reqwest::Client::builder().user_agent(concat!("zcode/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(20)).read_timeout(Duration::from_secs(60)).build().map_err(|e| e.to_string())?;
        let scheme = if loopback(&r.endpoint) { "http" } else { "https" };
        Ok(Reg { http, base: format!("{scheme}://{}", r.endpoint), repo: r.repo.clone(), token: Default::default() })
    }

    async fn authenticate(&self, challenge: &str) -> Result<(), String> {
        let Some(p) = parse_challenge(challenge) else {
            return Err("the registry requires credentials (only anonymous bearer-token pulls are supported)".into());
        };
        let get = |k: &str| p.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone());
        let realm = get("realm").ok_or("the registry's auth challenge has no realm")?;
        let mut url = reqwest::Url::parse(&realm).map_err(|e| format!("bad auth realm `{realm}`: {e}"))?;
        // never let a registry reached over https send us to a plaintext token endpoint
        if url.scheme() != "https" && !(url.scheme() == "http" && (self.base.starts_with("http://") || url.host_str().is_some_and(loopback))) {
            return Err(format!("refusing to fetch a token from {url}: not https"));
        }
        {
            let mut q = url.query_pairs_mut();
            if let Some(s) = get("service") { q.append_pair("service", &s); }
            q.append_pair("scope", &get("scope").unwrap_or_else(|| format!("repository:{}:pull", self.repo)));
        }
        let r = self.http.get(url).send().await.map_err(|e| format!("token request failed: {e}"))?;
        if !r.status().is_success() { return Err(format!("token request failed: HTTP {} (private images need credentials, which are not supported)", r.status())); }
        let v: Value = r.json().await.map_err(|e| format!("bad token response: {e}"))?;
        let t = v["token"].as_str().or_else(|| v["access_token"].as_str()).ok_or("token response has no token")?;
        *self.token.lock().await = Some(t.to_string());
        Ok(())
    }

    async fn get(&self, path: &str, accept: &str) -> Result<reqwest::Response, String> {
        let url = format!("{}{path}", self.base);
        for attempt in 0..2 {
            let mut rq = self.http.get(&url).header(reqwest::header::ACCEPT, accept);
            if let Some(t) = self.token.lock().await.as_ref() { rq = rq.bearer_auth(t); }
            let r = rq.send().await.map_err(|e| format!("{url}: {e}"))?;
            if r.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                let ch = r.headers().get(reqwest::header::WWW_AUTHENTICATE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                self.authenticate(&ch).await?;
                continue;
            }
            if !r.status().is_success() {
                return Err(match r.status().as_u16() { 404 => format!("{url}: not found (check the image name and tag)"), 401 | 403 => format!("{url}: access denied (private images need credentials, which are not supported)"), c => format!("{url}: HTTP {c}") });
            }
            return Ok(r);
        }
        Err(format!("{url}: unauthorized"))
    }

    /// Fetch a manifest or index; when `reference` is a digest the bytes must hash to it.
    async fn manifest(&self, reference: &str) -> Result<(Value, String), String> {
        let mut r = self.get(&format!("/v2/{}/manifests/{reference}", self.repo), ACCEPT).await?;
        let mut body = Vec::new();
        while let Some(c) = r.chunk().await.map_err(|e| e.to_string())? {
            if body.len() + c.len() > MAX_MANIFEST { return Err("manifest is larger than 4 MiB".into()); }
            body.extend_from_slice(&c);
        }
        let digest = format!("sha256:{}", hex(&Sha256::digest(&body)));
        if is_digest(reference) && digest != reference { return Err(format!("manifest digest mismatch: expected {reference}, got {digest}")); }
        let v: Value = serde_json::from_slice(&body).map_err(|e| format!("bad manifest: {e}"))?;
        Ok((v, digest))
    }

    /// Download one blob into the cache (verified: size and sha256), returning its path. A cache hit costs no request.
    async fn blob(&self, digest: &str, size: u64) -> Result<(PathBuf, bool), String> {
        if !is_digest(digest) { return Err(format!("unsupported digest `{digest}` (only sha256)")); }
        let fin = blobs_dir().join(&digest[7..]);
        if std::fs::metadata(&fin).is_ok_and(|m| m.len() == size) { return Ok((fin, true)); }
        if size > MAX_BLOB { return Err(format!("blob {digest} is larger than {} GiB", MAX_BLOB >> 30)); }
        std::fs::create_dir_all(blobs_dir()).map_err(|e| format!("{}: {e}", blobs_dir().display()))?;
        let part = fin.with_extension(format!("part{}", std::process::id()));
        let r = self.download(digest, size, &part).await;
        match r {
            Ok(()) => { std::fs::rename(&part, &fin).map_err(|e| e.to_string())?; Ok((fin, false)) }
            Err(e) => { let _ = std::fs::remove_file(&part); Err(e) }
        }
    }

    async fn download(&self, digest: &str, size: u64, part: &Path) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let mut r = self.get(&format!("/v2/{}/blobs/{digest}", self.repo), "*/*").await?;
        let mut f = tokio::fs::File::create(part).await.map_err(|e| format!("{}: {e}", part.display()))?;
        let (mut h, mut n) = (Sha256::new(), 0u64);
        while let Some(c) = r.chunk().await.map_err(|e| format!("downloading {digest}: {e}"))? {
            n += c.len() as u64;
            if n > size { return Err(format!("blob {digest} is larger than the {size} bytes its manifest declares")); }
            h.update(&c);
            f.write_all(&c).await.map_err(|e| e.to_string())?;
        }
        f.flush().await.map_err(|e| e.to_string())?;
        if n != size { return Err(format!("blob {digest}: expected {size} bytes, got {n}")); }
        let got = format!("sha256:{}", hex(&h.finalize()));
        if got != digest { return Err(format!("sha256 mismatch for blob {digest} (got {got}); refusing to use it")); }
        Ok(())
    }
}

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

// ---------------------------------------------------------------- pull

/// Resolve an index to the image manifest for `want`.
async fn resolve(reg: &Reg, first: Value, first_digest: String, want: &Platform) -> Result<(Value, String), String> {
    let (mut m, mut digest) = (first, first_digest);
    for _ in 0..3 {
        if let Some(list) = m["manifests"].as_array() {
            let pick = list.iter().find(|e| want.matches(e)).ok_or_else(|| {
                let have: Vec<String> = list.iter().filter_map(|e| Some(format!("{}/{}", e["platform"]["os"].as_str()?, e["platform"]["architecture"].as_str()?))).filter(|s| !s.starts_with("unknown")).collect();
                format!("no image for {} in this index (available: {}); pick one with --platform", want.show(), have.join(", "))
            })?;
            let d = pick["digest"].as_str().ok_or("index entry without a digest")?.to_string();
            (m, digest) = reg.manifest(&d).await?;
        } else if m["layers"].is_array() {
            return Ok((m, digest));
        } else if m["fsLayers"].is_array() {
            return Err("this image uses the obsolete schema 1 manifest, which is not supported".into());
        } else {
            return Err("unrecognised manifest".into());
        }
    }
    Err("manifest index nests too deeply".into())
}

fn open_layer(p: &Path) -> Result<Box<dyn Read>, String> {
    let mut f = std::fs::File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let mut magic = [0u8; 2];
    let n = f.read(&mut magic).unwrap_or(0);
    if n == 2 && magic == [0x1f, 0x8b] {
        let f = std::fs::File::open(p).map_err(|e| e.to_string())?;
        return Ok(Box::new(flate2::read::MultiGzDecoder::new(std::io::BufReader::with_capacity(256 << 10, f))));
    }
    // plain tar reads directly; zstd/xz/bzip2 layers use the system decompressor like `image import`
    super::image::open_stream(p)
}

/// `zcode image pull`: returns a one-line summary (plus any warnings).
pub async fn pull(reference: &str, name: &str, platform: Option<&str>) -> Result<String, String> {
    if !valid_name(name) { return Err(format!("invalid image name `{name}` (letters, digits, . _ -)")); }
    let dest = images_dir().join(name);
    if dest.exists() { return Err(format!("image `{name}` already exists; remove it first with `zcode image rm {name}`")); }
    let want = match platform { Some(p) => Platform::parse(p)?, None => Platform::host() };
    let r = parse_ref(reference)?;
    let reg = Arc::new(Reg::new(&r)?);
    let (first, d0) = reg.manifest(&r.reference).await?;
    let (m, digest) = resolve(&reg, first, d0, &want).await?;
    let layers: Vec<(String, u64)> = m["layers"].as_array().into_iter().flatten().map(|l| {
        if l["urls"].is_array() { return Err("this image has foreign (non-distributable) layers, which are not supported".to_string()); }
        Ok((l["digest"].as_str().ok_or("layer without a digest")?.to_string(), l["size"].as_u64().ok_or("layer without a size")?))
    }).collect::<Result<_, String>>()?;
    let total: u64 = layers.iter().map(|l| l.1).sum();
    eprintln!("pulling {reference} ({}, {} layers, {:.1} MB)", want.show(), layers.len(), total as f64 / 1e6);

    // download (3 at a time), each verified; the cache makes a repeat pull free
    let sem = Arc::new(tokio::sync::Semaphore::new(PARALLEL));
    let mut set = tokio::task::JoinSet::new();
    let count = layers.len();
    for (i, (d, size)) in layers.iter().cloned().enumerate() {
        let (reg, sem) = (reg.clone(), sem.clone());
        set.spawn(async move {
            let _p = sem.acquire().await.map_err(|e| e.to_string())?;
            let (path, hit) = reg.blob(&d, size).await?;
            eprintln!("  layer {}/{count} {} {}", i + 1, &d[7..19], if hit { "(cached)" } else { "done" });
            Ok::<(usize, PathBuf), String>((i, path))
        });
    }
    let mut paths: Vec<Option<PathBuf>> = vec![None; layers.len()];
    while let Some(j) = set.join_next().await {
        match j.map_err(|e| e.to_string()).and_then(|x| x) {
            Ok((i, p)) => paths[i] = Some(p),
            Err(e) => { set.abort_all(); return Err(e); }
        }
    }
    let paths: Vec<PathBuf> = paths.into_iter().flatten().collect();

    // unpack in order into a temp dir, then rename into place
    let tmp = images_dir().join(format!(".{name}.tmp{}", std::process::id()));
    let (name_s, ref_s, digest_s, plat, layer_ids) = (name.to_string(), reference.to_string(), digest.clone(), want.show(), layers.iter().map(|l| l.0.clone()).collect::<Vec<_>>());
    let dest2 = dest.clone();
    tokio::task::spawn_blocking(move || -> Result<String, String> {
        super::force_remove(&tmp);
        let root = tmp.join("rootfs");
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let mut warnings = vec![];
        let res = (|| {
            for p in &paths { untar_stream(open_layer(p)?, &root, &mut warnings, true)?; }
            let meta = json!({"ref": ref_s, "manifest": digest_s, "platform": plat, "layers": layer_ids});
            std::fs::write(tmp.join("image.json"), meta.to_string()).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, &dest2).map_err(|e| e.to_string())
        })();
        if let Err(e) = res { super::force_remove(&tmp); return Err(e); }
        let mut s = format!("pulled `{ref_s}` as `{name_s}` into {}", dest2.join("rootfs").display());
        for w in warnings.iter().take(10) { s.push_str(&format!("\nwarning: {w}")); }
        if warnings.len() > 10 { s.push_str(&format!("\nwarning: ... and {} more", warnings.len() - 10)); }
        Ok(s)
    }).await.map_err(|e| e.to_string())?
}

// ---------------------------------------------------------------- gc

/// `zcode image gc`: delete cached blobs no installed image refers to, plus leftovers of interrupted pulls.
pub fn gc() -> Result<String, String> {
    let mut keep = std::collections::HashSet::new();
    for e in std::fs::read_dir(images_dir()).into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.starts_with('.') {
            // an interrupted pull or import: remove it unless its process is still alive
            let pid = n.rsplit_once(".tmp").and_then(|(_, p)| p.parse::<i32>().ok());
            if pid.is_none_or(|p| unsafe { libc::kill(p, 0) } != 0) { super::force_remove(&e.path()); }
            continue;
        }
        let Ok(s) = std::fs::read_to_string(e.path().join("image.json")) else { continue };
        let v: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
        for l in v["layers"].as_array().into_iter().flatten().filter_map(|l| l.as_str()) { keep.insert(l.trim_start_matches("sha256:").to_string()); }
    }
    let (mut n, mut bytes) = (0u64, 0u64);
    for e in std::fs::read_dir(blobs_dir()).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let part_pid = name.rsplit_once(".part").and_then(|(_, p)| p.parse::<i32>().ok());
        let stale_part = part_pid.is_some_and(|p| unsafe { libc::kill(p, 0) } != 0);
        if (part_pid.is_none() && keep.contains(&name)) || (part_pid.is_some() && !stale_part) { continue; }
        bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
        if std::fs::remove_file(e.path()).is_ok() { n += 1; }
    }
    Ok(format!("removed {n} cached blob(s), freed {:.1} MB", bytes as f64 / 1e6))
}

#[cfg(test)]
mod tests;
