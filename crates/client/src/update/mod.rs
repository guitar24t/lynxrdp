//! Finding a newer client on the releases page, and becoming it.
//!
//! The client checks `api.github.com` for releases of this repository, and
//! can replace the copy of itself that is running with the one it finds. The
//! server does not do this and must not: it runs as root, it is installed
//! from a `.deb` or `.rpm`, and a root daemon that downloads its own
//! replacement is a much larger thing to trust than a desktop application
//! that swaps a file in the user's own directory. `apt` and `dnf` already
//! own that path.
//!
//! # What this trusts
//!
//! TLS to `api.github.com` and `objects.githubusercontent.com`, and the
//! `SHA256SUMS` file published beside the assets in the same release. The
//! checksum is what catches a truncated or corrupted download; it is *not* a
//! second opinion about who published the release, because it arrives from
//! the same place over the same connection. There is no code signature on
//! any of this -- see "The installers are not signed" in the README -- so the
//! honest summary is: this trusts GitHub, and nothing else is checked.
//! [`verify`] is deliberately the only door the downloaded bytes come
//! through, so a signature check has one place to go if the project ever has
//! a key to check against.
//!
//! # Why so much of this is a pure function
//!
//! Nothing above [`worker`] does I/O. Which release to take, which asset
//! within it, whether this build may replace itself at all and what the swap
//! should be -- all of it is decided by functions that take their inputs as
//! arguments, because the interesting cases (a package-managed install, an
//! Intel Mac with no download, a working copy built by `cargo build`) cannot
//! be produced on the machine running the tests.
//!
//! # The version the client thinks it is
//!
//! `CARGO_PKG_VERSION` is *not* it. The workspace version stays at `0.1.0`
//! through every release candidate, so a build from `v0.1.0-rc.5` would read
//! its own version as `0.1.0`, compare that against `0.1.0-rc.6`, and
//! conclude by semver's own rules that the newer release was older than
//! itself. The release workflow puts the tag in `LYNXRDP_RELEASE_TAG`
//! instead, `build.rs` bakes it in, and a build without one is not a release
//! build and does not replace itself.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use crossbeam_channel::{Receiver, TryRecvError};
use semver::Version;
use serde::Deserialize;

pub mod fetch;
pub mod install;

use install::Plan;

/// How long between automatic checks.
///
/// GitHub allows sixty unauthenticated requests an hour per address, and a
/// check spends one. A day is far below that even for an office behind one
/// NAT, and it is also about how often a release is worth hearing about.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The most releases to ask for.
///
/// Only the newest matters, but "newest" is by version and GitHub orders by
/// creation date, which are not the same thing once a patch to an older
/// series is published. Thirty is one page and covers any realistic gap.
const RELEASE_PAGE: usize = 30;

/// The tag this build was made from, or `None` when it was not made by the
/// release workflow.
///
/// A working copy, a distribution's own build, anything built by hand: all of
/// them land here, and all of them are refused a self-update. That is the
/// intended answer rather than a limitation -- replacing a developer's
/// `cargo build` output with a download would throw away the thing they were
/// working on.
pub fn current_tag() -> Option<&'static str> {
    let tag = env!("LYNXRDP_RELEASE_TAG");
    (!tag.is_empty()).then_some(tag)
}

/// The version this build is, for comparison against a release.
///
/// A build with no tag is `0.0.0-dev`, which is below every real release. It
/// is deliberately not `None`: a developer asking "what is the latest
/// release" should get an answer, and the reason they cannot install it is
/// [`Blocker::NotARelease`], reported next to the version rather than
/// instead of it.
pub fn current_version() -> Version {
    current_tag()
        .and_then(parse_tag)
        .unwrap_or_else(|| Version::parse("0.0.0-dev").expect("a literal semver"))
}

/// What to show a user who asks what they are running.
pub fn current_label() -> String {
    match current_tag() {
        Some(tag) => tag.to_string(),
        None => format!("{} (not a release build)", env!("CARGO_PKG_VERSION")),
    }
}

/// `v0.1.0-rc.5` -> `0.1.0-rc.5`. Anything unparseable is `None` and is
/// skipped rather than guessed at.
pub fn parse_tag(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

/// `owner/name` for the GitHub API, from the repository in `Cargo.toml`.
///
/// Derived rather than written down a second time: the Help menu already
/// opens `CARGO_PKG_REPOSITORY`, and an updater pointed at a different
/// repository than the documentation link is a bug nobody would look for.
pub fn repo_slug(url: &str) -> Option<String> {
    let rest = url
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let (owner, name) = rest.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

/// This repository, as the updater will ask for it.
pub fn repo() -> Option<String> {
    repo_slug(env!("CARGO_PKG_REPOSITORY"))
}

// ------------------------------------------------------------- the release

/// One release, as much of GitHub's JSON as is used.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub published_at: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

/// One downloadable file in a release.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

/// Parse the release listing.
pub fn parse_releases(json: &str) -> Result<Vec<Release>> {
    serde_json::from_str(json).context("reading the release listing")
}

/// Which platform's downloads this build wants, named the way the release
/// workflow names them.
///
/// Kept as a function of two strings so the mapping can be tested for every
/// platform from any one of them. `None` means the release has nothing for
/// this machine -- an Intel Mac, a 32-bit host, a BSD -- which is a thing to
/// say plainly rather than a failure to download.
pub fn platform_key(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        ("macos", "aarch64") => Some("macos-aarch64"),
        ("windows", "x86_64") => Some("windows-x86_64"),
        _ => None,
    }
}

/// This machine's platform key.
pub fn platform() -> Option<&'static str> {
    platform_key(std::env::consts::OS, std::env::consts::ARCH)
}

/// Which of a release's files to take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavour {
    /// The plain archive: a `.tar.gz` holding the binary or the application
    /// bundle, or a `.zip` on Windows.
    Archive,
    /// The Windows installer, which is the only one of these that can ask
    /// for administrator rights.
    Installer,
}

/// The end of the asset name to look for.
///
/// Matched by suffix rather than built whole, because the file name carries
/// the *Cargo* version (`lynxrdp-0.1.0-linux-x86_64.tar.gz`) and the release
/// is tagged with something else. Reconstructing the name would mean
/// deciding which of the two versions goes in it, and being wrong on every
/// release candidate.
pub fn asset_suffix(platform: &str, flavour: Flavour) -> String {
    match flavour {
        Flavour::Installer => format!("-{platform}-setup.exe"),
        Flavour::Archive if platform.starts_with("windows-") => format!("-{platform}.zip"),
        Flavour::Archive => format!("-{platform}.tar.gz"),
    }
}

