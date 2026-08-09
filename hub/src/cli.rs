//! The argv parse, and the grammar it enforces.
//!
//! > **`--flags` act on the hub. Bare words act on an app.**
//!
//! One sentence, no ambiguous case, and nothing new to learn: it is the line
//! the station already draws today, where `--update`/`--uninstall`/`--export`
//! are station-level lifecycle flags and the single bare-word subcommand,
//! `run`, is the app-level one. Design source:
//! `../TFSAppWorkstation/.project/hub/003-cli-surface.md` §1–§2.
//!
//! Two rules keep this module the size the station's `cli.rs` stayed across
//! sixty plans:
//!
//! - **It parses and routes; it never works.** [`parse`] turns argv into a
//!   [`Command`] and nothing else — no filesystem, no registry, no process.
//!   That is what makes every form in the grammar testable without a machine
//!   that has anything installed.
//! - **The whole surface is declared here from the start**, including the parts
//!   no plan has implemented yet. A command that is recognised but unavailable
//!   is answered with "not implemented yet" ([`EXIT_UNIMPLEMENTED`]), never
//!   with an unknown-command error — a wrong message here sends a user hunting
//!   for a typo that is not there.
//!
//! [`SURFACE`] is the single source of truth for both halves of that second
//! rule: `--help` renders it, and a usage error quotes the offending command's
//! own line out of it. A subcommand cannot therefore be added to the grammar
//! and forgotten in the help text.

use std::{collections::BTreeMap, fmt};

/// The command ran and did what it said.
pub const EXIT_OK: i32 = 0;
/// The command was understood and failed while doing its work.
pub const EXIT_FAILED: i32 = 1;
/// The command line itself is wrong: an unknown word, a missing argument. The
/// station's own value for the same situation.
pub const EXIT_USAGE: i32 = 2;
/// The command line is *right* and the hub cannot honour it yet. Distinct from
/// both of the above on purpose: nothing the user types will fix it, and
/// nothing about their machine is broken.
pub const EXIT_UNIMPLEMENTED: i32 = 3;

/// Temporary and hidden — the double underscore says so, and nothing prints it
/// in a usage line or in `--help`. It exists so the `platform` fingerprint can
/// be eyeballed against the bundled binary's real `php -m` before anything
/// depends on it, and it goes away once an installed app records one and `list`
/// can show it.
pub const PLATFORM_SUBCOMMAND: &str = "__platform";

/// Hidden, same convention, and not temporary: this is the form `open <id>`
/// re-executes the hub binary with (plan 007, see `open.rs`). It is one half of
/// one command rather than a command of its own — a user has no reason to type
/// it and no way to get it right, since the identity it carries is exactly what
/// the parent just resolved for them.
///
/// It grew out of plan 003's `__open --identity <id>`, which opened a window
/// from argv alone because nothing was installed yet to resolve an identity
/// from. Same hidden word, now with a real caller.
pub const OPEN_CHILD_SUBCOMMAND: &str = "__open";

/// Which level of subject a form acts on — the grammar's whole content, and
/// the two groups `--help` prints under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Hub,
    App,
}

/// Whether the hub can honour a form today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Implemented,
    NotYet,
}

/// One line of the grammar.
pub struct Spec {
    /// The word [`Command::name`] answers with — the lookup key, and what a
    /// "not implemented yet" message names.
    pub name: &'static str,
    /// The usage line, without the binary's own name. `--help` prints it as it
    /// stands; a usage error prints `usage: tfsapp-hub {form}`.
    pub form: &'static str,
    /// One line, in `--help`'s right-hand column. Written for someone who has
    /// not read the plans: what the command does, not how.
    pub summary: &'static str,
    pub level: Level,
    pub availability: Availability,
}

