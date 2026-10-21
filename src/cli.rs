use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::error::Error;
use crate::id::{Provider, parse_effort};
use crate::job::{self, NewJob};
use crate::tier::{self, Tier};

/// One CLI over the agent runners, named jobs, machine load, and credit usage.
#[derive(Debug, Parser)]
#[command(
    name = "crew",
    version,
    about = "Run agent CLIs, named jobs, machine load, and credit usage",
    long_about = "crew is one binary over claude, gemini, grok, and codex.\n\
        \n\
        Start with `crew models` for the tier table, then \
        `crew <provider> --tier fast \"...\"`. Leaving out --read-only and \
        --write is full autonomy. --auto is that same mode, written out.\n\
        \n\
        A run longer than 20 minutes prints one stderr line. Rate it later \
        with `crew review write`. crew does not file the review itself.\n\
        \n\
        `crew help <command>` and `<command> --help` print the same text.\n\
        \n\
        Agents\n  \
        claude\n  \
        gemini    aliases: agy, antigravity\n  \
        grok\n  \
        codex     aliases: gpt, chatgpt\n\
        \n\
        Jobs\n  \
        job       run, ls, show, result, log, wait, resume, stop\n\
        \n\
        Machine\n  \
        load      CPU, memory, GPU, and running agents\n  \
        usage     plans, bars, API cost\n\
        \n\
        Config\n  \
        review    rating log and model picker\n  \
        models    the tier table",
    after_long_help = "Examples:\n  \
        crew models\n  \
        crew grok --tier fast \"Reply with the single word pong\"\n  \
        crew job run demo grok --tier fast \"Reply with the single word pong\"\n  \
        crew job wait demo\n  \
        crew load\n  \
        crew usage",
    arg_required_else_help = true,
    disable_help_subcommand = false
)]
pub struct Cli {
    /// Internal. Re-exec'd by `crew job run` as the detached supervisor.
    #[arg(long, hide = true)]
    supervise: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(
        about = "Run Claude Code",
        long_about = "Run the claude CLI. Use this for a foreground task. \
            For a detached run, use `crew job run <name> claude ...` instead.",
        after_long_help = "Examples:\n  \
            crew claude --tier fast \"Reply with the single word pong\"\n  \
            crew claude -m fable -e max \"Read the plan and list the risks\"\n  \
            crew claude --read-only -C ~/Projects/crew \"What does src/tier.rs decide?\""
    )]
    Claude(ProviderArgs),
    #[command(
        about = "Run the Gemini CLI (agy)",
        long_about = "Run agy. Aliases: antigravity, agy. Effort is baked into \
            the model slug (gemini-3.8-flash-low, and so on). crew does not also \
            pass --effort. A deadline kills the process group and drops the \
            partial transcript.",
        after_long_help = "Examples:\n  \
            crew gemini --tier fast \"Reply with the single word pong\"\n  \
            crew agy --read-only \"Summarize src/cli.rs\"\n  \
            crew gemini -m gemini-3.8-flash-high \"Look at the last commit\"",
        visible_alias = "antigravity",
        visible_alias = "agy"
    )]
    Gemini(GeminiArgs),
    #[command(
        about = "Run the Grok CLI",
        long_about = "Run grok. A tier only selects the model (grok-4.7). \
            It does not select effort. -e does, and the default is medium. \
            `crew models` prints '-' in the grok effort column.",
        after_long_help = "Examples:\n  \
            crew grok --tier fast \"Reply with the single word pong\"\n  \
            crew grok --tier max -e high \"Review the diff\"\n  \
            crew grok --read-only -n 4 \"List the files in src/\""
    )]
    Grok(GrokArgs),
    #[command(
        about = "Run Codex",
        long_about = "Run codex. Aliases: gpt, chatgpt. Default effort is \
            medium. User-facing max is sent as ultra, except on gpt-6-luna, \
            which stays max. --review with a scope (--uncommitted, --base, \
            or --commit) does not take a prompt.",
        after_long_help = "Examples:\n  \
            crew codex --tier fast \"Reply with the single word pong\"\n  \
            crew codex --review --uncommitted\n  \
            crew codex --read-only -m gpt-6-luna \"Explain src/tier.rs\"",
        visible_alias = "gpt",
        visible_alias = "chatgpt"
    )]
    Codex(CodexArgs),
    #[command(
        about = "Run, list, and resume named background jobs",
        long_about = "Detach a provider run under a name and pick it up later. \
            The store is $CREW_HOME or ~/.local/share/crew. Status is derived \
            from the process and the recorded exit, not stored as a field.",
        after_long_help = "Examples:\n  \
            crew job run demo grok --tier fast \"Reply with the single word pong\"\n  \
            crew job ls\n  \
            crew job wait demo\n  \
            crew job result demo"
    )]
    Job(JobArgs),
    #[command(
        about = "Show CPU, memory, GPU, and running agent processes",
        long_about = "Same lines as subagentcounter, plus a crew: line. \
            macOS only for the GPU figure. Shells whose first argument is -c \
            are not counted, and neither is this process's own tree.",
        after_long_help = "Examples:\n  crew load"
    )]
    Load,
    #[command(
        about = "Show plan, quota bars, and API-equivalent cost",
        long_about = "Bars come from a Claude /usage probe, agy /usage, the Grok \
            billing API, and the Codex usage API. A reset is printed only when \
            that payload includes one. Bars are sorted by the least-used window.\n\
            \n\
            The plan name is organizationType in ~/.claude.json (the same value \
            as `claude auth status`), chatgpt_plan_type in the local Codex login, \
            and subscription_tier_display in ~/.grok/settings_cache.json. \
            Antigravity reports no plan. ~/.config/crew/plans.toml, or CREW_PLANS, \
            fills a gap and the line says \"(from config)\". The antigravity row \
            is [gemini]. A price with no such note is the public monthly list \
            price of that exact plan, not the invoice.\n\
            \n\
            The api figure prices this machine's Claude Code and Codex session \
            logs over the quota window, or the last 7 days when that window's \
            start is unknown. Prices are LiteLLM's \
            model_prices_and_context_window.json, cached under \
            ~/.local/share/crew and reused offline. Tokens whose model is missing \
            from that table are left out. Grok and Antigravity write no \
            input/output ledger, so they have no api line. The figure is every \
            local session of that CLI, not only crew runs.",
        after_long_help = "Examples:\n  crew usage"
    )]
    Usage,
    #[command(
        about = "Record and query ratings of finished runs",
        long_about = "The log is ~/.local/share/reviewsubagent/reviews.jsonl, \
            capped at 30. crew does not write a row when a run finishes. \
            The orchestrator does that later with `crew review write`.",
        after_long_help = "Examples:\n  \
            crew review query all\n  \
            crew review pick \"fix the date parser\"\n  \
            crew review write grok-4.7-medium \"pong\" \"replied pong\" 12s - 80 grok"
    )]
    Review(ReviewArgs),
    #[command(
        about = "Print the tier table",
        long_about = "Print provider, tier, model, effort, and whether the \
            row is the built-in table or ~/.config/crew/tiers.toml. \
            CREW_TIERS overrides that path. A user file replaces the whole table. \
            Grok effort is '-', because -e selects it.",
        after_long_help = "Examples:\n  crew models"
    )]
    Models,
}

