//! The one question the hub asks before touching a machine.
//!
//! The commands that ask it change something a user cannot undo by
//! retyping, so each prints what it is about to do and waits first. The
//! wording lives here rather than in each of them so the answer means the
//! same thing whichever one asked.
//!
//! `publish`'s `actions.secrets.ipc` gate asks the same question through
//! [`confirmed_at_terminal`], but no flag can stand in for its author: the
//! setting ships to everyone who installs the release.
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

    ask_or_refuse(
        "tfsapp-hub: stdin is not a terminal — refusing to guess. Pass --yes to \
         confirm non-interactively.",
    )
}

/// The one question no flag can answer: `publish`'s `actions.secrets.ipc` gate
/// ships its setting to every user who installs the release, so its author
/// confirms in person. The refusal says the confirmation is only given at a
/// terminal instead of pointing at a flag that does nothing here.
pub fn confirmed_at_terminal() -> bool {
    ask_or_refuse(
        "tfsapp-hub: stdin is not a terminal — this confirmation is only given \
         at a terminal; no flag answers it.",
    )
}

/// The shared half: refuse away from a terminal with the caller's wording,
/// ask the same `[y/N]` question at one.
fn ask_or_refuse(refusal: &str) -> bool {
    if !io::stdin().is_terminal() {
        eprintln!("{refusal}");
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