/// The whole CLI surface, in the order `--help` prints it.
///
/// Declared in full from plan 005, implemented across plans 005–012. Several
/// rows may share a `name` (`run` has three forms); [`spec`] answers with the
/// first, which is the form a usage error quotes.
pub const SURFACE: &[Spec] = &[
    Spec {
        name: "--version",
        form: "--version",
        summary: "Print the hub's version.",
        level: Level::Hub,
        availability: Availability::Implemented,
    },
    Spec {
        name: "--help",
        form: "--help",
        summary: "Print this message.",
        level: Level::Hub,
        availability: Availability::Implemented,
    },
    Spec {
        name: "--update",
        form: "--update [--yes]",
        summary: "Update the hub itself to the latest release.",
        level: Level::Hub,
        availability: Availability::NotYet,
    },
    Spec {
        name: "--rollback",
        form: "--rollback [--yes]",
        summary: "Undo the last hub update.",
        level: Level::Hub,
        availability: Availability::NotYet,
    },
    Spec {
        name: "install",
        form: "install <source> [--as <id>] [--ref <tag|branch|sha>] [--yes] \
               [--no-desktop-entry]",
        summary: "Install an app from a local directory or a git URL.",
        level: Level::App,
        availability: Availability::Implemented,
    },
    Spec {
        name: "list",
        form: "list",
        summary: "List the installed apps.",
        level: Level::App,
        availability: Availability::Implemented,
    },
    Spec {
        name: "open",
        form: "open <id>",
        summary: "Open an installed app's window.",
        level: Level::App,
        availability: Availability::Implemented,
    },
    // The second bare word whose argument is a path rather than an app id —
    // `install`'s own exemption, taken again for the same reason: `dev`'s
    // subject is a source, not yet an installed app (plan 005's 2026-08-08
    // amendment). The grammar rule stays unbent.
    Spec {
        name: "dev",
        form: "dev <local-path>",
        summary: "Run a live project in its own window, served in place.",
        level: Level::App,
        availability: Availability::Implemented,
    },
    Spec {
        name: "update",
        form: "update <id> [--ref <tag|branch|sha>] [--force]",
        summary: "Re-resolve an app's source and update it.",
        level: Level::App,
        availability: Availability::NotYet,
    },
    Spec {
        name: "remove",
        form: "remove <id> [--purge] [--yes]",
        summary: "Uninstall an app; --purge also drops its data.",
        level: Level::App,
        availability: Availability::Implemented,
    },
    Spec {
        name: "export",
        form: "export <id> <path>",
        summary: "Write an app's data to <path>.tar.gz.",
        level: Level::App,
        availability: Availability::NotYet,
    },
    Spec {
        name: "import",
        form: "import <id> <path> [--force]",
        summary: "Seed an app's data from a .tar.gz written by export.",
        level: Level::App,
        availability: Availability::NotYet,
    },
    Spec {
        name: "run",
        form: "run <id>",
        summary: "List an app's declared run aliases.",
        level: Level::App,
        availability: Availability::NotYet,
    },
    Spec {
        name: "run",
        form: "run <id> <alias> [args...]",
        summary: "Run an app-declared command in the foreground.",
        level: Level::App,
        availability: Availability::NotYet,
    },
    Spec {
        name: "run",
        form: "run --stop <id>",
        summary: "Stop whatever run command that app is running.",
        level: Level::App,
        availability: Availability::NotYet,
    },
    Spec {
        name: "run",
        form: "run --replace <id> <alias> [args...]",
        summary: "Stop an active run command, then start <alias> in its place.",
        level: Level::App,
        availability: Availability::NotYet,
    },
];

/// The grammar line for `name`, or `None` for the hidden subcommands, which
/// deliberately have none.
pub fn spec(name: &str) -> Option<&'static Spec> {
    SURFACE.iter().find(|spec| spec.name == name)
}

/// The marker a form that no plan has implemented yet carries in `--help`.
///
/// Printed rather than hidden, and in the same words the refusal uses: a user
/// who reads the help must be able to tell what the hub can do today from what
/// it is going to do, without typing it to find out.
pub const NOT_YET_MARKER: &str = "(not implemented yet)";

/// The column `--help` starts its summaries in. A form longer than that gets
/// its summary on the next line rather than pushing every other summary right.
const SUMMARY_COLUMN: usize = 26;

/// `--help`, rendered from [`SURFACE`].
///
/// Rendered rather than written out, so the grammar cannot be extended and the
/// help forgotten — the failure mode a hand-maintained help text has every
/// time. The rule itself leads, because it is the one thing that makes the
/// rest of the list predictable instead of memorised.
pub fn help_text() -> String {
    let mut text = String::from(
        "tfsapp-hub — install and run several Symfony apps from one binary.\n\n  \
         --flags act on the hub. Bare words act on an app.\n",
    );

    for (level, heading) in [(Level::Hub, "The hub itself:"), (Level::App, "An app:")] {
        text.push_str(&format!("\n{heading}\n"));

        for spec in SURFACE.iter().filter(|spec| spec.level == level) {
            let form = format!("  {}", spec.form);
            let summary = match spec.availability {
                Availability::Implemented => spec.summary.to_string(),
                Availability::NotYet => format!("{} {NOT_YET_MARKER}", spec.summary),
            };

            match form.len() < SUMMARY_COLUMN {
                true => text.push_str(&format!("{form:SUMMARY_COLUMN$}{summary}\n")),
                false => text.push_str(&format!(
                    "{form}\n{blank:SUMMARY_COLUMN$}{summary}\n",
                    blank = ""
                )),
            }
        }
    }

    text
}