#[derive(Debug, Args)]
struct ProviderArgs {
    #[command(flatten)]
    shared: SharedArgs,
    #[command(flatten)]
    dirs: AddDirArgs,
    /// JSON schema. `{...}` is the schema text. Anything else is a file path, with `~` expanded.
    #[arg(long)]
    schema: Option<String>,
    /// Print the CLI's own output and skip crew's success check.
    #[arg(long)]
    raw: bool,
    /// Drop web search and web fetch from the tool list.
    #[arg(long)]
    no_web: bool,
}

#[derive(Debug, Args)]
struct GeminiArgs {
    #[command(flatten)]
    shared: SharedArgs,
    #[command(flatten)]
    dirs: AddDirArgs,
    /// Named agy agent. With no -m, the tier model is not sent.
    #[arg(long)]
    agent: Option<String>,
    /// Pass agy's --sandbox.
    #[arg(long)]
    sandbox: bool,
}

#[derive(Debug, Args)]
struct GrokArgs {
    #[command(flatten)]
    shared: SharedArgs,
    /// JSON schema. `{...}` is the schema text. Anything else is a file path, with `~` expanded.
    #[arg(long)]
    schema: Option<String>,
    /// Print grok's own output and skip the stopReason check.
    #[arg(long)]
    raw: bool,
    /// Pass --disable-web-search, and drop web tools from a read-only fallback.
    #[arg(long)]
    no_web: bool,
    /// Stop after this many turns. Maps to grok --max-turns.
    #[arg(short = 'n', long = "max-turns")]
    max_turns: Option<u32>,
}

