//! The one question the hub asks before touching a machine.
//!
//! Two commands change something a user cannot undo by retyping — `install`
//! runs third-party PHP, `remove --purge` deletes their data — and both print
//! what they are about to do and wait. The wording lives here rather than in
//! each of them so the answer means the same thing whichever one asked.
//!
//! The non-terminal case is the one worth stating: a piped or scripted
//! invocation with no `--yes` is **refused**, never assumed. Reading a closed
//! stdin as "no" would be safe but silent, and as "yes" would be a way to
//! delete someone's data from a cron line they thought was a dry run.

use std::io::{self, IsTerminal, Write};

/// Ask, unless `assume_yes` already answered. `true` means go ahead.
///
/// Prints its own refusal for the non-terminal case, since that message is
/// about *how* the question could not be asked and is the same either way. The
/// caller prints what aborting means for it.
pub fn confirmed(assume_yes: bool) -> bool {
    if assume_yes {
        return true;
    }

    if !io::stdin().is_terminal() {
        eprintln!(
            "tfsapp-hub: stdin is not a terminal — refusing to guess. Pass --yes to \
             confirm non-interactively."
        );
        return false;
    }

    print!("Proceed? [y/N] ");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    // Anything but an explicit yes is a no, including an empty line — the
    // capitalised N in the prompt is the promise this keeps.
    matches!(answer.trim(), "y" | "Y" | "yes")
}
