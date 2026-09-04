//! The three requests the updater makes.
//!
//! All of them are plain HTTPS GETs to GitHub, all of them are blocking, and
//! all of them run on a worker thread rather than the one drawing the window.
//! There is no authentication: the repository is public, and a token would be
//! a secret on disk, which this client does not do.
//!
//! # What GitHub sees
//!
//! An address, and the `User-Agent` below -- which names the release this
//! build came from, because the API requires a user agent and a truthful one
//! is more useful than a vague one. Nothing else is sent: no identifier, no
//! saved connections, no telemetry of any kind. A user who would rather not
//! make the request at all turns the automatic check off, and then nothing
//! here runs unless they ask for it by hand.

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use ureq::Agent;

/// Long enough for a slow link to finish a large asset, short enough that a
/// black-holed connection does not leave a thread parked forever.
const TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// The listing and the checksums are small; anything claiming otherwise is
/// not what we asked for.
const JSON_LIMIT: u64 = 4 * 1024 * 1024;
const SUMS_LIMIT: u64 = 1024 * 1024;

/// A ceiling on a download, well clear of the ~20 MB a release actually is.
const ASSET_LIMIT: u64 = 512 * 1024 * 1024;

/// How much to move between progress reports.
const CHUNK: usize = 64 * 1024;

/// Truthful, and required: GitHub rejects an API request with no user agent.
fn user_agent() -> String {
    format!(
        "lynxrdp/{} (+{})",
        super::current_label(),
        env!("CARGO_PKG_REPOSITORY")
    )
}

fn agent() -> Agent {
    Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .user_agent(user_agent())
        .build()
        .into()
}

/// The newest `count` releases of `repo`, as JSON.
///
/// `/releases` rather than `/releases/latest`, which ignores prereleases
/// entirely -- and every release of this project so far is one, so `latest`
/// would answer "there are none" forever.
pub fn releases(repo: &str, count: usize) -> Result<String> {
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page={count}");
    let mut response = agent()
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        // Pinned so a future default cannot change the shape of the answer
        // under a client that is already deployed.
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .with_context(|| format!("asking {url}"))?;
    response
        .body_mut()
        .with_config()
        .limit(JSON_LIMIT)
        .read_to_string()
        .context("reading the release listing")
}

/// Where a release publishes its checksums.
///
/// Built from the tag rather than taken from the listing, because
/// `SHA256SUMS` is the one asset whose name never varies and the download URL
/// for a public release is a stable, documented shape.
pub fn sums_url(repo: &str, tag: &str) -> String {
    format!("https://github.com/{repo}/releases/download/{tag}/SHA256SUMS")
}

/// Fetch a small text file.
pub fn text(url: &str) -> Result<String> {
    let mut response = agent()
        .get(url)
        .call()
        .with_context(|| format!("fetching {url}"))?;
    response
        .body_mut()
        .with_config()
        .limit(SUMS_LIMIT)
        .read_to_string()
        .with_context(|| format!("reading {url}"))
}

/// Stream an asset to `dest`, reporting progress as it goes.
///
/// The total comes from the release listing where it can, because a
/// `Content-Length` is absent on a chunked or transparently decompressed
/// response -- and a progress bar that cannot say how long it will be is
/// still better than a window that looks hung.
pub fn download(url: &str, dest: &Path, progress: &dyn Fn(u64, Option<u64>)) -> Result<()> {
    let mut response = agent()
        .get(url)
        .call()
        .with_context(|| format!("downloading {url}"))?;
    let total = response.body().content_length();
    let mut reader = response
        .body_mut()
        .with_config()
        .limit(ASSET_LIMIT)
        .reader();
    let mut file =
        std::fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut buf = vec![0u8; CHUNK];
    let mut done: u64 = 0;
    loop {
        let n = reader.read(&mut buf).context("reading the download")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .with_context(|| format!("writing {}", dest.display()))?;
        done += n as u64;
        progress(done, total);
    }
    // Not for durability -- the file is consumed in a moment -- but so that a
    // reader that opens it by name sees every byte this thread wrote.
    file.flush().context("flushing the download")?;
    if done == 0 {
        bail!("the download was empty");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_agent_says_what_it_is_without_saying_who() {
        let ua = user_agent();
        assert!(ua.starts_with("lynxrdp/"));
        assert!(ua.contains("github.com/guitar24t/lynxrdp"));
        // The only two things in it are the build and the project. Anything
        // resembling an identifier would make an update check a beacon.
        assert!(!ua.contains('@'));
    }

    #[test]
    fn the_checksums_come_from_the_release_being_offered() {
        // Not from "latest": a user installing an older release must be
        // checked against that release's own sums.
        assert_eq!(
            sums_url("guitar24t/lynxrdp", "v0.1.0-rc.6"),
            "https://github.com/guitar24t/lynxrdp/releases/download/v0.1.0-rc.6/SHA256SUMS"
        );
    }
}
