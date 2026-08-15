//! `mana launch <cli>` -- the v2 PM session.
//!
//! Six things happen, in this order, and each one kills a v1 defect:
//!
//! 1. **Resolve the catalogue entry.** Everything CLI-specific below comes
//!    from it, so nothing here branches on which CLI the user named.
//! 2. **Install the PM skill.** `assets/roles/pm/SKILL.md` is embedded in the
//!    binary and rewritten to the CLI's own skills directory on every launch,
//!    so the role text can never drift from the code that serves its tools
//!    (design §6). SKILL.md is the one role format ~40 tools already read;
//!    `--append-system-prompt` exists on one CLI in five.
//! 3. **Write the MCP config.** mana registers itself by `current_exe()`, so
//!    `mana` does not have to be on `$PATH` -- one of v1's three launch
//!    blockers. The flag that points the CLI at the file is an argv template
//!    in the catalogue, not a branch here.
//! 4. **Start the driver and send one activation line.** The skill does the
//!    teaching; the launch message only says which skill to load.
//! 5. **Run the loop.** PM events in, chat pane out; `notifications.jsonl`
//!    tailed and each finished dispatch injected as a user turn, which is how
//!    the PM learns an executor finished without polling for it.
//! 6. **Shut down.** Ctrl+C or a dead PM both end the session and reap the
//!    process -- v1 could do neither.
//!
//! Steps 1-4 and the loop's body are `prepare_session` and `Session`, with no
//! terminal anywhere near them: the milestone-2 smoke test drives that exact
//! code against a fake PM script, which is the only way the flow gets tested
//! without paying a real CLI.

use crate::catalog::{Catalog, CliEntry, ToolChannel, substitute};
use crate::mcp::runs::{Notification, notifications_path};
use crate::pm::{self, PmEvent, PmTransport};
use crate::project::{
    ProjectPaths, ensure_project_structure, mana_home, project_name_from_dir, resolve_project_paths,
};
use crate::task::Role;
use crate::tui::app::{App, Source};
use crate::tui::event::{AppEvent, CrosstermEventSource, EventSource, map_key_event};
use crate::tui::graph::GraphCache;
use crate::tui::render;
use anyhow::{Context, Result, bail};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The PM role text, embedded so it ships and versions with the tools it
/// teaches (design §6).
const PM_SKILL: &str = include_str!("../../assets/roles/pm/SKILL.md");

/// The whole of what mana says at launch. Everything else the PM needs to
/// know is in the skill and in the tool schemas -- a long activation message
/// would be a second copy of both, free to disagree with them.
const ACTIVATION: &str = "You are the mana PM for this session. Load and follow the mana-pm skill.";

/// Directory name the skill is installed under, inside the CLI's skills dir.
const SKILL_NAME: &str = "mana-pm";

/// The user's catalogue escape valve (design §7), read from the same place
/// every other command reads it. A missing file is normal.
const CATALOG_OVERRIDE: &str = "catalog.local.toml";

/// Where mana writes the MCP registration it hands to the PM's CLI.
const MCP_CONFIG: &str = "mcp-config.json";

/// How often `notifications.jsonl` is read. A dispatch takes minutes, so half
/// a second is instant from where the user sits, and it costs one `metadata`
/// call on a file that is usually unchanged.
const NOTIFICATION_POLL: Duration = Duration::from_millis(500);

/// How long the loop blocks waiting for a key before redrawing.
const TICK: Duration = Duration::from_millis(50);

pub fn run(agent_cli: &str) -> Result<()> {
    let home = mana_home()?;
    let project_root = std::env::current_dir()?;
    let mut session = prepare_session(&home, &project_root, agent_cli)?;
    let mut app = App::new(&session.cli_name);
    app.push(
        Source::Mana,
        &format!(
            "[mana] PM session started on {}. The mana-pm skill is installed at {}.",
            session.cli_name,
            session.skill_path.display()
        ),
    );

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let outcome = run_loop(
        &mut terminal,
        &mut session,
        &mut app,
        &mut GraphCache::new(),
        &mut CrosstermEventSource,
    );
    // Restore the terminal before anything is printed or propagated: an error
    // rendered into the alternate screen is an error nobody ever reads.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    let _ = session.shutdown();
    match outcome? {
        SessionEnd::UserQuit => Ok(()),
        // A PM that died on its own took the session with it, and the only
        // explanation there is arrived on its stderr -- which is now behind a
        // screen the user cannot get back to.
        SessionEnd::PmExited { code } => {
            let status = match code {
                Some(code) => format!("exit code {code}"),
                None => "a signal".to_string(),
            };
            let reason = app
                .last_raw
                .map(|line| format!("\nits last output was: {line}"))
                .unwrap_or_default();
            bail!(
                "the PM ({}) ended the session with {status}{reason}",
                session.cli_name
            )
        }
    }
}

/// Why the loop returned.
#[derive(Debug, PartialEq)]
enum SessionEnd {
    UserQuit,
    PmExited { code: Option<i32> },
}