#[derive(Debug, Args)]
struct CodexArgs {
    #[command(flatten)]
    shared: SharedArgs,
    #[command(flatten)]
    dirs: AddDirArgs,
    /// JSON schema passed through to codex. Not read as a file.
    #[arg(long)]
    schema: Option<String>,
    /// Print codex's own output and skip the turn.failed check.
    #[arg(long)]
    raw: bool,
    /// Run `codex review` instead of a normal turn.
    #[arg(long)]
    review: bool,
    /// Review scope: uncommitted changes. Does not take a prompt. Requires --review.
    #[arg(long)]
    uncommitted: bool,
    /// Review scope: changes against this base branch. Requires --review.
    #[arg(long)]
    base: Option<String>,
    /// Review scope: this commit. Requires --review.
    #[arg(long)]
    commit: Option<String>,
}

#[derive(Debug, Args)]
struct SharedArgs {
    /// fast, balanced, smart, or max. See `crew models`. -m overrides the row's model. A grok tier does not set effort.
    #[arg(long, value_enum)]
    tier: Option<Tier>,
    /// Override the tier model. Passed through. Claude short names: haiku, sonnet, opus, fable.
    #[arg(short = 'm', long)]
    model: Option<String>,
    /// low, medium, high, or max. Default medium. With a tier, the row's effort wins except on grok, which stays medium unless you pass -e. max is sent as max (claude), ultra (codex, except gpt-6-luna stays max), high baked into the gemini slug, xhigh (grok; high on grok-4.5).
    #[arg(short = 'e', long)]
    effort: Option<String>,
    /// Read-only tools. Claude uses its read-only permission mode and a system rule. Gemini omits the skip-permissions flag and appends a read-only sentence to the prompt. Grok tries the read-only sandbox once, then a read-only tool list, and passes a rule. Codex uses its read-only sandbox and appends the same sentence. The sentence is skipped when --schema is set. It is still sent with --raw, and it is not stored on a job.
    #[arg(long, conflicts_with_all = ["write", "auto"])]
    read_only: bool,
    /// Allow edits. Claude: accept-edits style permissions. Gemini: --mode accept-edits. Grok: --sandbox workspace. Codex: its write sandbox.
    #[arg(long, conflicts_with_all = ["read_only", "auto"])]
    write: bool,
    /// Accepted and ignored. Omitting the mode flags is already full autonomy.
    #[arg(long, conflicts_with_all = ["read_only", "write"])]
    auto: bool,
    /// Seconds before the process group is killed. Default 1800. Must be finite and > 0. Codex is SIGKILL immediately. The others get SIGTERM, then SIGKILL after 5 seconds, and the partial output is dropped.
    #[arg(short = 't', long, default_value_t = 1800.0)]
    timeout: f64,
    /// Working directory for the child. Maps to the CLI's own cwd flag where it has one.
    #[arg(short = 'C', long)]
    cwd: Option<PathBuf>,
    /// Resume this session id. Claude and Codex reject -r together with -c. Grok keeps -r and drops -c. Gemini forwards both.
    #[arg(short = 'r', long)]
    resume: Option<String>,
    /// Resume the most recent session. Claude and Codex reject this together with -r. Grok ignores it when -r is set. Gemini forwards it.
    #[arg(short = 'c', long = "continue")]
    continue_session: bool,
    /// Write SESSION_ID=<id> to stderr when the CLI reports one.
    #[arg(long = "print-id")]
    print_id: bool,
    /// Prompt text, or `-` to read stdin. Omitted means the same as `-`. `-` on a terminal is an error. The prompt is an argument to the child CLI, not the child's stdin.
    prompt: Option<String>,
}

