//! `mana upgrade`, and the one line `mana launch` prints when a newer release
//! exists.
//!
//! Both halves talk to the same GitHub Release that `cargo-dist` built from
//! the tag `release-plz` pushed (see RELEASING.md). That is why the artifact
//! layout is spelled out here as constants rather than left to `self_update`'s
//! guesswork: `archive_name` is asserted against what `dist plan` actually
//! prints, so a change to `dist-workspace.toml` that would break `mana upgrade`
//! fails a test instead of a user's update.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The repo mana releases from. Duplicated in Cargo.toml's `repository` key,
/// which cargo-dist reads to build the same URLs from the other side.
const REPO_OWNER: &str = "SekmenAhmet";
const REPO_NAME: &str = "mana";

/// The binary, and -- because cargo-dist names archives after the *package*,
/// which here has the same name -- the archive's stem too.
const BIN_NAME: &str = "mana";

/// The archive extension `dist-workspace.toml` pins for every target, Windows
/// included. It has to agree with the `archive-tar` + `compression-flate2`
/// features in Cargo.toml: `self_update` treats an extension it does not know
/// as "not an archive" and copies the raw bytes over the binary, silently.
const ARCHIVE_EXT: &str = ".tar.gz";

/// Where the binary sits *inside* the archive.
///
/// cargo-dist tarballs are not flat: `mana-<target>.tar.gz` unpacks to a
/// directory named after the archive, and the binary is inside it. Verified by
/// unpacking a real `dist build` artifact (2026-08-15):
///
/// ```text
/// mana-aarch64-apple-darwin/
/// mana-aarch64-apple-darwin/mana
/// mana-aarch64-apple-darwin/README.md
/// ```
///
/// Without this, `self_update` looks for `mana` at the archive root, finds
/// nothing, and every `mana upgrade` fails. `{{ target }}` and `{{ bin }}` are
/// substituted by `self_update` at extraction time, and `{{ bin }}` carries
/// `std::env::consts::EXE_SUFFIX` -- so the same template resolves to
/// `mana-x86_64-pc-windows-msvc/mana.exe` on Windows.
const BIN_PATH_IN_ARCHIVE: &str = "mana-{{ target }}/{{ bin }}";

/// This build's version, as `self_update`'s semver comparison wants it.
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Set this to anything but `0` or the empty string and `mana launch` never
/// looks for a newer release. Documented in README's "Updates" section.
pub(crate) const NO_CHECK_ENV: &str = "MANA_NO_UPDATE_CHECK";

/// The launch-time check's answer is cached this long. One call a day is
/// enough to notice a release; more than that is a network round trip nobody
/// asked for on a command that is supposed to start a session.
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// How long the TUI keeps looking for the check's answer before giving up on
/// it for this session.
///
/// It is a deadline on *waiting*, not on the request: `self_update` 0.44 has no
/// timeout knob (1.0 adds `.timeout()`; it is still a release candidate). The
/// thread is detached, so a request that outlives the deadline hurts nobody --
/// and it still writes the cache, which is what makes the *next* launch show
/// the notice instantly.
const CHECK_DEADLINE: Duration = Duration::from_secs(2);

/// Cache file under `~/.mana`.
const CACHE_FILE: &str = "update-check.json";

/// The archive cargo-dist uploads for `target`.
///
/// Asserted against `dist plan`'s real output in the tests below.
fn archive_name(target: &str) -> String {
    format!("{BIN_NAME}-{target}{ARCHIVE_EXT}")
}

pub(crate) fn describe_update_result(status: &self_update::Status) -> String {
    match status {
        self_update::Status::UpToDate(version) => format!("mana is already up to date ({version})"),
        self_update::Status::Updated(version) => {
            format!("mana updated to version {version}")
        }
    }
}

pub fn run() -> anyhow::Result<()> {
    let target = self_update::get_target();
    let status = self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .target(target)
        // The identifier is the whole archive name rather than the extension,
        // so what mana asks GitHub for is exactly what the test says cargo-dist
        // uploads -- a `.deb` or an `.msi` for the same triple could never be
        // taken for the tarball.
        //
        // It cannot separate the archive from its own `.sha256` sibling:
        // `asset_for` matches by substring, and every substring of the
        // archive's name is also a substring of the checksum's. What separates
        // those two is that GitHub returns release assets sorted by name
        // (verified against ruff, ripgrep and bat), so `X.tar.gz` is found
        // before `X.tar.gz.sha256`. If that ever stopped holding, the failure
        // would be loud -- a 100-byte text file is not a tarball -- and never a
        // corrupted binary.
        .identifier(&archive_name(target))
        .bin_path_in_archive(BIN_PATH_IN_ARCHIVE)
        .show_download_progress(true)
        .show_output(false)
        .no_confirm(true)
        .current_version(CURRENT_VERSION)
        .build()?
        .update()?;
    println!("{}", describe_update_result(&status));
    Ok(())
}