/// A live PM session, with no terminal attached.
///
/// This is the half of `run` that has decisions in it, kept separable so the
/// milestone-2 smoke test can drive a real launch -- skill, MCP config,
/// activation turn, notification injection, shutdown -- against a fake PM
/// script instead of a paid CLI.
struct Session {
    pm: Box<dyn PmTransport>,
    paths: ProjectPaths,
    /// Product name of the CLI driving the PM, for the status bar.
    cli_name: String,
    /// Where the skill was written this launch. Reported to the user, because
    /// "mana rewrote a file in your config directory" should not be silent.
    skill_path: PathBuf,
    notifications: NotificationTail,
}

impl Session {
    /// Everything the PM has said since the last call.
    fn drain(&mut self) -> Vec<PmEvent> {
        std::iter::from_fn(|| self.pm.events().try_recv().ok()).collect()
    }

    fn send_user(&mut self, text: &str) -> Result<()> {
        self.pm.send_user(text)
    }

    /// Injects one user turn per dispatch that finished since the last poll,
    /// and returns what was injected so the chat pane can show it too.
    ///
    /// This is the whole reason the PM does not have to poll: it asked for a
    /// dispatch minutes ago, the thread that ran it wrote a line to
    /// `notifications.jsonl` when it ended, and mana turns that line into a
    /// turn the PM reads like any other message.
    fn poll_notifications(&mut self, now: Instant) -> Result<Vec<String>> {
        let mut sent = Vec::new();
        for notification in self.notifications.poll(now) {
            let message = notification_message(&notification);
            self.pm.send_user(&message)?;
            sent.push(message);
        }
        Ok(sent)
    }

    fn shutdown(&mut self) -> Result<()> {
        self.pm.shutdown()
    }
}

/// Resolves the CLI, installs the skill, wires the tool channel and starts the
/// session -- everything up to the first frame.
fn prepare_session(home: &Path, project_root: &Path, agent_cli: &str) -> Result<Session> {
    let catalog = Catalog::load(Some(&home.join(CATALOG_OVERRIDE)))?;
    let entry = catalog.get(agent_cli).with_context(|| {
        format!(
            "unknown CLI id '{agent_cli}' -- the catalogue knows: {}",
            catalog.ids().join(", ")
        )
    })?;

    let paths = resolve_project_paths(home, &project_name_from_dir(project_root));
    ensure_project_structure(&paths)?;

    let skill_path = install_pm_skill(entry, dirs::home_dir().as_deref())?;
    let extra_args = tool_channel_args(entry, &paths, project_root)?;

    let mut pm = pm::start(entry, &extra_args)?;
    pm.send_user(ACTIVATION)
        .context("sending the activation message to the PM")?;

    Ok(Session {
        pm,
        cli_name: entry.cli.name.clone(),
        skill_path,
        notifications: NotificationTail::new(notifications_path(&paths)),
        paths,
    })
}

/// Writes the PM skill where this CLI will read it, and returns the path.
///
/// Rewritten on every launch on purpose: the file is generated output, and a
/// user who edited it (or an older mana that wrote an older version) would
/// otherwise leave the PM following instructions that no longer match the
/// tools it is served.
fn install_pm_skill(entry: &CliEntry, home: Option<&Path>) -> Result<PathBuf> {
    let candidates: Vec<PathBuf> = entry
        .skills
        .dirs
        .iter()
        .map(|dir| expand_home(dir, home))
        .collect();
    // Most specific first, and the first one that already exists wins: a user
    // who has `~/.claude/skills` gets the skill where that CLI looks first,
    // and the vendor-neutral `~/.agents/skills` is the fallback the catalogue
    // lists after it.
    let dir = candidates
        .iter()
        .find(|dir| dir.is_dir())
        .or_else(|| candidates.first())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{}: [skills].dirs is empty, so mana has nowhere to install the PM role",
                entry.cli.id
            )
        })?;

    let path = dir.join(SKILL_NAME).join("SKILL.md");
    std::fs::create_dir_all(path.parent().expect("joined two components above"))
        .with_context(|| format!("creating the skill directory {}", dir.display()))?;
    std::fs::write(&path, PM_SKILL)
        .with_context(|| format!("writing the PM skill to {}", path.display()))?;
    Ok(path)
}