#[derive(Debug, Args, Default)]
struct AddDirArgs {
    /// Extra directory the CLI may read. Repeatable. grok rejects this flag.
    #[arg(long = "add-dir")]
    add_dir: Vec<PathBuf>,
}

/// Parsed provider invocation. Runners turn this into one CLI's argv.
#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    pub provider: Provider,
    pub tier: Option<Tier>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub read_only: bool,
    pub write: bool,
    pub timeout_secs: f64,
    pub cwd: Option<PathBuf>,
    pub resume: Option<String>,
    pub continue_session: bool,
    pub print_id: bool,
    pub prompt: Option<String>,
    pub add_dir: Vec<PathBuf>,
    pub schema: Option<String>,
    pub raw: bool,
    pub no_web: bool,
    pub max_turns: Option<u32>,
    pub agent: Option<String>,
    pub sandbox: bool,
    pub review: bool,
    pub uncommitted: bool,
    pub base: Option<String>,
    pub commit: Option<String>,
}

#[derive(Debug, Args)]
struct JobArgs {
    #[command(subcommand)]
    command: JobCommand,
}

#[derive(Debug, Subcommand)]
enum JobCommand {
    #[command(
        about = "Detach a provider run under a name",
        long_about = "Start `crew <provider> ...` in the background and print the name. \
            The flags after the provider are the same flags as a foreground run.",
        after_long_help = "Examples:\n  \
            crew job run demo grok --tier fast \"Reply with the single word pong\"\n  \
            crew job run audit claude --read-only -C ~/Projects/crew \"List the risky spots\"\n  \
            crew job run --note \"nightly\" rev codex --review --uncommitted"
    )]
    Run(JobRunArgs),
    #[command(
        about = "List jobs, oldest first",
        long_about = "Print name, derived status, provider, model, and age. \
            Status is running, lost, stopped, ok, timeout, unclean, or failed.",
        after_long_help = "Examples:\n  crew job ls\n  crew job ls --json"
    )]
    Ls {
        /// Print one JSON array instead of the table.
        #[arg(long)]
        json: bool,
    },
    #[command(
        about = "Show one job's record and derived status",
        long_about = "Print the stored record. The status line is computed, not stored.",
        after_long_help = "Examples:\n  crew job show demo\n  crew job show demo --json"
    )]
    Show {
        /// Job name.
        #[arg(value_name = "NAME")]
        name: String,
        /// Print the record as JSON, including the derived status.
        #[arg(long)]
        json: bool,
    },
    #[command(
        about = "Print a finished job's stdout",
        long_about = "Exit 2 while the job is running, 3 if the supervisor is gone \
            without a recorded exit. Refuses a symlink. A run longer than 20 minutes \
            also prints the review reminder on stderr.",
        after_long_help = "Examples:\n  crew job result demo"
    )]
    Result {
        /// Job name.
        #[arg(value_name = "NAME")]
        name: String,
    },
    #[command(
        about = "Print the tail of a job's stderr",
        long_about = "Stderr holds the session id line and crew's own notes. Default is 20 lines.",
        after_long_help = "Examples:\n  crew job log demo\n  crew job log demo -n 50"
    )]
    Log {
        /// Job name.
        #[arg(value_name = "NAME")]
        name: String,
        /// How many lines from the end. Default 20.
        #[arg(short = 'n', default_value_t = 20)]
        lines: usize,
    },
    #[command(
        about = "Block until the named jobs finish",
        long_about = "Exit with the job's code. A stopped job returns 130. \
            An unknown name is exit 2. --timeout expiring is exit 4. \
            A run longer than 20 minutes prints the review reminder.",
        after_long_help = "Examples:\n  \
            crew job wait demo\n  \
            crew job wait one two --any\n  \
            crew job wait demo --timeout 600"
    )]
    Wait {
        /// One or more job names.
        #[arg(value_name = "NAME")]
        names: Vec<String>,
        /// Return when the first named job finishes, and print that name.
        #[arg(long)]
        any: bool,
        /// Give up after this many seconds. Exit 4.
        #[arg(long)]
        timeout: Option<u64>,
    },
    #[command(
        about = "Start <name>.2 (then .3, ...) in the parent's directory",
        long_about = "Holds the parent's resume.lock while the child name is chosen. \
            The child gets the parent's flags, -C of the parent's directory, \
            and -r of the parent's session id. Pass a new prompt, or omit it to read stdin.",
        after_long_help = "Examples:\n  \
            crew job resume demo \"Look at the tests you skipped\"\n  \
            crew job resume demo"
    )]
    Resume {
        /// Parent job name.
        #[arg(value_name = "NAME")]
        name: String,
        /// New prompt. Omit it to read stdin. At most one argument.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        prompt: Vec<String>,
    },
    #[command(
        about = "Stop a running job",
        long_about = "SIGTERM the supervisor. It SIGTERMs the child group, waits 10 seconds, \
            then SIGKILL. The stored exit is the raw signal code (143 or 137). \
            `crew job wait` returns 130 for a stopped job.",
        after_long_help = "Examples:\n  crew job stop demo"
    )]
    Stop {
        /// Job name.
        #[arg(value_name = "NAME")]
        name: String,
    },
}