/// One recognised invocation. Every field a later plan will need is parsed
/// here and now, so implementing a command is writing its module and nothing
/// else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Version,
    Help,
    HubUpdate {
        assume_yes: bool,
    },
    HubRollback {
        assume_yes: bool,
    },
    Install {
        source: String,
        /// `--as`: the hub-local handle to install under. `None` leaves the
        /// installer to derive one from the manifest.
        id: Option<String>,
        reference: Option<String>,
        /// `--yes`: skip the confirmation. The two commands that touch a user's
        /// machine irreversibly ask first, and both take the same escape for a
        /// script — the spelling the station's own lifecycle flags established.
        assume_yes: bool,
        /// `--no-desktop-entry`: skip both the generated `.desktop` entry and
        /// the stable self-copy that only exists to be pointed at — a flag
        /// that says "do not touch my desktop" should not leave a silent
        /// 170 MB copy behind either.
        no_desktop_entry: bool,
    },
    List,
    Open {
        id: String,
    },
    Dev {
        path: String,
    },
    Update {
        id: String,
        reference: Option<String>,
        force: bool,
    },
    Remove {
        id: String,
        purge: bool,
        assume_yes: bool,
    },
    Export {
        id: String,
        path: String,
    },
    Import {
        id: String,
        path: String,
        force: bool,
    },
    Run(RunInvocation),
    /// Hidden, temporary — see [`PLATFORM_SUBCOMMAND`].
    Platform,
    /// Hidden — see [`OPEN_CHILD_SUBCOMMAND`]. Carries the raw strings rather
    /// than an `Identity` so this module stays free of the identity types it
    /// would otherwise have to know about.
    OpenChild {
        /// Which of `open`'s or `dev`'s constructors the child rebuilds its
        /// `LaunchSpec` with.
        source: OpenChildSource,
        identifier: String,
        product_name: String,
        icon_path: Option<String>,
    },
}

/// `__open`'s `--id <id>` or `--project <path>` — exactly one, never both.
///
/// One hidden subcommand serving two callers rather than a second hidden
/// subcommand: `open <id>` re-execs with `--id`, `dev <path>` (plan 009) with
/// `--project`, and the child rebuilds the matching `LaunchSpec` from
/// whichever it was given. A second hidden subcommand would fork the child
/// path, which is precisely what the `LaunchSpec` refactor exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenChildSource {
    Id(String),
    Project(String),
}

/// `run`'s three forms. Kept apart from [`Command`]'s other variants because
/// they share one word and one module, exactly as the station's `run.rs` has
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunInvocation {
    /// `run <id>` alone: list the app's declared aliases rather than run one.
    /// The hub's own addition, absent from the station's grammar — the
    /// station discovers aliases through `--help`, which the hub cannot do
    /// because the aliases belong to an app and not to the binary.
    List {
        id: String,
    },
    Stop {
        id: String,
    },
    Start {
        id: String,
        alias: String,
        /// Everything after the alias, passed to the app's command untouched.
        args: Vec<String>,
        /// `--replace`: stop an active run command first.
        replace: bool,
    },
}

impl Command {
    /// The grammar word this command was reached by — the [`SURFACE`] lookup
    /// key, and what a "not implemented yet" message names.
    ///
    /// An exhaustive match on purpose: a variant added without a name will not
    /// compile, which is what keeps the enum and the help text in step.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Version => "--version",
            Self::Help => "--help",
            Self::HubUpdate { .. } => "--update",
            Self::HubRollback { .. } => "--rollback",
            Self::Install { .. } => "install",
            Self::List => "list",
            Self::Open { .. } => "open",
            Self::Dev { .. } => "dev",
            Self::Update { .. } => "update",
            Self::Remove { .. } => "remove",
            Self::Export { .. } => "export",
            Self::Import { .. } => "import",
            Self::Run(_) => "run",
            Self::Platform => PLATFORM_SUBCOMMAND,
            Self::OpenChild { .. } => OPEN_CHILD_SUBCOMMAND,
        }
    }

    /// Whether the hub can honour this command today. Read from [`SURFACE`]
    /// rather than from a second match, so marking a command implemented is one
    /// edit and not two that can drift. The hidden subcommands have no row and
    /// are, by construction, implemented — nothing else would justify hiding
    /// them.
    pub fn is_implemented(&self) -> bool {
        spec(self.name()).is_none_or(|spec| spec.availability == Availability::Implemented)
    }
}