/// The argv that attaches mana's tools to this PM, plus the argv that takes
/// away its ability to write code.
fn tool_channel_args(
    entry: &CliEntry,
    paths: &ProjectPaths,
    project_root: &Path,
) -> Result<Vec<String>> {
    let mut args = Vec::new();
    match entry.tools.channel {
        ToolChannel::Mcp => {
            let config = write_mcp_config(paths, project_root)?;
            let config = config.to_string_lossy().into_owned();
            let vars = HashMap::from([("config_path", config.as_str())]);
            args.extend(
                substitute(&entry.tools.mcp_args, &vars)
                    .with_context(|| format!("{}: [tools].mcp_args", entry.cli.id))?,
            );
        }
        // Nothing to attach: the PM emits fenced blocks and mana parses them
        // out of the same event stream. No shipped entry reaches this arm --
        // agy's driver is refused earlier -- and the parser that executes
        // those blocks lands with it (mana v2, task 3.2).
        ToolChannel::Sentinel => {}
    }
    // Empty for a CLI with no equivalent flag, which is why this is data:
    // the PM's no-code rule is enforced where it can be and stated in the
    // skill everywhere else (design §6).
    args.extend(
        substitute(&entry.pm.permission_args, &HashMap::new())
            .with_context(|| format!("{}: [pm].permission_args", entry.cli.id))?,
    );
    Ok(args)
}

/// Writes the MCP registration the PM's CLI will read, and returns its path.
fn write_mcp_config(paths: &ProjectPaths, project_root: &Path) -> Result<PathBuf> {
    let exe = std::env::current_exe().context(
        "resolving mana's own path, which is what the PM's CLI will run to reach mana's tools",
    )?;
    let path = paths.root.join(MCP_CONFIG);
    std::fs::create_dir_all(&paths.root)?;
    let document = serde_json::to_string_pretty(&mcp_config(&exe, project_root))?;
    std::fs::write(&path, format!("{document}\n"))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// The registration document.
///
/// `command` is mana's own absolute path rather than the string "mana": one of
/// v1's three launch blockers was that the PM's CLI could not find mana on
/// `$PATH`, and a binary that just ran from a build directory is not on it at
/// all (design §5). `--project-root` is passed because the PM's CLI spawns
/// this server from wherever it happens to be, and which project a task
/// belongs to is not negotiable at that point.
fn mcp_config(exe: &Path, project_root: &Path) -> serde_json::Value {
    serde_json::json!({
        "mcpServers": {
            "mana": {
                "command": exe.to_string_lossy(),
                "args": [
                    "mcp-server",
                    "--project-root",
                    project_root.to_string_lossy(),
                ],
            }
        }
    })
}

/// Expands a leading `~`, which is how the catalogue writes skills
/// directories -- it is the notation the CLIs' own documentation uses, and it
/// is the only place mana has to understand it.
fn expand_home(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

/// Follows `notifications.jsonl` from wherever it was when the session
/// started.
///
/// Starting at the end rather than at the beginning is the whole subtlety:
/// the file is append-only across every session this project ever had, and
/// replaying it would have the PM chasing tasks somebody closed last week.
struct NotificationTail {
    path: PathBuf,
    offset: u64,
    next_poll: Instant,
}

impl NotificationTail {
    fn new(path: PathBuf) -> Self {
        let offset = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        NotificationTail {
            path,
            offset,
            next_poll: Instant::now(),
        }
    }

    fn poll(&mut self, now: Instant) -> Vec<Notification> {
        if now < self.next_poll {
            return Vec::new();
        }
        self.next_poll = now + NOTIFICATION_POLL;
        self.read_new()
    }

    /// Reads the complete lines appended since the last read.
    ///
    /// Byte offsets, not line counts: several dispatch threads append to this
    /// file independently, so a read can land mid-write. Everything after the
    /// last newline is left for the next poll, and the offset advances by the
    /// bytes actually consumed -- a partial line is never parsed and never
    /// skipped. Every failure returns "nothing new": a notification is a
    /// convenience, and losing the file must not take the session down.
    fn read_new(&mut self) -> Vec<Notification> {
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return Vec::new();
        };
        let length = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        if length < self.offset {
            // Truncated or replaced under us: start over rather than seek past
            // the end and read nothing forever.
            self.offset = 0;
        }
        if length == self.offset || file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            return Vec::new();
        }
        let Some(last_newline) = bytes.iter().rposition(|byte| *byte == b'\n') else {
            return Vec::new();
        };
        let complete = &bytes[..=last_newline];
        self.offset += complete.len() as u64;
        String::from_utf8_lossy(complete)
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

/// The turn a finished dispatch becomes.
///
/// It ends by asking for a decision because the PM's loop is decide → dispatch
/// → decide: a bare status line invites an acknowledgement, and an
/// acknowledgement is a paid turn that moved nothing.
fn notification_message(notification: &Notification) -> String {
    format!(
        "[mana] {} finished for task {}: {}. Decide the next step.",
        role_word(&notification.role),
        notification.task_id,
        notification.outcome
    )
}

/// The role as the PM knows it -- the same word the `launch_subagent` schema
/// uses, so a notification reads back in the vocabulary the PM wrote in.
fn role_word(role: &Role) -> &'static str {
    match role {
        Role::Executor => "executor",
        Role::Reviewer => "reviewer",
    }
}

fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    session: &mut Session,
    app: &mut App,
    graph: &mut GraphCache,
    events: &mut dyn EventSource,
) -> Result<SessionEnd> {
    loop {
        let now = Instant::now();
        let mut ended = None;
        for event in session.drain() {
            app.apply(&event);
            if let PmEvent::Exited { code } = event {
                ended = Some(code);
            }
        }

        // A dispatch reporting in is the one event that certainly changed the
        // graph, so it is what refreshes it -- not the frame rate.
        //
        // A delivery failure is almost always a PM that just died, and the
        // `Exited` event ends the session a tick later with a better message
        // than this one. Reported and carried on rather than propagated: the
        // user should see why, in the pane they are already looking at.
        let finished = match session.poll_notifications(now) {
            Ok(finished) => finished,
            Err(error) => {
                app.push(
                    Source::Mana,
                    &format!("[mana] could not tell the PM a dispatch finished: {error:#}"),
                );
                Vec::new()
            }
        };
        let changed = !finished.is_empty();
        for message in finished {
            app.push(Source::Mana, &message);
        }
        graph.refresh(&session.paths, now, changed);

        terminal.draw(|frame| render::draw(frame, app, graph.nodes()))?;

        // Drawn first, so the last thing the PM said is on screen before mana
        // reports that it is gone.
        if let Some(code) = ended {
            return Ok(SessionEnd::PmExited { code });
        }

        if let Some(key) = events.poll_key(TICK)?
            && let Some(app_event) = map_key_event(key.code, key.modifiers)
            && !apply_app_event(app_event, app, session)
        {
            return Ok(SessionEnd::UserQuit);
        }
    }
}