#[derive(Debug, Args)]
struct JobRunArgs {
    /// Free-text note stored on the job. Not sent to the provider.
    #[arg(long)]
    note: Option<String>,
    /// Job name. Must match ^[a-z0-9][a-z0-9._-]{0,63}$.
    #[arg(value_name = "NAME")]
    name: String,
    /// claude, gemini (agy, antigravity), grok, or codex (gpt, chatgpt).
    #[arg(value_name = "PROVIDER")]
    provider: String,
    /// Wrapper flags and the prompt. Passed through to `crew <provider>`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    trailing: Vec<String>,
}

#[derive(Debug, Args)]
struct ReviewArgs {
    #[command(subcommand)]
    command: ReviewCommand,
}

#[derive(Debug, Subcommand)]
enum ReviewCommand {
    #[command(
        about = "Append one rating",
        long_about = "Append a row and keep the newest 30. The model is \
            <id>-<effort>, for example grok-4.7-medium. Duration is seconds \
            or 1h30m15s. Cost is a number, nan, or '-'. Score is an integer 0 through 100.",
        after_long_help = "Examples:\n  \
            crew review write grok-4.7-medium \"pong\" \"replied pong\" 12s - 80 grok\n  \
            crew review write claude-haiku-5-5-medium \"audit\" \"listed three risks\" 1h30m15s 0.05 70 claude"
    )]
    Write {
        /// Model id and effort, joined by a hyphen: grok-4.7-medium.
        #[arg(value_name = "MODEL")]
        model: String,
        /// What the run was asked to do.
        #[arg(value_name = "TASK")]
        task: String,
        /// What it actually did.
        #[arg(value_name = "WHAT")]
        what: String,
        /// Seconds, or a clock like 1h30m15s.
        #[arg(value_name = "DURATION")]
        duration: String,
        /// A number, nan, or '-'.
        #[arg(value_name = "COST")]
        cost: String,
        /// Integer 0 through 100.
        #[arg(value_name = "SCORE")]
        score: String,
        /// Who is writing the rating.
        #[arg(value_name = "REVIEWER")]
        reviewer: String,
    },
    #[command(
        about = "Print the rating table",
        long_about = "Harness is all, codex, claude, agy, or grok. An empty log still prints the header.",
        after_long_help = "Examples:\n  crew review query all\n  crew review query grok"
    )]
    Query {
        /// all, codex, claude, agy, or grok.
        #[arg(value_name = "HARNESS")]
        harness: String,
    },
    #[command(
        about = "Ask a judge which logged model should take a task",
        long_about = "Candidates are models already in the log. The judge is a \
            random one of claude/haiku, gemini/gemini-3.8-flash, or codex/gpt-6-luna, \
            effort high, read-only, 300 seconds. If that judge fails to launch, \
            the other two are tried. An empty log is exit 2.",
        after_long_help = "Examples:\n  crew review pick \"fix the date parser\""
    )]
    Pick {
        /// The task the chosen model would do.
        #[arg(value_name = "TASK")]
        task: String,
    },
}