/// The asset for this platform, if the release has one.
pub fn asset_for<'a>(release: &'a Release, platform: &str, flavour: Flavour) -> Option<&'a Asset> {
    let suffix = asset_suffix(platform, flavour);
    release
        .assets
        .iter()
        .find(|a| a.name.starts_with("lynxrdp-") && a.name.ends_with(&suffix))
}

/// The newest release worth offering, or `None` when there is not one.
///
/// Draft releases are never offered: they are not published, and their assets
/// may not exist yet. A prerelease is offered only when asked for -- see
/// [`wants_prereleases`] for how that question answers itself.
pub fn pick<'a>(
    releases: &'a [Release],
    current: &Version,
    prereleases: bool,
    platform: &str,
) -> Option<&'a Release> {
    releases
        .iter()
        .filter(|r| !r.draft)
        .filter(|r| prereleases || !r.prerelease)
        // A release with nothing for this platform is not an update; offering
        // it would produce a button that can only fail.
        .filter(|r| asset_for(r, platform, Flavour::Archive).is_some())
        .filter_map(|r| parse_tag(&r.tag_name).map(|v| (v, r)))
        .filter(|(v, _)| v > current)
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, r)| r)
}

/// Whether to offer prereleases, given what the user asked for and what they
/// are running.
///
/// `None` -- the default -- means "the same kind of build I have now". Every
/// release so far is a release candidate, so a plain `false` would leave a
/// user on `v0.1.0-rc.5` being told forever that they are up to date; and
/// once `v0.1.0` proper exists, someone who installed *it* should not be
/// moved onto the next candidate series without saying so.
pub fn wants_prereleases(setting: Option<bool>, current: &Version) -> bool {
    setting.unwrap_or(!current.pre.is_empty())
}

// ---------------------------------------------------------------- checksum

/// The checksum for `name` from a `sha256sum`-format file.
///
/// The format is `<64 hex>  <name>`, and GNU `sha256sum` writes a `*` before
/// the name for a binary-mode file. Both are accepted; anything else in the
/// file is ignored rather than fatal, because a release could reasonably grow
/// a comment line.
pub fn checksum_for<'a>(sums: &'a str, name: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hex = parts.next()?;
        let file = parts.next()?.trim_start_matches('*');
        (file == name && hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then_some(hex)
    })
}

/// Check a downloaded file against its published checksum.
///
/// The one door the downloaded bytes come through. Everything that installs
/// anything calls this first, and it takes the expected digest as an argument
/// so that the test suite can prove a wrong one is refused without a network.
pub fn verify(file: &Path, expected: &str) -> Result<()> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(file).with_context(|| format!("opening {}", file.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher).with_context(|| format!("reading {}", file.display()))?;
    let got = hasher.finalize();
    let got = got.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    });
    if !got.eq_ignore_ascii_case(expected) {
        bail!(
            "the download does not match its published checksum (expected {expected}, got {got})"
        );
    }
    Ok(())
}

// ------------------------------------------------------------- eligibility

/// Why this copy of the client cannot replace itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blocker {
    /// Not built by the release workflow, so there is nothing to compare and
    /// nothing that should be overwritten.
    NotARelease,
    /// Installed from a `.deb` or `.rpm`. The package manager owns these
    /// files; writing over them leaves the system's own database describing
    /// a file that is no longer there.
    PackageManaged(PathBuf),
    /// No download is published for this machine.
    NoDownload,
    /// The application is somewhere this user cannot write, and on this
    /// platform there is no installer to hand the job to.
    ReadOnly(PathBuf),
}

impl Blocker {
    /// One sentence, for the window.
    pub fn explain(&self) -> String {
        match self {
            Self::NotARelease => "This build did not come from the releases page, so it will not \
                 replace itself. Download a release, or build the new version yourself."
                .into(),
            Self::PackageManaged(path) => format!(
                "{} was installed by a package manager, which owns that file. Update it with \
                 apt or dnf instead.",
                path.display()
            ),
            Self::NoDownload => format!(
                "There is no published download for {} on {}. Building from source still works.",
                std::env::consts::ARCH,
                std::env::consts::OS
            ),
            Self::ReadOnly(path) => format!(
                "{} cannot be written by this user. Reinstall over it, or run the update as \
                 someone who can.",
                path.display()
            ),
        }
    }
}

/// Whether a path is one a package manager laid down.
///
/// `/usr/bin` and `/usr/sbin` only. `/usr/local` is where a hand-installed
/// binary belongs and is deliberately *not* on the list, and neither is
/// `~/.local/bin`: both are places a user put something themselves, and both
/// should update themselves like any other copy. The check is by path rather
/// than by asking dpkg or rpm because it must also be right on a system that
/// has neither.
pub fn is_package_managed(exe: &Path) -> bool {
    let s = exe.to_string_lossy().replace('\\', "/");
    ["/usr/bin/", "/usr/sbin/", "/bin/", "/sbin/"]
        .iter()
        .any(|p| s.starts_with(p))
}

/// The `.app` bundle a macOS executable is inside, if it is inside one.
///
/// `…/LynxRDP.app/Contents/MacOS/lynxrdp` -> `…/LynxRDP.app`. The whole
/// bundle is what gets replaced, not the executable inside it, because
/// `Info.plist` carries the version and the icon lives beside it; swapping
/// only the binary would leave Finder describing the release before last.
pub fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    if macos.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    let app = contents.parent()?;
    app.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("app"))
        .then(|| app.to_path_buf())
}