/// What the launch-time check remembered last time.
#[derive(Debug, Serialize, Deserialize)]
struct CachedCheck {
    /// Unix seconds at which the answer was fetched.
    checked_at: u64,
    /// The newest published version, without its `v` prefix.
    latest: String,
}

/// `true` when the user asked not to be checked on.
///
/// `0` and the empty string mean "off" so that `MANA_NO_UPDATE_CHECK=0` reads
/// the way anyone would expect it to, rather than being one more way to
/// silence the check by accident.
fn opted_out(value: Option<&str>) -> bool {
    matches!(value, Some(v) if !v.is_empty() && v != "0")
}

/// Whether a stamp written at `checked_at` still counts.
///
/// A stamp in the future is a clock that moved, not a fresh answer: treated as
/// stale, because the alternative is a machine that silently never checks
/// again.
fn cache_is_fresh(checked_at: u64, now: u64) -> bool {
    now >= checked_at && now - checked_at < CACHE_TTL.as_secs()
}

/// The one line the user sees, or nothing at all.
///
/// Nothing when the latest release is not newer, and nothing when either
/// version fails to parse as semver -- a tag someone pushed by hand is not
/// worth an error message on a command that was asked to start a session.
fn notice(current: &str, latest: &str) -> Option<String> {
    self_update::version::bump_is_greater(current, latest)
        .ok()
        .filter(|greater| *greater)
        .map(|_| format!("[mana] mana {latest} available -- run `mana upgrade`"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_cache(path: &Path) -> Option<CachedCheck> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Best effort: a cache that cannot be written costs one HTTP request next
/// launch, which is not worth interrupting anyone over.
fn write_cache(path: &Path, cache: &CachedCheck) {
    let Ok(body) = serde_json::to_string(cache) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, body);
}

/// Asks GitHub for the newest release, or gives up.
///
/// `api_url` overrides the API base; `None` means github.com. Every failure --
/// offline, rate limited, DNS, a repo with no releases yet -- collapses to
/// `None`, because being offline is the normal state of a laptop on a train.
fn fetch_latest_version(api_url: Option<&str>) -> Option<String> {
    let mut builder = self_update::backends::github::Update::configure();
    builder
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .current_version(CURRENT_VERSION);
    if let Some(url) = api_url {
        builder.with_url(url);
    }
    let release = builder.build().ok()?.get_latest_release().ok()?;
    Some(release.version)
}

/// The whole check, with the network behind the cache.
fn check(cache_path: &Path, current: &str, now: u64) -> Option<String> {
    let cached = read_cache(cache_path).filter(|c| cache_is_fresh(c.checked_at, now));
    let latest = match cached {
        Some(cached) => cached.latest,
        None => {
            let fetched = fetch_latest_version(None)?;
            write_cache(
                cache_path,
                &CachedCheck {
                    checked_at: now,
                    latest: fetched.clone(),
                },
            );
            fetched
        }
    };
    notice(current, &latest)
}

/// Starts the launch-time check on its own thread.
///
/// `None` when the user opted out. Otherwise a receiver that yields exactly
/// one line if there is something to say, and closes silently if there is not
/// -- which is also what every failure looks like from the caller's side.
pub(crate) fn spawn_check(home: &Path) -> Option<Receiver<String>> {
    // Reading the variable is the only part that cannot be exercised from a
    // test without mutating process-global state, so it is the only part that
    // lives out here.
    spawn_check_unless(home, opted_out(std::env::var(NO_CHECK_ENV).ok().as_deref()))
}

fn spawn_check_unless(home: &Path, opted_out: bool) -> Option<Receiver<String>> {
    if opted_out {
        return None;
    }
    let cache_path: PathBuf = home.join(CACHE_FILE);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        if let Some(line) = check(&cache_path, CURRENT_VERSION, now_secs()) {
            let _ = tx.send(line);
        }
    });
    Some(rx)
}