/// Why an invocation was refused, and which form it should have taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError {
    pub message: String,
    /// The offending command's own grammar line, when the command itself was
    /// recognised. `None` when it was not — there is no single right form to
    /// point at, so [`UsageError::hint`] points at `--help` instead.
    pub form: Option<&'static str>,
}

impl UsageError {
    /// The second line to print after the message: the form that was meant, or
    /// the way to find it.
    pub fn hint(&self) -> String {
        match self.form {
            Some(form) => format!("usage: tfsapp-hub {form}"),
            None => "try `tfsapp-hub --help` for the full grammar.".to_string(),
        }
    }
}

impl fmt::Display for UsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for UsageError {}

/// Turn argv (already stripped of the binary's own name) into a [`Command`].
pub fn parse(args: &[String]) -> Result<Command, UsageError> {
    // No arguments at all: print the grammar rather than an error. The hub is
    // the kind of binary a newcomer runs bare to find out what it does, and
    // there is no window to fall through to the way the station has.
    let Some(first) = args.first().map(String::as_str) else {
        return Ok(Command::Help);
    };

    // `--help` wins wherever it appears — `tfsapp-hub install --help` is asking
    // a question, not asking for an install. The one place it is not the hub's
    // to read is after `run <id> <alias>`, where every remaining word belongs to
    // the app's own command; `parse_run` handles a leading `--help` itself.
    if first != "run" && args.iter().any(|arg| arg == "--help") {
        return Ok(Command::Help);
    }

    let rest = &args[1..];
    match first {
        "--version" => {
            no_arguments("--version", rest)?;
            Ok(Command::Version)
        }
        "--update" => Ok(Command::HubUpdate {
            assume_yes: assume_yes("--update", rest)?,
        }),
        "--rollback" => Ok(Command::HubRollback {
            assume_yes: assume_yes("--rollback", rest)?,
        }),
        "install" => {
            let mut options = options(
                "install",
                rest,
                &["--as", "--ref"],
                &["--yes", "-y", "--no-desktop-entry"],
            )?;
            let source = options.exactly_one("install", "a source")?;
            Ok(Command::Install {
                source,
                id: options.value("--as"),
                reference: options.value("--ref"),
                assume_yes: options.assume_yes(),
                no_desktop_entry: options.flag("--no-desktop-entry"),
            })
        }
        "list" => {
            no_arguments("list", rest)?;
            Ok(Command::List)
        }
        "open" => {
            let mut options = options("open", rest, &[], &[])?;
            Ok(Command::Open {
                id: options.exactly_one("open", "an app id")?,
            })
        }
        "dev" => {
            let mut options = options("dev", rest, &[], &[])?;
            Ok(Command::Dev {
                path: options.exactly_one("dev", "a project path")?,
            })
        }
        "update" => {
            let mut options = options("update", rest, &["--ref"], &["--force"])?;
            Ok(Command::Update {
                id: options.exactly_one("update", "an app id")?,
                reference: options.value("--ref"),
                force: options.flag("--force"),
            })
        }
        "remove" => {
            let mut options = options("remove", rest, &[], &["--purge", "--yes", "-y"])?;
            Ok(Command::Remove {
                id: options.exactly_one("remove", "an app id")?,
                purge: options.flag("--purge"),
                assume_yes: options.assume_yes(),
            })
        }
        "export" => {
            let mut options = options("export", rest, &[], &[])?;
            let (id, path) = options.exactly_two("export", "an app id and a path")?;
            Ok(Command::Export { id, path })
        }
        "import" => {
            let mut options = options("import", rest, &[], &["--force"])?;
            let (id, path) = options.exactly_two("import", "an app id and a path")?;
            Ok(Command::Import {
                id,
                path,
                force: options.flag("--force"),
            })
        }
        "run" => parse_run(rest),
        PLATFORM_SUBCOMMAND => Ok(Command::Platform),
        OPEN_CHILD_SUBCOMMAND => parse_open_child(rest),
        unknown => Err(UsageError {
            message: match unknown.starts_with('-') {
                // Saying which level was meant is the whole grammar in one
                // line, at the moment it is most likely to be needed.
                true => format!(
                    "unknown hub-level option {unknown}. Flags act on the hub; \
                     bare words act on an app."
                ),
                false => format!("unknown command {unknown}"),
            },
            form: None,
        }),
    }
}

