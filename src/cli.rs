use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::error::Error;
use crate::id::{Provider, parse_effort};
use crate::job::{self, NewJob};
use crate::tier::{self, Tier};

/// One CLI over the agent runners, named jobs, machine load, and credit usage.
#[derive(Debug, Parser)]
#[command(name = "crew", version, arg_required_else_help = true)]
pub struct Cli {
    /// Internal. Re-exec'd by `crew job run` as the detached supervisor.
    #[arg(long, hide = true)]
    supervise: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Claude(ProviderArgs),
    #[command(alias = "antigravity", alias = "agy")]
    Gemini(GeminiArgs),
    Grok(GrokArgs),
    #[command(alias = "gpt", alias = "chatgpt")]
    Codex(CodexArgs),
    Job(JobArgs),
    Load,
    Usage,
    Review(ReviewArgs),
    Models,
}

#[derive(Debug, Args)]
struct ProviderArgs {
    #[command(flatten)]
    shared: SharedArgs,
    #[command(flatten)]
    dirs: AddDirArgs,
    #[arg(long)]
    schema: Option<String>,
    #[arg(long)]
    raw: bool,
    #[arg(long)]
    no_web: bool,
}

#[derive(Debug, Args)]
struct GeminiArgs {
    #[command(flatten)]
    shared: SharedArgs,
    #[command(flatten)]
    dirs: AddDirArgs,
    #[arg(long)]
    agent: Option<String>,
    #[arg(long)]
    sandbox: bool,
}

#[derive(Debug, Args)]
struct GrokArgs {
    #[command(flatten)]
    shared: SharedArgs,
    #[arg(long)]
    schema: Option<String>,
    #[arg(long)]
    raw: bool,
    #[arg(long)]
    no_web: bool,
    #[arg(short = 'n', long = "max-turns")]
    max_turns: Option<u32>,
}

#[derive(Debug, Args)]
struct CodexArgs {
    #[command(flatten)]
    shared: SharedArgs,
    #[command(flatten)]
    dirs: AddDirArgs,
    #[arg(long)]
    schema: Option<String>,
    #[arg(long)]
    raw: bool,
    #[arg(long)]
    review: bool,
    #[arg(long)]
    uncommitted: bool,
    #[arg(long)]
    base: Option<String>,
    #[arg(long)]
    commit: Option<String>,
}

#[derive(Debug, Args)]
struct SharedArgs {
    #[arg(long, value_enum)]
    tier: Option<Tier>,
    #[arg(short = 'm', long)]
    model: Option<String>,
    #[arg(short = 'e', long)]
    effort: Option<String>,
    #[arg(long, conflicts_with_all = ["write", "auto"])]
    read_only: bool,
    #[arg(long, conflicts_with_all = ["read_only", "auto"])]
    write: bool,
    /// Accepted and ignored. Omitting the mode flags is already full autonomy.
    #[arg(long, conflicts_with_all = ["read_only", "write"])]
    auto: bool,
    #[arg(short = 't', long, default_value_t = 300.0)]
    timeout: f64,
    #[arg(short = 'C', long)]
    cwd: Option<PathBuf>,
    #[arg(short = 'r', long)]
    resume: Option<String>,
    #[arg(short = 'c', long = "continue")]
    continue_session: bool,
    #[arg(long = "print-id")]
    print_id: bool,
    /// Prompt text, or `-` to read stdin. Omitted means the same as `-`.
    prompt: Option<String>,
}

#[derive(Debug, Args, Default)]
struct AddDirArgs {
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
    /// Detach a provider run under a durable name.
    Run(JobRunArgs),
    Ls {
        #[arg(long)]
        json: bool,
    },
    Show {
        name: String,
        #[arg(long)]
        json: bool,
    },
    Result {
        name: String,
    },
    Log {
        name: String,
        #[arg(short = 'n', default_value_t = 20)]
        lines: usize,
    },
    Wait {
        names: Vec<String>,
        #[arg(long)]
        any: bool,
        #[arg(long)]
        timeout: Option<u64>,
    },
    Resume {
        name: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        prompt: Vec<String>,
    },
    Stop {
        name: String,
    },
}

#[derive(Debug, Args)]
struct JobRunArgs {
    #[arg(long)]
    note: Option<String>,
    name: String,
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
    Write {
        model: String,
        task: String,
        what: String,
        duration: String,
        cost: String,
        score: String,
        reviewer: String,
    },
    Query {
        harness: String,
    },
    Pick {
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
}