/// The launch loop's half of the check: one poll per tick, at most one line,
/// and no reason to keep the receiver once the deadline has passed.
///
/// Returns the line to show, if this is the tick it arrived on. `*receiver` is
/// cleared as soon as there is nothing left to wait for.
pub(crate) fn poll_check(
    receiver: &mut Option<Receiver<String>>,
    elapsed: Duration,
) -> Option<String> {
    let rx = receiver.as_ref()?;
    match rx.try_recv() {
        Ok(line) => {
            *receiver = None;
            Some(line)
        }
        Err(TryRecvError::Disconnected) => {
            *receiver = None;
            None
        }
        Err(TryRecvError::Empty) => {
            if elapsed >= CHECK_DEADLINE {
                *receiver = None;
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use self_update::Status;
    use std::net::TcpListener;

    #[test]
    fn describe_update_result_reports_already_up_to_date() {
        let status = Status::UpToDate("0.1.0".to_string());
        assert_eq!(
            describe_update_result(&status),
            "mana is already up to date (0.1.0)"
        );
    }

    #[test]
    fn describe_update_result_reports_new_version_installed() {
        let status = Status::Updated("0.2.0".to_string());
        assert_eq!(
            describe_update_result(&status),
            "mana updated to version 0.2.0"
        );
    }

    /// The names `dist plan` printed for this exact `dist-workspace.toml`
    /// (dist 0.32.0, 2026-08-15), copied verbatim. If cargo-dist ever changes
    /// how it names archives, this list is what notices.
    const DIST_PLAN_ARTIFACTS: &[&str] = &[
        "mana-aarch64-apple-darwin.tar.gz",
        "mana-aarch64-unknown-linux-gnu.tar.gz",
        "mana-x86_64-apple-darwin.tar.gz",
        "mana-x86_64-pc-windows-msvc.tar.gz",
        "mana-x86_64-unknown-linux-gnu.tar.gz",
    ];

    /// The targets cargo-dist is configured to build, read from the config
    /// itself rather than repeated here: adding a target without teaching
    /// `mana upgrade` about it should fail, not ship.
    fn configured_targets() -> Vec<String> {
        let config: toml::Value = toml::from_str(include_str!("../../dist-workspace.toml"))
            .expect("dist-workspace.toml is valid TOML");
        config["dist"]["targets"]
            .as_array()
            .expect("[dist].targets is an array")
            .iter()
            .map(|t| t.as_str().expect("a target is a string").to_string())
            .collect()
    }

    #[test]
    fn archive_name_matches_what_dist_plan_prints_for_every_target() {
        let mut produced: Vec<String> = configured_targets()
            .iter()
            .map(|target| archive_name(target))
            .collect();
        produced.sort();
        let mut expected: Vec<String> = DIST_PLAN_ARTIFACTS.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(produced, expected);
    }

    /// The template resolves to the directory cargo-dist puts inside the
    /// tarball, plus the binary.
    ///
    /// `{{ bin }}` is not `BIN_NAME`: `self_update`'s `bin_name()` appends
    /// `std::env::consts::EXE_SUFFIX` before storing it, which is what makes
    /// the Windows archive's `mana.exe` findable from the same template. This
    /// test substitutes the same way, so it asserts `.../mana` on unix and
    /// `.../mana.exe` on windows rather than pretending the two are alike.
    #[test]
    fn bin_path_in_archive_is_the_archive_stem_plus_the_binary() {
        let exe = format!("{BIN_NAME}{}", std::env::consts::EXE_SUFFIX);
        for target in configured_targets() {
            let stem = archive_name(&target)
                .strip_suffix(ARCHIVE_EXT)
                .expect("every archive ends with the configured extension")
                .to_string();
            let rendered = BIN_PATH_IN_ARCHIVE
                .replace("{{ target }}", &target)
                .replace("{{ bin }}", &exe);
            assert_eq!(rendered, format!("{stem}/{exe}"));
        }
    }

    #[test]
    fn dist_builds_every_target_the_five_platforms_need() {
        let targets = configured_targets();
        for expected in [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-pc-windows-msvc",
        ] {
            assert!(targets.iter().any(|t| t == expected), "missing {expected}");
        }
    }

    #[test]
    fn opted_out_only_when_the_variable_says_something() {
        assert!(!opted_out(None));
        assert!(!opted_out(Some("")));
        assert!(!opted_out(Some("0")));
        assert!(opted_out(Some("1")));
        assert!(opted_out(Some("true")));
        assert!(opted_out(Some("yes")));
    }

    #[test]
    fn cache_is_fresh_within_the_day_and_stale_after_it() {
        let day = CACHE_TTL.as_secs();
        assert!(cache_is_fresh(1000, 1000));
        assert!(cache_is_fresh(1000, 1000 + day - 1));
        assert!(!cache_is_fresh(1000, 1000 + day));
        assert!(!cache_is_fresh(1000, 1000 + day * 7));
    }

    #[test]
    fn cache_written_in_the_future_counts_as_stale() {
        assert!(!cache_is_fresh(2000, 1000));
    }

    #[test]
    fn notice_only_for_a_strictly_newer_version() {
        assert_eq!(
            notice("0.1.0", "0.2.0").as_deref(),
            Some("[mana] mana 0.2.0 available -- run `mana upgrade`")
        );
        assert_eq!(notice("0.1.0", "0.1.0"), None);
        assert_eq!(notice("0.2.0", "0.1.9"), None);
        assert!(notice("0.1.0", "1.0.0").is_some());
    }

    #[test]
    fn notice_is_silent_on_a_version_that_is_not_semver() {
        assert_eq!(notice("0.1.0", "nightly"), None);
        assert_eq!(notice("not-a-version", "0.2.0"), None);
    }

    #[test]
    fn cache_round_trips_and_a_fresh_one_answers_without_the_network() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join(CACHE_FILE);
        write_cache(
            &path,
            &CachedCheck {
                checked_at: 10_000,
                latest: "9.9.9".to_string(),
            },
        );
        let read = read_cache(&path).expect("the cache we just wrote reads back");
        assert_eq!(read.checked_at, 10_000);
        assert_eq!(read.latest, "9.9.9");
        // Fresh cache, so `check` must answer from disk. A network call here
        // would hang this test on a machine without one.
        assert_eq!(
            check(&path, "0.1.0", 10_000).as_deref(),
            Some("[mana] mana 9.9.9 available -- run `mana upgrade`")
        );
    }

    #[test]
    fn check_says_nothing_when_the_cached_version_is_not_newer() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CACHE_FILE);
        write_cache(
            &path,
            &CachedCheck {
                checked_at: 10_000,
                latest: "0.0.1".to_string(),
            },
        );
        assert_eq!(check(&path, "0.1.0", 10_000), None);
    }

    #[test]
    fn read_cache_shrugs_at_a_corrupt_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CACHE_FILE);
        std::fs::write(&path, "{not json").unwrap();
        assert!(read_cache(&path).is_none());
        assert!(read_cache(&tmp.path().join("absent.json")).is_none());
    }

    /// A port nothing is listening on: bind one, learn its number, drop it.
    fn closed_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        listener.local_addr().expect("the bound address").port()
    }

    #[test]
    fn fetch_is_silent_when_the_endpoint_is_not_there() {
        let url = format!("http://127.0.0.1:{}", closed_port());
        assert_eq!(fetch_latest_version(Some(&url)), None);
    }

    #[test]
    fn poll_check_yields_the_line_once_and_then_lets_go() {
        let (tx, rx) = mpsc::channel();
        tx.send("[mana] mana 0.2.0 available".to_string()).unwrap();
        let mut receiver = Some(rx);
        assert_eq!(
            poll_check(&mut receiver, Duration::ZERO).as_deref(),
            Some("[mana] mana 0.2.0 available")
        );
        assert!(receiver.is_none());
        assert_eq!(poll_check(&mut receiver, Duration::ZERO), None);
    }

    #[test]
    fn poll_check_lets_go_when_the_check_had_nothing_to_say() {
        let (tx, rx) = mpsc::channel::<String>();
        drop(tx);
        let mut receiver = Some(rx);
        assert_eq!(poll_check(&mut receiver, Duration::ZERO), None);
        assert!(receiver.is_none());
    }

    #[test]
    fn poll_check_waits_until_the_deadline_then_stops_looking() {
        let (_tx, rx) = mpsc::channel::<String>();
        let mut receiver = Some(rx);
        assert_eq!(poll_check(&mut receiver, Duration::from_millis(1)), None);
        assert!(receiver.is_some(), "still within the deadline");
        assert_eq!(poll_check(&mut receiver, CHECK_DEADLINE), None);
        assert!(receiver.is_none(), "deadline passed");
    }

    #[test]
    fn spawn_check_starts_nothing_when_opted_out() {
        let tmp = tempfile::tempdir().unwrap();
        // A cache that would otherwise produce a line: opting out has to win
        // over having an answer ready.
        write_cache(
            &tmp.path().join(CACHE_FILE),
            &CachedCheck {
                checked_at: now_secs(),
                latest: "9.9.9".to_string(),
            },
        );
        assert!(spawn_check_unless(tmp.path(), true).is_none());
    }

    #[test]
    fn spawn_check_delivers_the_line_from_a_fresh_cache() {
        let tmp = tempfile::tempdir().unwrap();
        write_cache(
            &tmp.path().join(CACHE_FILE),
            &CachedCheck {
                checked_at: now_secs(),
                latest: "9999.0.0".to_string(),
            },
        );
        let rx = spawn_check_unless(tmp.path(), false).expect("not opted out");
        let line = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a fresh cache answers from disk, with no network involved");
        assert_eq!(line, "[mana] mana 9999.0.0 available -- run `mana upgrade`");
    }

    #[test]
    fn the_opt_out_variable_is_the_documented_one() {
        assert_eq!(NO_CHECK_ENV, "MANA_NO_UPDATE_CHECK");
    }
}