/// What replacing this build would mean, or why it cannot be done.
///
/// Only about *this installation*: where the application lives, and whether
/// it can be written. Whether a download exists for the machine at all is
/// [`Blocker::NoDownload`], and that is settled by [`platform`] before a
/// release is ever chosen -- asking it here would mean mixing the caller's
/// operating system with the running machine's architecture, which is
/// exactly the confusion the two-argument [`platform_key`] avoids.
///
/// `writable` is passed in rather than tested here so the decision can be
/// exercised for every platform from one of them: the caller answers it by
/// actually creating a file (see [`install::can_write`]), because permission
/// bits describe intentions and a read-only mount or an ACL describes what
/// will happen.
pub fn plan_for(exe: &Path, os: &str, tagged: bool, writable: bool) -> Result<Plan, Blocker> {
    if !tagged {
        return Err(Blocker::NotARelease);
    }
    if os != "windows" && is_package_managed(exe) {
        return Err(Blocker::PackageManaged(exe.to_path_buf()));
    }
    match os {
        "macos" => match bundle_of(exe) {
            // The bundle's *parent* is what has to be writable: the swap
            // renames the whole application into place beside itself.
            Some(bundle) if writable => Ok(Plan::Bundle { bundle }),
            Some(bundle) => Err(Blocker::ReadOnly(bundle)),
            None if writable => Ok(Plan::Binary {
                target: exe.to_path_buf(),
            }),
            None => Err(Blocker::ReadOnly(exe.to_path_buf())),
        },
        "windows" if writable => Ok(Plan::WindowsExe {
            target: exe.to_path_buf(),
        }),
        // Program Files, which is where the installer puts it. We cannot
        // elevate, but the installer's own manifest asks for administrator,
        // so handing the job to it is the whole answer -- and it keeps the
        // uninstall entry and the Start Menu shortcut correct, which a file
        // swap would not.
        "windows" => Ok(Plan::WindowsInstaller),
        _ if writable => Ok(Plan::Binary {
            target: exe.to_path_buf(),
        }),
        _ => Err(Blocker::ReadOnly(exe.to_path_buf())),
    }
}

/// Which asset a plan needs.
pub fn flavour_for(plan: &Plan) -> Flavour {
    match plan {
        Plan::WindowsInstaller => Flavour::Installer,
        _ => Flavour::Archive,
    }
}

// ------------------------------------------------------------------ timing

/// Seconds since the epoch, for the last-checked stamp.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether an automatic check is due.
///
/// A stamp in the future is treated as due rather than as a very long wait:
/// a clock that has been corrected backwards, or a settings file copied from
/// another machine, should not switch checking off until the date catches up.
pub fn due(last: Option<u64>, now: u64) -> bool {
    match last {
        None => true,
        Some(then) if then > now => true,
        Some(then) => now - then >= CHECK_INTERVAL.as_secs(),
    }
}

// ------------------------------------------------------------------- state

/// A release that is newer than this build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub tag: String,
    pub version: Version,
    pub notes_url: String,
    pub published: String,
    pub asset: Asset,
    /// Why it cannot be installed here, if it cannot. Carried alongside the
    /// release rather than replacing it: "there is a new version, and here is
    /// why this copy cannot take it" is more useful than either half.
    pub blocker: Option<Blocker>,
}

impl Found {
    /// Whether the Install button does anything.
    pub fn installable(&self) -> bool {
        self.blocker.is_none()
    }
}

/// What the updater is doing.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum State {
    #[default]
    Idle,
    Checking,
    /// Checked, and this is the newest there is.
    UpToDate,
    Found(Box<Found>),
    Downloading {
        done: u64,
        total: Option<u64>,
    },
    /// In place. The running process is still the old one.
    Installed,
    /// The Windows installer has been started and is waiting for the user;
    /// this process should get out of its way.
    HandedOff,
    Failed(String),
}

/// What a worker thread reports back.
#[derive(Debug)]
enum Event {
    Checked(Result<Option<Box<Found>>, String>),
    Progress { done: u64, total: Option<u64> },
    Done(Result<Outcome, String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Installed,
    HandedOff,
}

/// The updater, as the window sees it.
///
/// One of these lives in the launcher. It owns no window and no egui types:
/// everything it does is start a thread, collect what the thread says, and
/// hold the answer until something asks.
#[derive(Debug, Default)]
pub struct Updater {
    state: State,
    rx: Option<Receiver<Event>>,
    /// The user closed the notice for this run. Not persisted: a dismissal
    /// is about this window, and the next start should mention it again.
    dismissed: bool,
}

impl Updater {
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Whether a thread is out there.
    pub fn busy(&self) -> bool {
        matches!(self.state, State::Checking | State::Downloading { .. })
    }

    /// Whether the window should keep repainting to follow progress.
    pub fn animating(&self) -> bool {
        matches!(self.state, State::Downloading { .. })
    }

    /// The release found, if one was.
    pub fn found(&self) -> Option<&Found> {
        match &self.state {
            State::Found(f) => Some(f),
            _ => None,
        }
    }

    /// Whether to show the notice above the list.
    pub fn announcing(&self) -> bool {
        !self.dismissed && self.found().is_some()
    }

    pub fn dismiss(&mut self) {
        self.dismissed = true;
    }

    /// Ask GitHub what the newest release is.
    ///
    /// Does nothing while a thread is already running, so a user leaning on
    /// the menu item cannot start five requests into a rate limit.
    pub fn check(&mut self, prereleases: Option<bool>) {
        if self.busy() {
            return;
        }
        self.dismissed = false;
        self.state = State::Checking;
        let (tx, rx) = crossbeam_channel::bounded(64);
        self.rx = Some(rx);
        std::thread::Builder::new()
            .name("lynxrdp-update-check".into())
            .spawn(move || {
                let found = worker::check(prereleases).map_err(|e| format!("{e:#}"));
                let _ = tx.send(Event::Checked(found.map(|f| f.map(Box::new))));
            })
            .map_err(|e| self.state = State::Failed(format!("could not start the check: {e}")))
            .ok();
    }

    /// Download the release we found and put it in place.
    pub fn install(&mut self) {
        let Some(found) = self.found().cloned() else {
            return;
        };
        if !found.installable() || self.busy() {
            return;
        }
        self.state = State::Downloading {
            done: 0,
            total: Some(found.asset.size).filter(|s| *s > 0),
        };
        let (tx, rx) = crossbeam_channel::bounded(64);
        self.rx = Some(rx);
        let progress = tx.clone();
        std::thread::Builder::new()
            .name("lynxrdp-update-install".into())
            .spawn(move || {
                let outcome = worker::install(&found, &|done, total| {
                    let _ = progress.try_send(Event::Progress { done, total });
                })
                .map_err(|e| format!("{e:#}"));
                let _ = tx.send(Event::Done(outcome));
            })
            .map_err(|e| self.state = State::Failed(format!("could not start the download: {e}")))
            .ok();
    }