pub fn run_from_env() -> Result<i32, Error> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let code = err.exit_code();
            err.print().ok();
            return Ok(code);
        }
    };
    dispatch(cli)
}

fn dispatch(cli: Cli) -> Result<i32, Error> {
    if let Some(name) = cli.supervise {
        if cli.command.is_some() {
            return Err(Error::new(2, "--supervise takes no subcommand"));
        }
        return job::supervise(&name);
    }
    match cli.command {
        None => Err(Error::new(2, "missing command")),
        Some(Command::Load) => crate::load::print_load(),
        Some(Command::Usage) => crate::usage::print_usage(),
        Some(Command::Models) => tier::print(),
        Some(Command::Review(args)) => dispatch_review(args),
        Some(Command::Job(args)) => dispatch_job(args),
        Some(Command::Claude(args)) => crate::providers::execute(&invocation_from_claude(args)?),
        Some(Command::Gemini(args)) => crate::providers::execute(&invocation_from_gemini(args)?),
        Some(Command::Grok(args)) => crate::providers::execute(&invocation_from_grok(args)?),
        Some(Command::Codex(args)) => crate::providers::execute(&invocation_from_codex(args)?),
    }
}

fn dispatch_review(args: ReviewArgs) -> Result<i32, Error> {
    match args.command {
        ReviewCommand::Write {
            model,
            task,
            what,
            duration,
            cost,
            score,
            reviewer,
        } => crate::review::write(crate::review::NewReview {
            model,
            task,
            what,
            duration,
            cost,
            score,
            reviewer,
        }),
        ReviewCommand::Query { harness } => crate::review::query(&harness),
        ReviewCommand::Pick { task } => crate::review::pick(&task),
    }
}

fn dispatch_job(args: JobArgs) -> Result<i32, Error> {
    match args.command {
        JobCommand::Run(args) => {
            let provider: Provider = args.provider.parse()?;
            // Reject a trailing command that is not a legal invocation of that provider.
            let _ = parse_trailing(provider, &args.trailing)?;
            job::run(NewJob {
                name: args.name,
                provider,
                note: args.note,
                trailing: args.trailing,
            })
        }
        JobCommand::Ls { json } => job::ls(json),
        JobCommand::Show { name, json } => job::show(&name, json),
        JobCommand::Result { name } => job::result(&name),
        JobCommand::Log { name, lines } => job::log(&name, lines),
        JobCommand::Wait {
            names,
            any,
            timeout,
        } => {
            if names.is_empty() {
                return Err(Error::new(2, "job wait needs at least one name"));
            }
            job::wait(&names, any, timeout)
        }
        JobCommand::Resume { name, prompt } => {
            let prompt = match prompt.as_slice() {
                [] => None,
                [one] => Some(one.clone()),
                _ => {
                    return Err(Error::new(2, "job resume takes at most one prompt"));
                }
            };
            job::resume(&name, prompt)
        }
        JobCommand::Stop { name } => job::stop(&name),
    }
}

