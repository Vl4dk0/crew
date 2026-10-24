mod claude;
mod codex;
mod gemini;
mod grok;
mod proc;
mod pty;

pub(crate) use proc::{detach_session, terminate_group};

use crate::cli::Invocation;
use crate::error::Error;

/// Run one provider in the foreground.
///
/// Each provider file owns that CLI's argv and its success predicate.
/// The dispatch stays here so a new flag does not fork four call sites.
pub fn execute(invocation: &Invocation) -> Result<i32, Error> {
    match invocation.provider {
        crate::id::Provider::Claude => claude::run(invocation),
        crate::id::Provider::Gemini => gemini::run(invocation),
        crate::id::Provider::Grok => grok::run(invocation),
        crate::id::Provider::Codex => codex::run(invocation),
    }
}