    /// Put the updater where a successful check would have left it.
    ///
    /// Test-only, and the reason it exists is worth saying: the launcher's
    /// window tests draw whole frames, and a frame that contains an update
    /// notice has to be reachable without a network.
    #[cfg(test)]
    pub fn offer_for_test(&mut self, found: Found) {
        self.apply(Event::Checked(Ok(Some(Box::new(found)))));
    }

    /// Collect whatever the thread has said. Returns true when something
    /// changed, which the caller turns into a repaint.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        loop {
            let Some(rx) = &self.rx else { return changed };
            match rx.try_recv() {
                Ok(event) => {
                    self.apply(event);
                    changed = true;
                }
                Err(TryRecvError::Empty) => return changed,
                Err(TryRecvError::Disconnected) => {
                    self.rx = None;
                    // A thread that ended without saying anything left us
                    // mid-sentence; saying so beats a spinner that never
                    // stops.
                    if self.busy() {
                        self.state = State::Failed("the update thread stopped unexpectedly".into());
                        changed = true;
                    }
                    return changed;
                }
            }
        }
    }

    fn apply(&mut self, event: Event) {
        self.state = match event {
            Event::Checked(Ok(Some(found))) => State::Found(found),
            Event::Checked(Ok(None)) => State::UpToDate,
            Event::Checked(Err(e)) => State::Failed(e),
            // A late progress message after the outcome has landed must not
            // put the window back into a download that has already finished.
            Event::Progress { done, total } => match self.state {
                State::Downloading { .. } => State::Downloading { done, total },
                _ => return,
            },
            Event::Done(Ok(Outcome::Installed)) => State::Installed,
            Event::Done(Ok(Outcome::HandedOff)) => State::HandedOff,
            Event::Done(Err(e)) => State::Failed(e),
        };
    }
}

/// The threaded half: everything here does I/O.
mod worker {
    use super::*;

    /// One check: ask for the releases, pick one, work out whether it could
    /// be installed here.
    pub fn check(prereleases: Option<bool>) -> Result<Option<Found>> {
        let repo = repo().context("this build has no GitHub repository to ask about")?;
        let platform = platform();
        let current = current_version();
        let json = fetch::releases(&repo, RELEASE_PAGE)?;
        let releases = parse_releases(&json)?;
        // With no platform key there is no asset to look for, so there is
        // nothing to offer even when a newer release exists. Said as a
        // blocker on the newest release we can name, rather than as silence.
        let Some(platform) = platform else {
            let newest = releases
                .iter()
                .filter(|r| !r.draft)
                .filter_map(|r| parse_tag(&r.tag_name).map(|v| (v, r)))
                .filter(|(v, _)| *v > current)
                .max_by(|(a, _), (b, _)| a.cmp(b));
            return Ok(newest.map(|(version, r)| Found {
                tag: r.tag_name.clone(),
                version,
                notes_url: r.html_url.clone(),
                published: r.published_at.clone(),
                asset: Asset {
                    name: String::new(),
                    browser_download_url: String::new(),
                    size: 0,
                },
                blocker: Some(Blocker::NoDownload),
            }));
        };
        let allow_pre = wants_prereleases(prereleases, &current);
        let Some(release) = pick(&releases, &current, allow_pre, platform) else {
            return Ok(None);
        };
        let version = parse_tag(&release.tag_name).context("the chosen release has no version")?;
        let exe = std::env::current_exe().context("finding this executable")?;
        let plan = plan_for(
            &exe,
            std::env::consts::OS,
            current_tag().is_some(),
            install::can_write(&exe),
        );
        // The asset follows the plan: a Windows install in Program Files
        // needs the installer, everything else the plain archive.
        let flavour = plan.as_ref().map(flavour_for).unwrap_or(Flavour::Archive);
        let asset = asset_for(release, platform, flavour)
            .or_else(|| asset_for(release, platform, Flavour::Archive))
            .context("the release has no download for this platform")?;
        Ok(Some(Found {
            tag: release.tag_name.clone(),
            version,
            notes_url: release.html_url.clone(),
            published: release.published_at.clone(),
            asset: asset.clone(),
            blocker: plan.err(),
        }))
    }

    /// Download, verify, and put in place.
    pub fn install(found: &Found, progress: &dyn Fn(u64, Option<u64>)) -> Result<Outcome> {
        let repo = repo().context("this build has no GitHub repository to ask about")?;
        let exe = std::env::current_exe().context("finding this executable")?;
        let plan = plan_for(
            &exe,
            std::env::consts::OS,
            current_tag().is_some(),
            install::can_write(&exe),
        )
        .map_err(|b| anyhow::anyhow!("{}", b.explain()))?;

        // Somewhere to put the download that is not the directory being
        // replaced: the staging for the swap itself has to be on the target's
        // filesystem, but a half-downloaded archive has no business sitting
        // next to the application.
        let scratch = tempfile::Builder::new()
            .prefix("lynxrdp-update-")
            .tempdir()
            .context("creating a temporary directory for the download")?;
        let archive = scratch.path().join(&found.asset.name);

        // The checksums first. Downloading fifteen megabytes to then discover
        // the release has no SHA256SUMS wastes the user's time and bandwidth,
        // and there is no path here that installs an unverified file.
        let sums = fetch::text(&fetch::sums_url(&repo, &found.tag))
            .context("fetching SHA256SUMS from the release")?;
        let expected = checksum_for(&sums, &found.asset.name)
            .with_context(|| format!("{} is not listed in SHA256SUMS", found.asset.name))?
            .to_string();

        fetch::download(&found.asset.browser_download_url, &archive, progress)?;
        verify(&archive, &expected)?;

        match plan {
            Plan::WindowsInstaller => {
                // The installer has to outlive the temporary directory and
                // this process both, so it moves somewhere that is not swept
                // out from under it.
                let kept = install::keep_installer(&archive, &found.asset.name)?;
                install::run_installer(&kept)?;
                Ok(Outcome::HandedOff)
            }
            plan => {
                install::apply(&plan, &archive)?;
                Ok(Outcome::Installed)
            }
        }
    }
}

