use serde::Deserialize;
use std::collections::BTreeMap;

/// One CLI's catalogue entry -- the whole of what mana is allowed to know
/// about a CLI. Every struct below carries `deny_unknown_fields` on purpose:
/// the catalogue is hand-edited (by maintainers here, by users in the local
/// override), and a typo that serde quietly dropped would surface much later
/// as a mysteriously missing flag instead of a parse error.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CliEntry {
    /// Bumped only when the shape changes in a way older binaries cannot read.
    /// It exists because outside humans edit these files; the internal
    /// PM/tool contract deliberately has no version.
    pub schema: u32,
    /// Free text for maintainers -- provenance of a flag, a measured pitfall,
    /// what is still unverified. Never injected into any agent's context.
    #[serde(default)]
    pub notes: String,
    pub cli: CliMeta,
    pub pm: Pm,
    pub tools: Tools,
    pub subagent: Subagent,
    pub models: Models,
    /// A CLI whose quota model is entirely unknown simply declares no pool.
    #[serde(default)]
    pub quota: Quota,
    /// Ordered: the first signature that matches a failed run wins, so the
    /// file's order is load-bearing. TOML arrays of tables preserve it.
    #[serde(default)]
    pub failure: Vec<Failure>,
    pub skills: Skills,
    pub install: Install,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CliMeta {
    /// Stable key used everywhere else (routing, logs, override matching).
    pub id: String,
    /// Product name for display. Frequently unrelated to the binary name, so
    /// neither can be derived from the other.
    pub name: String,
    pub bin: String,
    pub version_args: Vec<String>,
    /// Keyed by `std::env::consts::OS` ("windows", "macos", "linux"), for the
    /// CLIs that ship under a different binary name on some platform.
    #[serde(default)]
    pub bin_overrides: BTreeMap<String, String>,
}

impl CliMeta {
    /// The binary name to spawn on the machine we are running on. Kept here
    /// rather than at the call sites so no caller has to remember that
    /// per-OS overrides exist.
    pub fn bin(&self) -> &str {
        self.bin_overrides
            .get(std::env::consts::OS)
            .map(String::as_str)
            .unwrap_or(&self.bin)
    }
}

/// How mana drives an interactive PM session. One of a small closed set of
/// generic transports -- the catalogue picks which one and supplies its argv,
/// so a new CLI is data and only a genuinely new protocol shape is new code.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PmDriver {
    Acp,
    Stream,
    OneshotContinue,
}

/// How a prompt reaches the process: as an argv element, on stdin, or as a
/// JSONL frame on stdin.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PromptMode {
    Argv,
    Stdin,
    StdinJsonl,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Pm {
    pub driver: PmDriver,
    /// Used by the long-lived drivers (`acp`, `stream`). Left empty by
    /// `oneshot-continue`, which spawns a fresh process per turn instead.
    #[serde(default)]
    pub args: Vec<String>,
    /// Argv that takes power away from the PM, appended after `args` at
    /// launch. The PM plans and delegates and must be *mechanically* unable
    /// to write code (design §6): a constraint the model merely reads is one
    /// it can talk itself out of on a long session, and every line it writes
    /// itself is expensive quota spent where cheap quota was the whole point.
    /// Empty where the CLI has no such flag -- the skill text says it, and
    /// that is all mana can do there.
    #[serde(default)]
    pub permission_args: Vec<String>,
    /// `oneshot-continue` only: the first turn of a session.
    pub first_args: Option<Vec<String>>,
    /// `oneshot-continue` only: every turn after the first.
    pub continue_args: Option<Vec<String>>,
    /// How a turn reaches the process. Absent for `acp`, where the protocol
    /// carries the prompt as typed content and every value of this field would
    /// be a lie -- the same reason `events` is absent there.
    pub prompt: Option<PromptMode>,
    /// Absent for `acp`, which carries its own typed notifications.
    pub events: Option<PmEvents>,
}