/// Parse the trailing words of `crew job run` as `crew <provider> ...`.
pub fn parse_trailing(provider: Provider, trailing: &[String]) -> Result<Invocation, Error> {
    let mut argv = vec!["crew".to_string(), provider.as_str().to_string()];
    argv.extend(trailing.iter().cloned());
    let cli = Cli::try_parse_from(argv).map_err(|err| Error::new(2, err.to_string()))?;
    match (provider, cli.command) {
        (Provider::Claude, Some(Command::Claude(args))) => invocation_from_claude(args),
        (Provider::Gemini, Some(Command::Gemini(args))) => invocation_from_gemini(args),
        (Provider::Grok, Some(Command::Grok(args))) => invocation_from_grok(args),
        (Provider::Codex, Some(Command::Codex(args))) => invocation_from_codex(args),
        _ => Err(Error::new(
            2,
            "job run trailing args are not a provider invocation",
        )),
    }
}

fn finish(
    provider: Provider,
    shared: SharedArgs,
    add_dir: Vec<PathBuf>,
    extra: Extra,
) -> Result<Invocation, Error> {
    if !shared.timeout.is_finite() || shared.timeout <= 0.0 {
        return Err(Error::new(2, "timeout must be a positive finite number"));
    }
    if let Some(effort) = &shared.effort {
        parse_effort(effort)?;
    }
    // Claude and codex reject both. Grok keeps -r and drops -c. Gemini forwards both.
    if shared.resume.is_some()
        && shared.continue_session
        && matches!(provider, Provider::Claude | Provider::Codex)
    {
        return Err(Error::new(2, "pass -r or -c, not both"));
    }
    let review_scope = extra.uncommitted || extra.base.is_some() || extra.commit.is_some();
    if review_scope && !extra.review {
        return Err(Error::new(2, "review scope requires --review"));
    }
    if extra.review && (shared.resume.is_some() || shared.continue_session) {
        return Err(Error::new(2, "--review cannot be combined with -r or -c"));
    }
    if extra.review && review_scope && shared.prompt.is_some() {
        return Err(Error::new(2, "choose a review scope or a custom prompt"));
    }
    Ok(Invocation {
        provider,
        tier: shared.tier,
        model: shared.model,
        effort: shared.effort,
        read_only: shared.read_only,
        write: shared.write,
        timeout_secs: shared.timeout,
        cwd: shared.cwd,
        resume: shared.resume,
        continue_session: shared.continue_session,
        print_id: shared.print_id,
        prompt: shared.prompt,
        add_dir,
        schema: extra.schema,
        raw: extra.raw,
        no_web: extra.no_web,
        max_turns: extra.max_turns,
        agent: extra.agent,
        sandbox: extra.sandbox,
        review: extra.review,
        uncommitted: extra.uncommitted,
        base: extra.base,
        commit: extra.commit,
    })
}

#[derive(Default)]
struct Extra {
    schema: Option<String>,
    raw: bool,
    no_web: bool,
    max_turns: Option<u32>,
    agent: Option<String>,
    sandbox: bool,
    review: bool,
    uncommitted: bool,
    base: Option<String>,
    commit: Option<String>,
}

fn invocation_from_claude(args: ProviderArgs) -> Result<Invocation, Error> {
    let dirs = args.dirs.add_dir;
    finish(
        Provider::Claude,
        args.shared,
        dirs,
        Extra {
            schema: args.schema,
            raw: args.raw,
            no_web: args.no_web,
            ..Extra::default()
        },
    )
}

fn invocation_from_gemini(args: GeminiArgs) -> Result<Invocation, Error> {
    let dirs = args.dirs.add_dir;
    finish(
        Provider::Gemini,
        args.shared,
        dirs,
        Extra {
            agent: args.agent,
            sandbox: args.sandbox,
            ..Extra::default()
        },
    )
}