/// Start the new build and leave.
///
/// The launcher's own path, re-invoked with no arguments, which is exactly
/// how it was started -- and after an in-place update that path is the new
/// build. Running sessions are separate processes and are left alone, which
/// is the same promise closing the window already makes.
pub fn restart() -> Result<()> {
    let exe = std::env::current_exe().context("finding this executable")?;
    // The executable rather than the bundle, on macOS as everywhere else:
    // starting `LynxRDP.app/Contents/MacOS/lynxrdp` *is* starting the
    // application -- macOS reads the Info.plist beside it -- and it does not
    // need `open`, which would go through Launch Services and hand us a
    // failure we could not explain.
    std::process::Command::new(&exe)
        .spawn()
        .with_context(|| format!("restarting {}", exe.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, prerelease: bool, assets: &[&str]) -> Release {
        Release {
            tag_name: tag.into(),
            draft: false,
            prerelease,
            html_url: format!("https://github.com/guitar24t/lynxrdp/releases/tag/{tag}"),
            published_at: "2026-09-04T19:28:06Z".into(),
            assets: assets
                .iter()
                .map(|name| Asset {
                    name: (*name).into(),
                    browser_download_url: format!("https://example.invalid/{name}"),
                    size: 1024,
                })
                .collect(),
        }
    }

    /// The asset names a real release carries, which is what the matching has
    /// to pick out of.
    fn every_asset() -> Vec<&'static str> {
        vec![
            "lynxrdp-0.1.0-linux-aarch64.tar.gz",
            "lynxrdp-0.1.0-linux-x86_64.tar.gz",
            "lynxrdp-0.1.0-macos-aarch64.dmg",
            "lynxrdp-0.1.0-macos-aarch64.tar.gz",
            "lynxrdp-0.1.0-windows-x86_64-setup.exe",
            "lynxrdp-0.1.0-windows-x86_64.zip",
            "lynxrdp-client-0.1.0-1.aarch64.rpm",
            "lynxrdp-client-0.1.0-1.x86_64.rpm",
            "lynxrdp-client_0.1.0-1_amd64.deb",
            "lynxrdp-client_0.1.0-1_arm64.deb",
            "lynxrdp-server-0.1.0-1.x86_64.rpm",
            "lynxrdp-server_0.1.0-1_amd64.deb",
            "SHA256SUMS",
        ]
    }

    #[test]
    fn a_release_candidate_sorts_below_the_release_it_leads_to() {
        // The reason the tag is baked in at all: by Cargo's version alone
        // every one of these is 0.1.0, and rc.6 would look like a downgrade
        // from the build that shipped as rc.5.
        let v = |s: &str| parse_tag(s).unwrap();
        assert!(v("v0.1.0-rc.5") < v("v0.1.0-rc.6"));
        assert!(v("v0.1.0-rc.9") < v("v0.1.0-rc.10"));
        assert!(v("v0.1.0-rc.10") < v("v0.1.0"));
        assert!(v("v0.1.0") < v("v0.1.1-rc.1"));
        assert_eq!(v("0.1.0"), v("v0.1.0"), "the v is optional");
        assert_eq!(parse_tag("nightly"), None);
    }

    #[test]
    fn the_newest_release_wins_even_when_it_is_not_the_newest_entry() {
        // GitHub orders by creation date. A patch to an older series
        // published after a newer one would otherwise be offered as an
        // upgrade to everybody on it.
        let releases = vec![
            release("v0.1.2", false, &every_asset()),
            release("v0.2.0", false, &every_asset()),
            release("v0.1.3", false, &every_asset()),
        ];
        let current = parse_tag("v0.1.0").unwrap();
        let picked = pick(&releases, &current, false, "linux-x86_64").unwrap();
        assert_eq!(picked.tag_name, "v0.2.0");
    }

    #[test]
    fn a_prerelease_is_offered_only_when_it_is_wanted() {
        let releases = vec![
            release("v0.1.0", false, &every_asset()),
            release("v0.2.0-rc.1", true, &every_asset()),
        ];
        let current = parse_tag("v0.0.9").unwrap();
        assert_eq!(
            pick(&releases, &current, false, "linux-x86_64")
                .unwrap()
                .tag_name,
            "v0.1.0"
        );
        assert_eq!(
            pick(&releases, &current, true, "linux-x86_64")
                .unwrap()
                .tag_name,
            "v0.2.0-rc.1"
        );
    }

    #[test]
    fn someone_running_a_candidate_is_offered_candidates_by_default() {
        // Every release so far is a candidate, so a flat "stable only"
        // default would tell every existing user they were up to date
        // forever. Once v0.1.0 proper exists, its users stop being moved
        // onto the next candidate series unless they ask.
        let rc = parse_tag("v0.1.0-rc.5").unwrap();
        let stable = parse_tag("v0.1.0").unwrap();
        assert!(wants_prereleases(None, &rc));
        assert!(!wants_prereleases(None, &stable));
        // An explicit choice is obeyed in both directions.
        assert!(!wants_prereleases(Some(false), &rc));
        assert!(wants_prereleases(Some(true), &stable));
    }

    #[test]
    fn a_draft_is_never_offered() {
        // Its assets may not exist yet, and it is not published.
        let mut draft = release("v9.9.9", false, &every_asset());
        draft.draft = true;
        let current = parse_tag("v0.1.0").unwrap();
        assert!(pick(&[draft], &current, true, "linux-x86_64").is_none());
    }

    #[test]
    fn the_same_version_is_not_an_update() {
        let releases = vec![release("v0.1.0-rc.5", true, &every_asset())];
        let current = parse_tag("v0.1.0-rc.5").unwrap();
        assert!(pick(&releases, &current, true, "linux-x86_64").is_none());
    }

    #[test]
    fn a_release_with_nothing_for_this_platform_is_not_an_update() {
        // Offering it would produce an Install button whose only possible
        // outcome is a failure to find the download.
        let releases = vec![release(
            "v0.2.0",
            false,
            &["lynxrdp-0.2.0-linux-x86_64.tar.gz"],
        )];
        let current = parse_tag("v0.1.0").unwrap();
        assert!(pick(&releases, &current, false, "macos-aarch64").is_none());
        assert!(pick(&releases, &current, false, "linux-x86_64").is_some());
    }

    #[test]
    fn each_platform_takes_its_own_file_out_of_a_real_release() {
        let r = release("v0.1.0", false, &every_asset());
        let archive = |p: &str| asset_for(&r, p, Flavour::Archive).map(|a| a.name.as_str());
        assert_eq!(
            archive("linux-x86_64"),
            Some("lynxrdp-0.1.0-linux-x86_64.tar.gz")
        );
        assert_eq!(
            archive("linux-aarch64"),
            Some("lynxrdp-0.1.0-linux-aarch64.tar.gz")
        );
        // The .dmg is a disk image a human mounts; the updater wants the
        // archive it can unpack without asking the window server.
        assert_eq!(
            archive("macos-aarch64"),
            Some("lynxrdp-0.1.0-macos-aarch64.tar.gz")
        );
        assert_eq!(
            archive("windows-x86_64"),
            Some("lynxrdp-0.1.0-windows-x86_64.zip")
        );
        assert_eq!(
            asset_for(&r, "windows-x86_64", Flavour::Installer).map(|a| a.name.as_str()),
            Some("lynxrdp-0.1.0-windows-x86_64-setup.exe")
        );
    }

    #[test]
    fn the_client_package_is_never_mistaken_for_the_archive() {
        // lynxrdp-client_0.1.0-1_amd64.deb starts with "lynxrdp-" too, and a
        // looser match would hand a .deb to the tar reader.
        let r = release("v0.1.0", false, &every_asset());
        for platform in ["linux-x86_64", "linux-aarch64"] {
            let name = &asset_for(&r, platform, Flavour::Archive).unwrap().name;
            assert!(name.ends_with(".tar.gz"), "{name}");
        }
    }

    #[test]
    fn every_platform_the_release_workflow_builds_has_a_key() {
        // The names come from the matrix in release.yml; if one is renamed
        // there, the asset for it stops being found here.
        assert_eq!(platform_key("linux", "x86_64"), Some("linux-x86_64"));
        assert_eq!(platform_key("linux", "aarch64"), Some("linux-aarch64"));
        assert_eq!(platform_key("macos", "aarch64"), Some("macos-aarch64"));
        assert_eq!(platform_key("windows", "x86_64"), Some("windows-x86_64"));
        // No build is published for these, and saying so is better than
        // downloading something that will not run.
        assert_eq!(platform_key("macos", "x86_64"), None);
        assert_eq!(platform_key("linux", "arm"), None);
        assert_eq!(platform_key("freebsd", "x86_64"), None);
    }

    #[test]
    fn the_repository_is_read_from_the_manifest_rather_than_written_twice() {
        assert_eq!(
            repo_slug("https://github.com/guitar24t/lynxrdp"),
            Some("guitar24t/lynxrdp".into())
        );
        assert_eq!(
            repo_slug("https://github.com/guitar24t/lynxrdp.git"),
            Some("guitar24t/lynxrdp".into())
        );
        assert_eq!(
            repo_slug("https://github.com/guitar24t/lynxrdp/"),
            Some("guitar24t/lynxrdp".into())
        );
        assert_eq!(repo_slug("https://gitlab.com/a/b"), None);
        assert_eq!(repo_slug("https://github.com/onlyowner"), None);
        // The real one, so a manifest change that breaks this is caught here.
        assert_eq!(repo(), Some("guitar24t/lynxrdp".into()));
    }

    #[test]
    fn a_checksum_is_found_by_name_and_only_by_name() {
        let sums = "\
0000000000000000000000000000000000000000000000000000000000000001  lynxrdp-0.1.0-linux-x86_64.tar.gz
0000000000000000000000000000000000000000000000000000000000000002 *lynxrdp-0.1.0-windows-x86_64.zip
not a checksum line
0000000000000000000000000000000000000000000000000000000000000003  SHA256SUMS
";
        assert_eq!(
            checksum_for(sums, "lynxrdp-0.1.0-linux-x86_64.tar.gz"),
            Some("0000000000000000000000000000000000000000000000000000000000000001")
        );
        // GNU sha256sum marks a binary-mode file with a star.
        assert_eq!(
            checksum_for(sums, "lynxrdp-0.1.0-windows-x86_64.zip"),
            Some("0000000000000000000000000000000000000000000000000000000000000002")
        );
        assert_eq!(
            checksum_for(sums, "lynxrdp-0.1.0-macos-aarch64.tar.gz"),
            None
        );
        // A partial name must not match: it is the whole point of the check.
        assert_eq!(checksum_for(sums, "lynxrdp-0.1.0-linux-x86_64"), None);
    }

    #[test]
    fn a_download_that_does_not_match_its_checksum_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("payload");
        std::fs::write(&file, b"lynxrdp").unwrap();
        // sha256 of "lynxrdp".
        let right = "ac6c1e2b0e1a0e0c5cc7dd5e1e5f4bbfd1a05ee6b1ac5b7b6ef4e6d9b6bd0f7e";
        let actual = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(b"lynxrdp");
            h.finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        assert!(verify(&file, &actual).is_ok());
        assert!(verify(&file, &actual.to_uppercase()).is_ok(), "hex case");
        assert!(verify(&file, right).is_err());
        assert!(verify(&file, "").is_err());
        assert!(verify(&dir.path().join("absent"), &actual).is_err());
    }

    #[test]
    fn a_working_copy_does_not_replace_itself() {
        // The most important refusal here: `cargo build` output is somebody's
        // work in progress, and overwriting it with a download would be
        // destroying it.
        let exe = Path::new("/home/dev/lynxrdp/target/debug/lynxrdp");
        assert_eq!(
            plan_for(exe, "linux", false, true),
            Err(Blocker::NotARelease)
        );
    }

    #[test]
    fn a_packaged_install_is_left_to_the_package_manager() {
        // Writing over /usr/bin/lynxrdp leaves dpkg's database describing a
        // file that is no longer the one it installed, and the next upgrade
        // silently reverts the user.
        let packaged = PathBuf::from("/usr/bin/lynxrdp");
        assert_eq!(
            plan_for(&packaged, "linux", true, true),
            Err(Blocker::PackageManaged(packaged))
        );
        assert!(is_package_managed(Path::new("/usr/bin/lynxrdp")));
        assert!(is_package_managed(Path::new("/bin/lynxrdp")));
        // Places a person put something themselves, which should update like
        // anything else.
        assert!(!is_package_managed(Path::new("/usr/local/bin/lynxrdp")));
        assert!(!is_package_managed(Path::new("/home/u/.local/bin/lynxrdp")));
        assert!(!is_package_managed(Path::new("/opt/lynxrdp/lynxrdp")));
    }

    #[test]
    fn a_macos_application_replaces_the_whole_bundle() {
        let exe = Path::new("/Applications/LynxRDP.app/Contents/MacOS/lynxrdp");
        assert_eq!(
            bundle_of(exe),
            Some(PathBuf::from("/Applications/LynxRDP.app"))
        );
        assert_eq!(
            plan_for(exe, "macos", true, true),
            Ok(Plan::Bundle {
                bundle: PathBuf::from("/Applications/LynxRDP.app")
            })
        );
        // Read-only names the bundle, not the executable inside it: that is
        // what the user has to be able to replace.
        assert_eq!(
            plan_for(exe, "macos", true, false),
            Err(Blocker::ReadOnly(PathBuf::from(
                "/Applications/LynxRDP.app"
            )))
        );
        // A loose binary on macOS is still just a binary.
        let loose = Path::new("/usr/local/bin/lynxrdp");
        assert_eq!(bundle_of(loose), None);
        assert_eq!(
            plan_for(loose, "macos", true, true),
            Ok(Plan::Binary {
                target: loose.to_path_buf()
            })
        );
        // Not every .app-shaped path is a bundle.
        assert_eq!(bundle_of(Path::new("/x/LynxRDP.app/lynxrdp")), None);
        assert_eq!(bundle_of(Path::new("/x/Foo/Contents/MacOS/lynxrdp")), None);
    }

    #[test]
    fn windows_in_program_files_hands_the_job_to_the_installer() {
        // We cannot elevate; the installer's manifest asks for administrator
        // on its own, and it keeps the uninstall entry correct besides.
        let exe = Path::new(r"C:\Program Files\LynxRDP\lynxrdp.exe");
        assert_eq!(
            plan_for(exe, "windows", true, false),
            Ok(Plan::WindowsInstaller)
        );
        assert_eq!(
            flavour_for(&plan_for(exe, "windows", true, false).unwrap()),
            Flavour::Installer
        );
        // A copy the user unzipped somewhere writable swaps itself instead,
        // because running the installer would move it to Program Files
        // behind their back.
        let portable = Path::new(r"C:\Users\u\Downloads\lynxrdp\lynxrdp.exe");
        assert_eq!(
            plan_for(portable, "windows", true, true),
            Ok(Plan::WindowsExe {
                target: portable.to_path_buf()
            })
        );
        assert_eq!(
            flavour_for(&plan_for(portable, "windows", true, true).unwrap()),
            Flavour::Archive
        );
    }

    #[test]
    fn a_linux_binary_the_user_cannot_write_is_refused_rather_than_attempted() {
        let exe = Path::new("/opt/lynxrdp/lynxrdp");
        assert_eq!(
            plan_for(exe, "linux", true, false),
            Err(Blocker::ReadOnly(exe.to_path_buf()))
        );
        assert_eq!(
            plan_for(exe, "linux", true, true),
            Ok(Plan::Binary {
                target: exe.to_path_buf()
            })
        );
    }

    #[test]
    fn every_blocker_says_something_a_user_can_act_on() {
        for blocker in [
            Blocker::NotARelease,
            Blocker::PackageManaged("/usr/bin/lynxrdp".into()),
            Blocker::NoDownload,
            Blocker::ReadOnly("/opt/lynxrdp".into()),
        ] {
            let text = blocker.explain();
            assert!(text.len() > 40, "{blocker:?}: {text}");
            assert!(text.ends_with('.'), "{blocker:?}: {text}");
        }
    }

    #[test]
    fn an_automatic_check_waits_a_day_and_survives_a_clock_change() {
        let day = CHECK_INTERVAL.as_secs();
        assert!(due(None, 1_000_000), "never checked");
        assert!(!due(Some(1_000_000), 1_000_000 + day - 1));
        assert!(due(Some(1_000_000), 1_000_000 + day));
        // A stamp in the future -- a corrected clock, or a settings file
        // copied from another machine -- must not switch checking off until
        // the date catches up with it.
        assert!(due(Some(2_000_000), 1_000_000));
    }

    #[test]
    fn the_release_listing_is_read_the_way_github_writes_it() {
        let json = r#"[
          {
            "tag_name": "v0.1.0-rc.6",
            "draft": false,
            "prerelease": true,
            "html_url": "https://github.com/guitar24t/lynxrdp/releases/tag/v0.1.0-rc.6",
            "published_at": "2026-09-04T19:28:06Z",
            "body": "notes we do not read",
            "assets": [
              {
                "name": "lynxrdp-0.1.0-linux-x86_64.tar.gz",
                "browser_download_url": "https://example.invalid/a.tar.gz",
                "size": 4096,
                "content_type": "application/gzip"
              }
            ]
          }
        ]"#;
        let releases = parse_releases(json).unwrap();
        assert_eq!(releases.len(), 1);
        assert!(releases[0].prerelease);
        assert_eq!(releases[0].assets[0].size, 4096);
        // Fields we do not name must not make the parse fail: GitHub adds
        // them without warning and an updater that stopped working because
        // of one would be worse than useless.
        assert!(parse_releases(r#"[{"tag_name":"v1.0.0","new_field":1}]"#).is_ok());
        assert!(parse_releases("not json").is_err());
    }

    #[test]
    fn the_state_machine_never_goes_backwards_into_a_finished_download() {
        // A progress message can arrive after the outcome: both are sent by
        // the same thread but read on another, and a stale one must not turn
        // "installed" back into a progress bar that never completes.
        let mut u = Updater {
            state: State::Downloading {
                done: 1,
                total: Some(2),
            },
            ..Default::default()
        };
        u.apply(Event::Done(Ok(Outcome::Installed)));
        assert_eq!(*u.state(), State::Installed);
        u.apply(Event::Progress {
            done: 2,
            total: Some(2),
        });
        assert_eq!(*u.state(), State::Installed);
    }

    #[test]
    fn a_dismissed_notice_stays_dismissed_until_the_next_check() {
        let mut u = Updater::default();
        u.apply(Event::Checked(Ok(Some(Box::new(Found {
            tag: "v9.9.9".into(),
            version: parse_tag("v9.9.9").unwrap(),
            notes_url: String::new(),
            published: String::new(),
            asset: Asset {
                name: "lynxrdp-9.9.9-linux-x86_64.tar.gz".into(),
                browser_download_url: String::new(),
                size: 1,
            },
            blocker: None,
        })))));
        assert!(u.announcing());
        u.dismiss();
        assert!(!u.announcing(), "the notice is closed for this run");
        assert!(u.found().is_some(), "but the release is still known");
    }

    #[test]
    fn a_failed_check_is_reported_rather_than_left_spinning() {
        let mut u = Updater {
            state: State::Checking,
            ..Default::default()
        };
        u.apply(Event::Checked(Err("no route to host".into())));
        assert_eq!(*u.state(), State::Failed("no route to host".into()));
        assert!(!u.busy());
    }

    /// The one test here that talks to GitHub.
    ///
    /// Ignored, so neither CI nor `cargo test` reaches the network: a suite
    /// that fails on an aeroplane is a suite people learn to ignore. Run it
    /// by hand -- `cargo test -p lynxrdp-client -- --ignored --nocapture` --
    /// when something about the release listing is in question, because the
    /// shape of that JSON is the one input here that is not ours to fix.
    #[test]
    #[ignore = "talks to api.github.com"]
    fn the_live_release_listing_still_looks_the_way_we_read_it() {
        let repo = repo().expect("a GitHub repository");
        let json = fetch::releases(&repo, RELEASE_PAGE).expect("fetching the releases");
        let releases = parse_releases(&json).expect("parsing the releases");
        assert!(!releases.is_empty(), "the project has published releases");
        for release in &releases {
            assert!(
                parse_tag(&release.tag_name).is_some(),
                "unreadable tag: {}",
                release.tag_name
            );
        }
        // Whatever the newest release is, it has a download for every
        // platform the workflow builds -- which is the assumption the whole
        // updater rests on.
        let newest = releases
            .iter()
            .find(|r| !r.draft)
            .expect("a published release");
        for platform in [
            "linux-x86_64",
            "linux-aarch64",
            "macos-aarch64",
            "windows-x86_64",
        ] {
            let asset = asset_for(newest, platform, Flavour::Archive)
                .unwrap_or_else(|| panic!("{} has nothing for {platform}", newest.tag_name));
            println!("{platform}: {} ({} bytes)", asset.name, asset.size);
        }
        assert!(
            asset_for(newest, "windows-x86_64", Flavour::Installer).is_some(),
            "{} has no Windows installer",
            newest.tag_name
        );
        // And the checksums it will be verified against are published.
        let sums = fetch::text(&fetch::sums_url(&repo, &newest.tag_name)).expect("SHA256SUMS");
        for platform in ["linux-x86_64", "windows-x86_64"] {
            let name = &asset_for(newest, platform, Flavour::Archive).unwrap().name;
            assert!(
                checksum_for(&sums, name).is_some(),
                "{name} is not in SHA256SUMS"
            );
        }
        println!("newest published release: {}", newest.tag_name);
    }

    /// The whole install path, against a real release, on this platform.
    ///
    /// Also ignored, and heavier: it downloads the archive this machine would
    /// actually be offered (a few megabytes), checks it against the published
    /// `SHA256SUMS`, and unpacks it over a stand-in in a temporary directory.
    /// It is the only thing that exercises the download, the checksum and the
    /// platform's own swap together, and it is worth running by hand before
    /// cutting a release that changes any of them.
    #[test]
    #[ignore = "downloads a release archive"]
    fn a_real_release_installs_over_a_stand_in() {
        let repo = repo().expect("a GitHub repository");
        let platform = platform().expect("a platform with published downloads");
        let json = fetch::releases(&repo, RELEASE_PAGE).expect("fetching the releases");
        let releases = parse_releases(&json).expect("parsing the releases");
        let newest = releases
            .iter()
            .find(|r| !r.draft)
            .expect("a published release");
        let asset = asset_for(newest, platform, Flavour::Archive).expect("an archive");

        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join(&asset.name);
        let seen = std::cell::Cell::new(0u64);
        fetch::download(&asset.browser_download_url, &archive, &|done, _| {
            seen.set(done)
        })
        .expect("downloading the asset");
        assert_eq!(
            seen.get(),
            std::fs::metadata(&archive).unwrap().len(),
            "progress must end at the size actually written"
        );

        let sums = fetch::text(&fetch::sums_url(&repo, &newest.tag_name)).expect("SHA256SUMS");
        let expected = checksum_for(&sums, &asset.name).expect("a checksum for the asset");
        verify(&archive, expected).expect("the download matches its checksum");
        assert!(
            verify(&archive, &"0".repeat(64)).is_err(),
            "a wrong checksum must be refused"
        );

        // Over a stand-in rather than over ourselves: the point is the swap,
        // not to replace the test runner's own binary.
        let target = dir.path().join(if cfg!(windows) {
            "lynxrdp.exe"
        } else {
            "lynxrdp"
        });
        std::fs::write(&target, b"the build being replaced").unwrap();
        let plan = if cfg!(windows) {
            install::Plan::WindowsExe {
                target: target.clone(),
            }
        } else {
            install::Plan::Binary {
                target: target.clone(),
            }
        };
        install::apply(&plan, &archive).expect("installing the new build");
        let installed = std::fs::metadata(&target).unwrap().len();
        assert!(
            installed > 1024 * 1024,
            "an executable, not a stub: {installed} bytes"
        );
        println!(
            "{}: {} -> {} ({installed} bytes in place)",
            newest.tag_name,
            asset.name,
            target.display()
        );
    }

    #[test]
    fn this_build_knows_what_it_is() {
        // Whatever built this: either a tag from the release workflow, or
        // nothing at all. What must never happen is a build that thinks it
        // is a release when it is not.
        match current_tag() {
            Some(tag) => {
                assert!(parse_tag(tag).is_some(), "the tag must be a version: {tag}");
                assert_eq!(current_label(), tag);
            }
            None => {
                assert!(current_label().contains("not a release build"));
                assert_eq!(current_version(), parse_tag("0.0.0-dev").unwrap());
            }
        }
    }
}