/// Applies one decoded key. `false` means quit.
///
/// Nothing here can fail the session: a turn that cannot be delivered says so
/// in the chat pane, because the user typed it and deserves an answer now,
/// and because tearing the TUI down over it would take the explanation with
/// it. The PM's `Exited` event is what actually ends the session.
fn apply_app_event(event: AppEvent, app: &mut App, session: &mut Session) -> bool {
    match event {
        AppEvent::Quit => return false,
        AppEvent::ToggleGraph => app.toggle_graph(),
        AppEvent::Key(c) => app.input.push(c),
        AppEvent::Backspace => {
            app.input.pop();
        }
        AppEvent::Enter => {
            let message = std::mem::take(&mut app.input);
            if message.trim() == "/graph" {
                // A local UI command: sending it would just leave the PM
                // wondering what "/graph" was supposed to mean.
                app.toggle_graph();
            } else if !message.trim().is_empty() {
                app.push(Source::User, &message);
                if let Err(error) = session.send_user(&message) {
                    app.push(
                        Source::Mana,
                        &format!("[mana] that turn did not reach the PM: {error:#}"),
                    );
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::parse_entry;

    /// A complete entry for a CLI that does not exist, built through the real
    /// parser so it cannot drift from the schema.
    pub(super) fn entry_source(
        bin: &str,
        skills: &[&str],
        permission: &str,
        channel: &str,
    ) -> String {
        let skills = serde_json::to_string(skills).unwrap();
        format!(
            r#"
schema = 1
notes = "launch fixture"

[cli]
id = "fixture"
name = "Fixture CLI"
bin = "{bin}"
version_args = ["--version"]

[pm]
driver = "stream"
args = ["-p"]
prompt = "stdin-jsonl"
{permission}

[pm.events]
text = "$.message.content[?@.type=='text'].text"
usage = "$.usage"

[tools]
channel = "{channel}"
mcp_args = ["--mcp-config", "{{config_path}}"]

[subagent]
args = []
prompt = "argv"
max_concurrent = 0
cwd_required_in_brief = false

[models]
discovery_args = []

[skills]
dirs = {skills}

[install]
url = "https://example.invalid/fixture"
"#
        )
    }

    fn entry(skills: &[&str]) -> CliEntry {
        parse_entry(&entry_source("fixture-cli", skills, "", "mcp")).unwrap()
    }

    fn paths_in(home: &Path) -> ProjectPaths {
        let paths = resolve_project_paths(home, "demo");
        ensure_project_structure(&paths).unwrap();
        paths
    }

    #[test]
    fn the_skill_lands_in_the_first_directory_that_already_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let second = tmp.path().join("agents/skills");
        std::fs::create_dir_all(&second).unwrap();
        let entry = entry(&[
            tmp.path().join("claude/skills").to_str().unwrap(),
            second.to_str().unwrap(),
        ]);

        let path = install_pm_skill(&entry, None).unwrap();
        assert_eq!(path, second.join("mana-pm/SKILL.md"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), PM_SKILL);
        // The directory that did not exist is left alone.
        assert!(!tmp.path().join("claude/skills").exists());
    }

    /// A fresh machine has none of them, and refusing to launch over a
    /// missing config directory would be absurd.
    #[test]
    fn the_first_directory_is_created_when_none_exists_yet() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("claude/skills");
        let entry = entry(&[
            first.to_str().unwrap(),
            tmp.path().join("agents/skills").to_str().unwrap(),
        ]);

        let path = install_pm_skill(&entry, None).unwrap();
        assert_eq!(path, first.join("mana-pm/SKILL.md"));
        assert!(path.exists());
    }

    /// The drift-proofing: whatever was there is replaced by what shipped in
    /// this binary.
    #[test]
    fn an_existing_skill_file_is_overwritten_every_launch() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skills");
        std::fs::create_dir_all(dir.join(SKILL_NAME)).unwrap();
        std::fs::write(dir.join(SKILL_NAME).join("SKILL.md"), "stale v1 text").unwrap();

        let path = install_pm_skill(&entry(&[dir.to_str().unwrap()]), None).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), PM_SKILL);
    }

    #[test]
    fn a_catalogue_entry_with_nowhere_to_put_the_skill_says_so() {
        let error = install_pm_skill(&entry(&[]), None).unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("[skills].dirs"), "{rendered}");
        assert!(rendered.contains("fixture"), "{rendered}");
    }

    #[test]
    fn skills_directories_are_written_with_a_tilde_and_read_from_the_home_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = entry(&["~/.fixture/skills"]);
        let path = install_pm_skill(&entry, Some(tmp.path())).unwrap();
        assert_eq!(path, tmp.path().join(".fixture/skills/mana-pm/SKILL.md"));
    }

    #[test]
    fn expand_home_leaves_absolute_and_bare_paths_alone() {
        let home = PathBuf::from("/home/x");
        assert_eq!(
            expand_home("/etc/skills", Some(&home)),
            PathBuf::from("/etc/skills")
        );
        // `~someone/skills` is another user's home, which is not a thing mana
        // resolves -- and not a thing any catalogue entry writes.
        assert_eq!(
            expand_home("~other/skills", Some(&home)),
            PathBuf::from("~other/skills")
        );
        // No home directory to expand against: better a relative path than a
        // panic on a machine without one.
        assert_eq!(expand_home("~/skills", None), PathBuf::from("~/skills"));
    }

    /// The `$PATH` blocker, structurally: the config names a binary by its
    /// absolute path, so nothing has to be installed anywhere for the PM to
    /// reach mana's tools.
    #[test]
    fn the_mcp_config_points_at_manas_own_binary_and_the_project() {
        let document = mcp_config(
            Path::new("/opt/mana/bin/mana"),
            Path::new("/home/x/code/demo"),
        );
        let server = &document["mcpServers"]["mana"];
        assert_eq!(server["command"], "/opt/mana/bin/mana");
        assert_eq!(
            server["args"],
            serde_json::json!(["mcp-server", "--project-root", "/home/x/code/demo"])
        );
    }

    #[test]
    fn the_mcp_config_is_written_and_substituted_into_the_catalogue_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let project = tmp.path().join("code/demo");

        let args = tool_channel_args(&entry(&["/nowhere"]), &paths, &project).unwrap();
        let config = paths.root.join(MCP_CONFIG);
        assert_eq!(
            args,
            vec![
                "--mcp-config".to_string(),
                config.to_string_lossy().into_owned()
            ]
        );

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(
            written["mcpServers"]["mana"]["command"],
            std::env::current_exe().unwrap().to_string_lossy().as_ref()
        );
        assert_eq!(
            written["mcpServers"]["mana"]["args"][2],
            project.to_string_lossy().as_ref()
        );
    }

    #[test]
    fn permission_args_are_appended_after_the_tool_channel_flags() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let entry = parse_entry(&entry_source(
            "fixture-cli",
            &["/nowhere"],
            r#"permission_args = ["--allowedTools", "mcp__mana__*,Read"]"#,
            "mcp",
        ))
        .unwrap();

        let args = tool_channel_args(&entry, &paths, tmp.path()).unwrap();
        assert_eq!(args.len(), 4);
        assert_eq!(&args[2..], ["--allowedTools", "mcp__mana__*,Read"]);
    }

    /// A sentinel CLI has no config to write and no flag to pass; its
    /// permission argv, if it had any, would still apply.
    #[test]
    fn a_sentinel_channel_asks_for_no_mcp_flags_and_writes_no_config() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let entry =
            parse_entry(&entry_source("fixture-cli", &["/nowhere"], "", "sentinel")).unwrap();

        assert!(
            tool_channel_args(&entry, &paths, tmp.path())
                .unwrap()
                .is_empty()
        );
        assert!(!paths.root.join(MCP_CONFIG).exists());
    }

    pub(super) fn notification(role: Role, task_id: &str, outcome: &str) -> Notification {
        Notification {
            ts: "2026-08-15T10:00:00Z".to_string(),
            task_id: task_id.to_string(),
            role,
            agent_id: "agent-1".to_string(),
            outcome: outcome.to_string(),
        }
    }

    #[test]
    fn a_finished_dispatch_becomes_a_turn_that_asks_for_a_decision() {
        let message =
            notification_message(&notification(Role::Reviewer, "3f2a1b6c", "exit 0 in 12.3s"));
        assert_eq!(
            message,
            "[mana] reviewer finished for task 3f2a1b6c: exit 0 in 12.3s. Decide the next step."
        );
    }

    pub(super) fn append(path: &Path, notification: &Notification) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(notification).unwrap()).unwrap();
    }

    #[test]
    fn the_tail_reports_only_lines_appended_after_it_started() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("notifications.jsonl");
        // A previous session's history, which the PM must not be told about.
        append(&path, &notification(Role::Executor, "old-task", "exit 0"));

        let mut tail = NotificationTail::new(path.clone());
        assert!(tail.read_new().is_empty());

        append(&path, &notification(Role::Executor, "new-task", "exit 0"));
        let seen = tail.read_new();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].task_id, "new-task");
        // ...and nothing is reported twice.
        assert!(tail.read_new().is_empty());
    }

    #[test]
    fn a_missing_notifications_file_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tail = NotificationTail::new(tmp.path().join("absent.jsonl"));
        assert!(tail.read_new().is_empty());
    }

    /// Several dispatch threads append independently, so a read can land in
    /// the middle of one. The half-line must wait, not be parsed or skipped.
    #[test]
    fn a_half_written_line_is_left_for_the_next_poll() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("notifications.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut tail = NotificationTail::new(path.clone());

        let line =
            serde_json::to_string(&notification(Role::Executor, "task-1", "exit 0")).unwrap();
        let (head, rest) = line.split_at(20);
        std::fs::write(&path, head).unwrap();
        assert!(tail.read_new().is_empty());

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{rest}").unwrap();
        assert_eq!(tail.read_new()[0].task_id, "task-1");
    }

    #[test]
    fn a_line_that_is_not_a_notification_is_skipped_rather_than_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("notifications.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut tail = NotificationTail::new(path.clone());

        std::fs::write(&path, "{\"half\": \"a record\"}\n").unwrap();
        append(&path, &notification(Role::Executor, "task-1", "exit 0"));
        let seen = tail.read_new();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].task_id, "task-1");
    }

    /// A file replaced (rather than appended to) leaves the offset past the
    /// end; reading nothing forever after that would be the worst outcome.
    #[test]
    fn a_truncated_file_is_read_from_the_start_again() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("notifications.jsonl");
        append(&path, &notification(Role::Executor, "task-1", "exit 0"));
        let mut tail = NotificationTail::new(path.clone());

        // Noticed at the next read, which is the only moment mana looks: the
        // file is shorter than where the tail had got to.
        std::fs::write(&path, "").unwrap();
        assert!(tail.read_new().is_empty());

        append(&path, &notification(Role::Reviewer, "task-2", "exit 0"));
        assert_eq!(tail.read_new()[0].task_id, "task-2");
    }

    #[test]
    fn the_tail_is_polled_at_most_once_per_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("notifications.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut tail = NotificationTail::new(path.clone());
        // After the tail exists: it opens its window at construction time, so
        // an earlier instant would fall inside it and prove nothing.
        let start = Instant::now();

        append(&path, &notification(Role::Executor, "task-1", "exit 0"));
        assert_eq!(tail.poll(start).len(), 1);

        append(&path, &notification(Role::Executor, "task-2", "exit 0"));
        assert!(
            tail.poll(start).is_empty(),
            "polled again within the window"
        );
        assert_eq!(tail.poll(start + NOTIFICATION_POLL).len(), 1);
    }
}

