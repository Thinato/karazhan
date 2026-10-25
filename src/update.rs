//! Startup check for a newer GitHub release (notice only, never installs).

const LATEST: &str = "https://api.github.com/repos/Thinato/karazhan/releases/latest";

/// Tag of the latest release when it is newer than this build. Any failure
/// (no curl, offline, rate limited) is `None`: the check must stay silent.
pub async fn newer_release() -> Option<String> {
    let out = tokio::process::Command::new("curl")
        .args(["-fsSL", "--max-time", "5", LATEST])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let tag = json["tag_name"].as_str()?;
    is_newer(tag, env!("CARGO_PKG_VERSION")).then(|| tag.to_string())
}

/// `v0.4.0` vs `0.3.0`. A tag that isn't plain `x.y.z` (e.g. `-rc1`) is never newer.
fn is_newer(tag: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> {
        v.trim_start_matches('v')
            .split('.')
            .map(|n| n.parse().ok())
            .collect()
    };
    matches!((parse(tag), parse(current)), (Some(t), Some(c)) if t > c)
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn compares_numerically() {
        assert!(is_newer("v0.4.0", "0.3.0"));
        assert!(is_newer("v0.10.0", "0.9.1"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("v0.3.0", "0.3.0"));
        assert!(!is_newer("v0.2.9", "0.3.0"));
        assert!(!is_newer("v0.4.0-rc1", "0.3.0"));
        assert!(!is_newer("garbage", "0.3.0"));
    }
}