/// Deliberately two fields and no more. Parsing a CLI's full proprietary
/// stream is what forces per-CLI code (the lesson vibe-kanban paid for), so
/// tool calls, permissions and session control travel over ACP, MCP or the
/// sentinel channel instead -- never over this map.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PmEvents {
    /// What to display in the chat pane.
    pub text: String,
    /// Optional enrichment (token counts, cost) when the CLI reports it.
    pub usage: Option<String>,
}

/// How the PM reaches mana's orchestration tools.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolChannel {
    Mcp,
    Sentinel,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Tools {
    pub channel: ToolChannel,
    /// Argv template attaching mana's MCP server. Empty for `sentinel`, which
    /// needs no flags at all. Flag-file and flag-json styles are just
    /// different templates, which is why this is an array and not a bool.
    #[serde(default)]
    pub mcp_args: Vec<String>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Subagent {
    /// Headless invocation; sub-agents speak no protocol, they are spawned
    /// with a prompt and observed from the outside.
    pub args: Vec<String>,
    /// Sub-agents run unattended, so an interactive permission prompt would
    /// hang them forever. Empty means the CLI has no such flag.
    #[serde(default)]
    pub auto_approve_args: Vec<String>,
    #[serde(default)]
    pub model_args: Vec<String>,
    pub prompt: PromptMode,
    /// 0 = unlimited. Required rather than defaulted: assuming a CLI can take
    /// parallel dispatches is how two of them died in 8 seconds.
    pub max_concurrent: u32,
    /// Some CLIs ignore the working directory they were spawned in unless the
    /// brief spells out absolute paths. Costly to rediscover (306k tokens
    /// once), so it is recorded.
    pub cwd_required_in_brief: bool,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Models {
    /// Empty means the static list is all there is. When set, mana runs the
    /// CLI with these args and reads model ids off the output -- which keeps
    /// half the catalogue from rotting on someone else's release schedule.
    pub discovery_args: Vec<String>,
    /// Applied per output line, capture group 1 = the model id. Kept as an
    /// opaque string: nothing compiles regexes yet.
    pub line_regex: Option<String>,
    /// `static` is a Rust keyword, hence the rename.
    #[serde(rename = "static", default)]
    pub static_models: Vec<StaticModel>,
}

/// Ordinal only. No absolute prices and no quality ranking: both rot on a
/// vendor's release schedule, and routing only ever needs "cheapest first,
/// escalate on failure".
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum CostClass {
    Cheap,
    Mid,
    Expensive,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StaticModel {
    pub id: String,
    pub cost_class: CostClass,
    /// Id of one of this entry's `[[quota.pools]]`.
    pub pool: String,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Quota {
    #[serde(default)]
    pub pools: Vec<QuotaPool>,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum QuotaKind {
    Requests,
    Tokens,
    Credits,
    Unknown,
}

/// Whether exhausting the pool takes down every model behind it or only the
/// one that hit the wall -- it decides how wide a cooldown reaches.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PoolScope {
    Global,
    PerModel,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct QuotaPool {
    pub id: String,
    pub kind: QuotaKind,
    /// Free text ("5h-window", "monthly", "unknown"): no CLI exposes a
    /// machine-readable reset schedule, so this is a maintainer's note that
    /// happens to be structured.
    pub period: String,
    pub pool_scope: PoolScope,
}

/// What a matched failure means for routing.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureMeans {
    QuotaExhausted,
    RateLimited,
    AuthExpired,
}

/// Quota state cannot be queried anywhere -- every CLI was probed and none
/// offers it -- so exhaustion is recognised after the fact, from the exit code
/// and the output of a run that already failed.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub means: FailureMeans,
    pub exit_codes: Option<Vec<i32>>,
    /// Opaque strings for now; matching lands with the sub-agent observer.
    pub stderr_regex: Option<String>,
    pub stdout_regex: Option<String>,
    /// How long to rest the pool once this signature matches.
    pub cooldown_minutes: Option<u32>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Skills {
    /// Where this CLI reads SKILL.md from, most-specific first. mana writes
    /// the PM skill to the first one that exists, which is what makes role
    /// injection one code path for every CLI.
    pub dirs: Vec<String>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Install {
    /// Shown when the CLI is missing; mana never installs anything itself.
    pub url: String,
}