/// The milestone-2 smoke test: the real launch pathway, driven headlessly
/// against a fake PM that is a shell script.
///
/// Everything a paid CLI would do is faked; everything mana does is real --
/// the catalogue override, the skill install, the MCP config, `pm::start`, the
/// activation turn, the notification tail and the shutdown. What it cannot
/// cover is whether a real CLI honours the flags, which is what the README's
/// manual QA is for.
///
/// Unix-only because the fake PM is a shell script, like every other process
/// test in the tree.
#[cfg(all(test, unix))]
mod smoke {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        project: PathBuf,
        /// Where the fake PM appends every frame mana wrote to its stdin.
        received: PathBuf,
        /// Where it dumps the argv it was started with.
        argv: PathBuf,
        skills: PathBuf,
    }

    impl Fixture {
        fn new() -> Fixture {
            let tmp = tempfile::tempdir().unwrap();
            let fixture = Fixture {
                home: tmp.path().join("mana-home"),
                project: tmp.path().join("demo"),
                received: tmp.path().join("received.jsonl"),
                argv: tmp.path().join("argv.txt"),
                skills: tmp.path().join("skills"),
                _tmp: tmp,
            };
            std::fs::create_dir_all(&fixture.project).unwrap();
            std::fs::create_dir_all(&fixture.home).unwrap();
            std::fs::create_dir_all(&fixture.skills).unwrap();
            fixture.write_override(&fixture.fake_pm());
            fixture
        }

        /// A PM that records what it is told and answers every turn once.
        fn fake_pm(&self) -> String {
            let path = self.home.join("fake-pm");
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\n\
                     printf '%s\\n' \"$*\" > '{argv}'\n\
                     while IFS= read -r line; do\n\
                     \x20 printf '%s\\n' \"$line\" >> '{received}'\n\
                     \x20 echo '{{\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"ack\"}}]}}}}'\n\
                     done\n",
                    argv = self.argv.display(),
                    received = self.received.display(),
                ),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.to_string_lossy().into_owned()
        }

        /// Goes through the real override path (design §7) rather than
        /// building a `Catalog` by hand, so the test exercises the code a user
        /// with a broken CLI would.
        fn write_override(&self, bin: &str) {
            let source = super::tests::entry_source(
                bin,
                &[self.skills.to_str().unwrap()],
                r#"permission_args = ["--allowedTools", "mcp__mana__*"]"#,
                "mcp",
            );
            std::fs::write(self.home.join(CATALOG_OVERRIDE), source).unwrap();
        }

        fn paths(&self) -> ProjectPaths {
            resolve_project_paths(&self.home, &project_name_from_dir(&self.project))
        }

        /// Waits for `path` to contain `needle`, which is how a test observes
        /// a child process that writes on its own schedule.
        fn wait_for(&self, path: &Path, needle: &str) -> String {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let contents = std::fs::read_to_string(path).unwrap_or_default();
                if contents.contains(needle) {
                    return contents;
                }
                assert!(
                    Instant::now() < deadline,
                    "{} never contained {needle:?}; it holds: {contents}",
                    path.display()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    #[test]
    fn a_launch_installs_the_skill_registers_mana_and_activates_the_pm() {
        let fixture = Fixture::new();
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture").expect("the PM started");

        // 1. The role text is on disk where the CLI reads skills from.
        let skill = fixture.skills.join("mana-pm/SKILL.md");
        assert_eq!(std::fs::read_to_string(&skill).unwrap(), PM_SKILL);
        assert_eq!(session.skill_path, skill);

        // 2. mana registered itself by absolute path, for this project.
        let config = fixture.paths().root.join(MCP_CONFIG);
        let document: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(
            document["mcpServers"]["mana"]["command"],
            std::env::current_exe().unwrap().to_string_lossy().as_ref()
        );
        assert_eq!(
            document["mcpServers"]["mana"]["args"],
            serde_json::json!([
                "mcp-server",
                "--project-root",
                fixture.project.to_string_lossy()
            ])
        );

        // 3. ...and the CLI was started with the flag that reads it, plus the
        // permission argv that keeps the PM from writing code.
        let argv = fixture.wait_for(&fixture.argv, "--mcp-config");
        assert!(argv.contains(config.to_string_lossy().as_ref()), "{argv}");
        assert!(argv.contains("--allowedTools mcp__mana__*"), "{argv}");

        // 4. The activation message reached the PM's stdin as a real frame.
        let received = fixture.wait_for(&fixture.received, ACTIVATION);
        assert_eq!(received.lines().count(), 1, "{received}");
        let frame: serde_json::Value =
            serde_json::from_str(received.lines().next().unwrap()).unwrap();
        assert_eq!(frame["type"], "user");
        assert_eq!(frame["message"]["content"], ACTIVATION);

        // ...and the PM's answer came back as chat text, not as raw noise.
        let mut app = App::new(&session.cli_name);
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.lines().next().is_none() && Instant::now() < deadline {
            for event in session.drain() {
                app.apply(&event);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(app.lines().next().unwrap().text, "ack");

        // 5. A dispatch reporting in is injected as a user turn -- this is how
        // the PM learns an executor finished without polling.
        let paths = fixture.paths();
        std::fs::create_dir_all(&paths.root).unwrap();
        super::tests::append(
            &notifications_path(&paths),
            &super::tests::notification(Role::Executor, "task-1", "exit 0 in 1.2s"),
        );
        let injected = session.poll_notifications(Instant::now()).unwrap();
        assert_eq!(injected.len(), 1);
        assert!(injected[0].contains("executor finished for task task-1"));
        let received = fixture.wait_for(&fixture.received, "executor finished");
        assert_eq!(received.lines().count(), 2, "{received}");

        // 6. Shutdown reaps the child: `Exited` is only sent after the reader
        // thread has actually reaped it, so seeing it is seeing the reap.
        session.shutdown().unwrap();
        let events = session.drain();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, PmEvent::Exited { .. })),
            "{events:?}"
        );
    }

    #[test]
    fn an_unknown_cli_id_lists_the_ones_the_catalogue_knows() {
        let fixture = Fixture::new();
        // `unwrap_err` would need `Debug` on a live PM session, which is not
        // worth deriving for a type nobody prints.
        let rendered = match prepare_session(&fixture.home, &fixture.project, "nosuchcli") {
            Ok(_) => panic!("a session started for a CLI the catalogue does not know"),
            Err(error) => format!("{error:#}"),
        };
        assert!(rendered.contains("nosuchcli"), "{rendered}");
        assert!(rendered.contains("claude"), "{rendered}");
        assert!(rendered.contains("fixture"), "{rendered}");
    }

    /// The project's state directory is created by launching, not by some
    /// earlier command the user may never have run.
    #[test]
    fn launching_prepares_the_projects_state_directory() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        assert!(!paths.tasks.exists());

        let mut session = prepare_session(&fixture.home, &fixture.project, "fixture").unwrap();
        assert!(paths.tasks.is_dir());
        assert!(paths.logs.is_dir());
        assert!(paths.reviews.is_dir());
        session.shutdown().unwrap();
    }

    /// Nobody touches the keyboard: what the loop faces while the user reads.
    /// It gives up after a deadline so a test can never hang on a loop that
    /// was supposed to end by itself.
    struct Idle {
        deadline: Instant,
    }

    impl EventSource for Idle {
        fn poll_key(&mut self, timeout: Duration) -> Result<Option<crossterm::event::KeyEvent>> {
            if Instant::now() > self.deadline {
                bail!("the loop never ended on its own");
            }
            std::thread::sleep(timeout);
            Ok(None)
        }
    }

    /// A dead PM must not take the interface down with it: the user typed
    /// that turn, and the explanation belongs where they are looking.
    #[test]
    fn a_turn_that_cannot_be_delivered_is_reported_in_the_chat_pane() {
        let fixture = Fixture::new();
        let mut session = prepare_session(&fixture.home, &fixture.project, "fixture").unwrap();
        session.shutdown().unwrap();

        let mut app = App::new(&session.cli_name);
        app.input = "are you there".to_string();
        assert!(
            apply_app_event(AppEvent::Enter, &mut app, &mut session),
            "a failed turn ended the session"
        );

        let lines: Vec<&str> = app.lines().map(|line| line.text.as_str()).collect();
        assert_eq!(lines[0], "are you there");
        assert!(lines[1].contains("did not reach the PM"), "{:?}", lines);
    }

    /// A PM that dies on its own must end the loop rather than leave mana
    /// waiting on a process that is gone -- the v1 zombie, from the TUI side.
    #[test]
    fn the_loop_ends_when_the_pm_exits_and_keeps_what_it_said() {
        use ratatui::backend::TestBackend;

        let fixture = Fixture::new();
        let dying = fixture.home.join("dying-pm");
        // It reads one line before dying, so the activation turn always lands
        // in a live pipe: a PM that exited before mana wrote to it would fail
        // the launch instead, which is a different (and correctly reported)
        // story than the one this test is about.
        std::fs::write(
            &dying,
            "#!/bin/sh\necho 'boom: no credentials found' >&2\nhead -n 1 > /dev/null\nexit 7\n",
        )
        .unwrap();
        std::fs::set_permissions(&dying, std::fs::Permissions::from_mode(0o755)).unwrap();
        fixture.write_override(&dying.to_string_lossy());

        let mut session = prepare_session(&fixture.home, &fixture.project, "fixture").unwrap();
        let mut app = App::new(&session.cli_name);
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let mut events = Idle {
            deadline: Instant::now() + Duration::from_secs(10),
        };

        let end = run_loop(
            &mut terminal,
            &mut session,
            &mut app,
            &mut GraphCache::new(),
            &mut events,
        )
        .unwrap();

        assert_eq!(end, SessionEnd::PmExited { code: Some(7) });
        // The reason is kept for the message printed after the TUI is gone.
        assert_eq!(app.last_raw.as_deref(), Some("boom: no credentials found"));
    }

    #[test]
    fn typing_a_turn_sends_it_to_the_pm_and_ctrl_c_quits() {
        use crate::tui::event::test_support::FakeEventSource;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::backend::TestBackend;

        let fixture = Fixture::new();
        let mut session = prepare_session(&fixture.home, &fixture.project, "fixture").unwrap();
        let mut app = App::new(&session.cli_name);
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let mut events = FakeEventSource::new([
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            // Escape used to quit here; now it does nothing at all.
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ]);

        let end = run_loop(
            &mut terminal,
            &mut session,
            &mut app,
            &mut GraphCache::new(),
            &mut events,
        )
        .unwrap();
        assert_eq!(end, SessionEnd::UserQuit);

        let received = fixture.wait_for(&fixture.received, "\"ho\"");
        let sent: Vec<serde_json::Value> = received
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(sent[0]["message"]["content"], ACTIVATION);
        assert_eq!(sent[1]["message"]["content"], "ho");

        // The keys after Enter went where they should: Escape did nothing at
        // all (the session survived it), Ctrl+G opened the graph pane.
        assert_eq!(app.mode, crate::tui::app::AppMode::Graph);
        // ...and the user's own turn is in the transcript, not only the PM's.
        assert!(
            app.lines().any(|line| line.text == "ho"),
            "the typed turn was never echoed"
        );
        session.shutdown().unwrap();
    }
}
