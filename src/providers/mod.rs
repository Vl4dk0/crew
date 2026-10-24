mod claude;
mod codex;
mod gemini;
mod grok;
mod proc;
mod pty;

use crate::cli::Invocation;
use crate::error::Error;
use crate::id::Provider;

/// Run one provider in the foreground.
///
/// Each provider file owns that CLI's argv and its success predicate.
/// The dispatch stays here so a new flag does not fork four call sites.
pub fn execute(invocation: &Invocation) -> Result<i32, Error> {
    match invocation.provider {
        Provider::Claude => claude::run(invocation),
        Provider::Gemini => gemini::run(invocation),
        Provider::Grok => grok::run(invocation),
        Provider::Codex => codex::run(invocation),
    }
}

/// Model argument the provider CLI actually receives.
///
/// This is the slug after tier resolution and any provider rewrite (gemini
/// bakes effort into the slug). None means that CLI is started with no model
/// flag, so the account default stands and crew does not know the name.
pub fn cli_model(invocation: &Invocation) -> Result<Option<String>, Error> {
    match invocation.provider {
        Provider::Claude => claude::cli_model(invocation),
        Provider::Gemini => gemini::cli_model(invocation),
        Provider::Grok => grok::cli_model(invocation),
        Provider::Codex => codex::cli_model(invocation),
    }
}