fn invocation_from_grok(args: GrokArgs) -> Result<Invocation, Error> {
    finish(
        Provider::Grok,
        args.shared,
        Vec::new(),
        Extra {
            schema: args.schema,
            raw: args.raw,
            no_web: args.no_web,
            max_turns: args.max_turns,
            ..Extra::default()
        },
    )
}

fn invocation_from_codex(args: CodexArgs) -> Result<Invocation, Error> {
    let dirs = args.dirs.add_dir;
    finish(
        Provider::Codex,
        args.shared,
        dirs,
        Extra {
            schema: args.schema,
            raw: args.raw,
            review: args.review,
            uncommitted: args.uncommitted,
            base: args.base,
            commit: args.commit,
            ..Extra::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(args: &[&str]) -> Result<Invocation, Error> {
        let trailing: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        let provider = if args.first().copied() == Some("codex") {
            Provider::Codex
        } else if args.first().copied() == Some("grok") {
            Provider::Grok
        } else if args.first().copied() == Some("gemini") || args.first().copied() == Some("agy") {
            Provider::Gemini
        } else {
            Provider::Claude
        };
        let rest = trailing.get(1..).unwrap_or(&[]).to_vec();
        parse_trailing(provider, &rest)
    }

    #[test]
    fn grok_rejects_add_dir() {
        let err = parse(&["grok", "--add-dir", "/tmp", "hi"]).unwrap_err();
        assert_eq!(err.code, 2);
    }

    #[test]
    fn claude_rejects_both_resume_flags() {
        let err = parse(&["claude", "-r", "abc", "-c", "hi"]).unwrap_err();
        assert_eq!(err.code, 2);
    }

    #[test]
    fn gemini_keeps_both_resume_flags() {
        let inv = parse(&["gemini", "-r", "abc", "-c", "hi"]).unwrap();
        assert!(inv.continue_session);
        assert_eq!(inv.resume.as_deref(), Some("abc"));
    }

    #[test]
    fn codex_review_scope_without_a_prompt_parses() {
        let inv = parse(&["codex", "--review", "--uncommitted"]).unwrap();
        assert!(inv.review);
        assert!(inv.uncommitted);
        assert_eq!(inv.prompt, None);
    }

    #[test]
    fn codex_review_scope_and_a_prompt_conflict() {
        let err = parse(&["codex", "--review", "--uncommitted", "hello"]).unwrap_err();
        assert_eq!(err.code, 2);
    }

    #[test]
    fn timeout_must_be_positive() {
        let err = parse(&["claude", "-t", "0", "hi"]).unwrap_err();
        assert_eq!(err.code, 2);
    }

    /// Help is the interface. A blank about or a blank option is a regression,
    /// including on commands added later.
    #[test]
    fn every_command_and_option_has_help() {
        fn assert_documented(cmd: &clap::Command) {
            let name = cmd.get_name().to_string();
            // clap's own `help` subcommand has an about and no long_about.
            let builtin_help = name == "help";
            let about = cmd.get_about().map(|s| s.to_string()).unwrap_or_default();
            assert!(!about.is_empty(), "{name} about");
            if !builtin_help {
                let long_about = cmd
                    .get_long_about()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                assert!(!long_about.is_empty(), "{name} long_about");
                let examples = cmd
                    .get_after_long_help()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                assert!(!examples.is_empty(), "{name} examples");
            }
            for arg in cmd.get_arguments() {
                let id = arg.get_id().as_str();
                if id == "help" || id == "version" {
                    continue;
                }
                let help = arg.get_help().map(|s| s.to_string()).unwrap_or_default();
                assert!(!help.is_empty(), "{name} option {id}");
            }
            for sub in cmd.get_subcommands() {
                assert_documented(sub);
            }
        }

        assert_documented(&Cli::command());
    }
}