/// `run <id> <alias> [args...]`, `run --stop <id>`, `run --replace <id> <alias>
/// [args...]`.
///
/// Parsed by hand rather than through [`options`] because of what makes `run`
/// different from every other form: after the alias, argv stops being the
/// hub's. A `--force` there is the app's command's `--force`, and reading it
/// as one of ours would corrupt the very invocation we are meant to forward.
fn parse_run(args: &[String]) -> Result<Command, UsageError> {
    let mut index = 0;
    let mut stop = false;
    let mut replace = false;

    while let Some(argument) = args.get(index).map(String::as_str) {
        match argument {
            "--help" => return Ok(Command::Help),
            "--stop" => stop = true,
            "--replace" => replace = true,
            other if other.starts_with('-') => {
                return Err(usage_error(
                    "run",
                    format!("unknown option {other} for run"),
                ))
            }
            _ => break,
        }
        index += 1;
    }

    if stop && replace {
        return Err(usage_error(
            "run",
            "run takes --stop or --replace, not both".to_string(),
        ));
    }

    let positionals = &args[index..];
    if stop {
        return match positionals {
            [id] => Ok(Command::Run(RunInvocation::Stop { id: id.clone() })),
            [] => Err(usage_error("run", "run --stop needs an app id".to_string())),
            _ => Err(usage_error(
                "run",
                "run --stop takes an app id and nothing else — it stops \
                 whatever command that app is running"
                    .to_string(),
            )),
        };
    }

    let (id, alias) =
        match positionals {
            [] => return Err(usage_error(
                "run",
                "run needs an app id — `run <id>` lists its declared aliases, `run <id> <alias>` \
                 runs one"
                    .to_string(),
            )),
            [id] => return Ok(Command::Run(RunInvocation::List { id: id.clone() })),
            [id, alias, ..] => (id.clone(), alias.clone()),
        };
    let forwarded = &positionals[2..];

    // A bare `--` separates the hub's words from the app's. It is optional (the
    // alias already marks that boundary) and is dropped rather than forwarded,
    // which is what makes `run <id> <alias> -- --force` a way to pass a flag
    // the hub would otherwise never have looked at anyway.
    let forwarded = match forwarded.split_first() {
        Some((separator, tail)) if separator == "--" => tail,
        _ => forwarded,
    };

    Ok(Command::Run(RunInvocation::Start {
        id,
        alias,
        args: forwarded.to_vec(),
        replace,
    }))
}

/// The hidden `__open (--id <id> | --project <path>) --identity <identifier>
/// --name <n> [--icon <p>]`.
///
/// `--identity`/`--name` are required, and neither is defaulted: this form is
/// written by `open <id>` or `dev <path>` and by nothing else, so a missing one
/// is a bug in the parent rather than a user's typo, and quietly filling it in
/// would hide exactly that. `--icon` stays optional because an app declaring no
/// icon is ordinary. `--id`/`--project` are exactly-one — see
/// [`OpenChildSource`].
fn parse_open_child(args: &[String]) -> Result<Command, UsageError> {
    let options = options(
        OPEN_CHILD_SUBCOMMAND,
        args,
        &["--id", "--project", "--identity", "--name", "--icon"],
        &[],
    )?;
    options.no_positionals(OPEN_CHILD_SUBCOMMAND)?;

    let source = match (options.value("--id"), options.value("--project")) {
        (Some(id), None) => OpenChildSource::Id(id),
        (None, Some(path)) => OpenChildSource::Project(path),
        (None, None) => {
            return Err(usage_error(
                OPEN_CHILD_SUBCOMMAND,
                format!("{OPEN_CHILD_SUBCOMMAND} needs --id or --project"),
            ))
        }
        (Some(_), Some(_)) => {
            return Err(usage_error(
                OPEN_CHILD_SUBCOMMAND,
                format!("{OPEN_CHILD_SUBCOMMAND} takes --id or --project, not both"),
            ))
        }
    };

    let required = |flag: &str| {
        options.value(flag).ok_or_else(|| {
            usage_error(
                OPEN_CHILD_SUBCOMMAND,
                format!("{OPEN_CHILD_SUBCOMMAND} needs {flag}"),
            )
        })
    };

    Ok(Command::OpenChild {
        source,
        identifier: required("--identity")?,
        product_name: required("--name")?,
        icon_path: options.value("--icon"),
    })
}

