//! `archix update` for home-manager: the latest nixos-unstable and home-manager master commits, through an injectable fetch.

use anyhow::{Context, Result, bail};

use crate::lock::HmLock;

/// nixos-unstable's commit, as plain text, from the channel that only moves once Hydra has built it.
pub const NIXPKGS_URL: &str = "https://channels.nixos.org/nixos-unstable/git-revision";
/// home-manager master's head, which tracks nixos-unstable.
pub const HOME_MANAGER_URL: &str = "https://api.github.com/repos/nix-community/home-manager/commits/master";

pub fn is_rev(rev: &str) -> bool {
    rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn rev_in(body: &str, url: &str) -> Result<String> {
    let body = body.trim();
    let rev = match body.starts_with('{') {
        true => serde_json::from_str::<serde_json::Value>(body)
            .with_context(|| format!("{url} answered with bad JSON"))?
            .get("sha")
            .and_then(|sha| sha.as_str())
            .map(str::to_string)
            .with_context(|| format!("{url} answered without a sha"))?,
        false => body.to_string(),
    };
    if !is_rev(&rev) {
        bail!("{url} answered {rev:?}, not a commit");
    }
    Ok(rev)
}

/// The newest revs, `fetch` returning a URL's body.
pub fn resolve(fetch: &dyn Fn(&str) -> Result<String>) -> Result<HmLock> {
    Ok(HmLock {
        home_manager: rev_in(&fetch(HOME_MANAGER_URL)?, HOME_MANAGER_URL)?,
        nixpkgs: rev_in(&fetch(NIXPKGS_URL)?, NIXPKGS_URL)?,
    })
}

/// A URL's body over HTTPS.
pub fn http_get(url: &str) -> Result<String> {
    let mut response = ureq::get(url)
        .header("User-Agent", "archix")
        .header("Accept", "application/vnd.github+json")
        .call()
        .with_context(|| format!("fetching {url}"))?;
    response
        .body_mut()
        .read_to_string()
        .with_context(|| format!("reading {url}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revs_come_from_the_channel_and_the_github_api() {
        let hm = "1".repeat(40);
        let np = "2".repeat(40);
        let fetch = |url: &str| -> Result<String> {
            Ok(match url {
                HOME_MANAGER_URL => format!("{{\"sha\": \"{hm}\", \"commit\": {{}}}}"),
                NIXPKGS_URL => format!("{np}\n"),
                _ => bail!("unexpected {url}"),
            })
        };
        assert_eq!(
            resolve(&fetch).unwrap(),
            HmLock {
                home_manager: hm.clone(),
                nixpkgs: np
            }
        );
        let bad = |url: &str| -> Result<String> {
            Ok(match url {
                HOME_MANAGER_URL => "<html>rate limited</html>".into(),
                _ => hm.clone(),
            })
        };
        assert!(resolve(&bad).unwrap_err().to_string().contains("not a commit"));
    }
}