/// What [`options`] found: the words that were not flags, the flags that took a
/// value, and the ones that did not.
#[derive(Default)]
struct Options {
    positionals: Vec<String>,
    values: BTreeMap<&'static str, String>,
    flags: Vec<&'static str>,
}

impl Options {
    fn value(&self, flag: &str) -> Option<String> {
        self.values.get(flag).cloned()
    }

    fn flag(&self, flag: &str) -> bool {
        self.flags.contains(&flag)
    }

    /// `--yes` or its `-y` short form, which the hub-level lifecycle flags
    /// already accept — one spelling for one meaning across the grammar.
    fn assume_yes(&self) -> bool {
        self.flag("--yes") || self.flag("-y")
    }

    fn exactly_one(&mut self, name: &'static str, expected: &str) -> Result<String, UsageError> {
        match self.positionals.len() {
            1 => Ok(self.positionals.remove(0)),
            0 => Err(usage_error(name, format!("{name} needs {expected}"))),
            found => Err(usage_error(
                name,
                format!("{name} takes {expected}, got {found} words"),
            )),
        }
    }

    fn exactly_two(
        &mut self,
        name: &'static str,
        expected: &str,
    ) -> Result<(String, String), UsageError> {
        match self.positionals.len() {
            2 => Ok((self.positionals.remove(0), self.positionals.remove(0))),
            found => Err(usage_error(
                name,
                format!("{name} needs {expected}, got {found} words"),
            )),
        }
    }

    fn no_positionals(&self, name: &'static str) -> Result<(), UsageError> {
        match self.positionals.first() {
            None => Ok(()),
            Some(unexpected) => Err(usage_error(
                name,
                format!("{name} takes no bare words, got {unexpected}"),
            )),
        }
    }
}

/// The plain "flags and bare words in any order" parse every form but `run`
/// has. Anything starting with `-` that was not declared is refused here rather
/// than silently taken for a positional.
fn options(
    name: &'static str,
    args: &[String],
    valued: &[&'static str],
    boolean: &[&'static str],
) -> Result<Options, UsageError> {
    let mut parsed = Options::default();
    let mut index = 0;

    while index < args.len() {
        let argument = args[index].as_str();

        if let Some(flag) = valued.iter().find(|flag| **flag == argument) {
            let value = args
                .get(index + 1)
                .ok_or_else(|| usage_error(name, format!("{flag} needs a value")))?;
            parsed.values.insert(flag, value.clone());
            index += 2;
            continue;
        }

        if let Some(flag) = boolean.iter().find(|flag| **flag == argument) {
            parsed.flags.push(flag);
            index += 1;
            continue;
        }

        if argument.starts_with('-') {
            return Err(usage_error(
                name,
                format!("unknown option {argument} for {name}"),
            ));
        }

        parsed.positionals.push(argument.to_string());
        index += 1;
    }

    Ok(parsed)
}

/// `--yes`/`-y`, the confirmation skip the two hub-level lifecycle flags share
/// with the station's.
fn assume_yes(name: &'static str, args: &[String]) -> Result<bool, UsageError> {
    let mut assume_yes = false;
    for argument in args {
        match argument.as_str() {
            "--yes" | "-y" => assume_yes = true,
            other => {
                return Err(usage_error(
                    name,
                    format!("unknown option {other} for {name}"),
                ))
            }
        }
    }
    Ok(assume_yes)
}

fn no_arguments(name: &'static str, args: &[String]) -> Result<(), UsageError> {
    match args.first() {
        None => Ok(()),
        Some(unexpected) => Err(usage_error(
            name,
            format!("{name} takes no arguments, got {unexpected}"),
        )),
    }
}

fn usage_error(name: &'static str, message: String) -> UsageError {
    UsageError {
        message,
        form: spec(name).map(|spec| spec.form),
    }
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
