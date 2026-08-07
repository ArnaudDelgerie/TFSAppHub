use super::{parse, Availability, Command, RunInvocation, UsageError, SURFACE};

/// Every test spells its invocation the way a user types it. Splitting on
/// whitespace is enough because no form in the grammar takes an argument
/// containing a space — and the one place that could change, `run`'s
/// passthrough, is covered by [`forwarded_arguments_are_not_the_hubs_to_read`]
/// with words the shell would not have split either.
fn parse_line(line: &str) -> Result<Command, UsageError> {
    let args: Vec<String> = line.split_whitespace().map(str::to_string).collect();
    parse(&args)
}

fn command(line: &str) -> Command {
    parse_line(line).unwrap_or_else(|error| panic!("{line:?} should parse: {error}"))
}

fn refusal(line: &str) -> UsageError {
    parse_line(line).expect_err(&format!("{line:?} should be refused"))
}

#[test]
fn hub_level_flags_route_to_the_hub() {
    assert_eq!(command("--version"), Command::Version);
    assert_eq!(command("--help"), Command::Help);
    assert_eq!(
        command("--update"),
        Command::HubUpdate { assume_yes: false }
    );
    assert_eq!(
        command("--update --yes"),
        Command::HubUpdate { assume_yes: true }
    );
    assert_eq!(
        command("--update -y"),
        Command::HubUpdate { assume_yes: true }
    );
    assert_eq!(
        command("--rollback"),
        Command::HubRollback { assume_yes: false }
    );
    assert_eq!(
        command("--rollback --yes"),
        Command::HubRollback { assume_yes: true }
    );
}

#[test]
fn bare_words_route_to_an_app() {
    assert_eq!(
        command("install ../TFSAppTest"),
        Command::Install {
            source: "../TFSAppTest".to_string(),
            id: None,
            reference: None,
        }
    );
    assert_eq!(
        command("install https://example.test/app.git --as demo --ref v1.4.0"),
        Command::Install {
            source: "https://example.test/app.git".to_string(),
            id: Some("demo".to_string()),
            reference: Some("v1.4.0".to_string()),
        }
    );
    assert_eq!(command("list"), Command::List);
    assert_eq!(
        command("open demo"),
        Command::Open {
            id: "demo".to_string()
        }
    );
    assert_eq!(
        command("update demo"),
        Command::Update {
            id: "demo".to_string(),
            reference: None,
            force: false,
        }
    );
    assert_eq!(
        command("update demo --ref main --force"),
        Command::Update {
            id: "demo".to_string(),
            reference: Some("main".to_string()),
            force: true,
        }
    );
    assert_eq!(
        command("remove demo"),
        Command::Remove {
            id: "demo".to_string(),
            purge: false,
        }
    );
    assert_eq!(
        command("remove demo --purge"),
        Command::Remove {
            id: "demo".to_string(),
            purge: true,
        }
    );
    assert_eq!(
        command("export demo /tmp/demo"),
        Command::Export {
            id: "demo".to_string(),
            path: "/tmp/demo".to_string(),
        }
    );
    assert_eq!(
        command("import demo /tmp/demo.tar.gz --force"),
        Command::Import {
            id: "demo".to_string(),
            path: "/tmp/demo.tar.gz".to_string(),
            force: true,
        }
    );
}

#[test]
fn run_takes_an_app_before_its_alias() {
    assert_eq!(
        command("run demo console"),
        Command::Run(RunInvocation::Start {
            id: "demo".to_string(),
            alias: "console".to_string(),
            args: Vec::new(),
            replace: false,
        })
    );
    assert_eq!(
        command("run --replace demo console"),
        Command::Run(RunInvocation::Start {
            id: "demo".to_string(),
            alias: "console".to_string(),
            args: Vec::new(),
            replace: true,
        })
    );
    assert_eq!(
        command("run --stop demo"),
        Command::Run(RunInvocation::Stop {
            id: "demo".to_string()
        })
    );
}

#[test]
fn forwarded_arguments_are_not_the_hubs_to_read() {
    // The point of the form: `--force` here is the app command's flag, and the
    // hub reading it as one of its own would corrupt the invocation it exists
    // to forward. Same for `--help`, which everywhere else wins outright.
    let forwarded = Command::Run(RunInvocation::Start {
        id: "demo".to_string(),
        alias: "console".to_string(),
        args: vec!["cache:clear".to_string(), "--force".to_string()],
        replace: false,
    });

    assert_eq!(command("run demo console cache:clear --force"), forwarded);
    // The optional `--` separator is dropped, not forwarded.
    assert_eq!(
        command("run demo console -- cache:clear --force"),
        forwarded
    );
    assert_eq!(
        command("run demo console --help"),
        Command::Run(RunInvocation::Start {
            id: "demo".to_string(),
            alias: "console".to_string(),
            args: vec!["--help".to_string()],
            replace: false,
        })
    );
}

#[test]
fn help_wins_wherever_it_appears_before_an_app_command() {
    // A user who types `install --help` is asking a question, not asking for an
    // install — and `--update --help` must never start an update.
    assert_eq!(command(""), Command::Help);
    assert_eq!(command("install ../TFSAppTest --help"), Command::Help);
    assert_eq!(command("--update --help"), Command::Help);
    assert_eq!(command("run --help"), Command::Help);
}

#[test]
fn a_malformed_invocation_names_the_right_form() {
    let cases = [
        ("open", "open <id>"),
        ("open one two", "open <id>"),
        ("install", "install <source>"),
        ("install ../app --as", "install <source>"),
        ("list --all", "list"),
        ("export demo", "export <id> <path>"),
        ("import demo /tmp/x.tar.gz --purge", "import <id> <path>"),
        ("update demo --ref", "update <id>"),
        ("run demo", "run <id> <alias>"),
        ("run --stop", "run <id> <alias>"),
        ("run --stop demo console", "run <id> <alias>"),
        ("run --stop --replace demo console", "run <id> <alias>"),
        ("--update --now", "--update [--yes]"),
        ("--version extra", "--version"),
    ];

    for (line, form) in cases {
        let error = refusal(line);
        let hint = error.hint();
        assert!(
            hint.contains(form),
            "{line:?} should point at {form:?}, said {hint:?}"
        );
    }
}

#[test]
fn an_unknown_word_is_refused_without_a_form_to_point_at() {
    // Nothing was recognised, so there is no right form — only the way to find
    // one.
    let error = refusal("instal ../TFSAppTest");
    assert!(error.message.contains("instal"), "{}", error.message);
    assert!(error.hint().contains("--help"), "{}", error.hint());

    // A hub-level flag nobody declared restates the grammar rule, which is the
    // most likely thing the user got wrong.
    let error = refusal("--list");
    assert!(error.message.contains("bare words"), "{}", error.message);
}

#[test]
fn unimplemented_commands_are_recognised_rather_than_rejected() {
    // The distinction the whole "not implemented yet" outcome exists for: a
    // wrong error here sends a user hunting for a typo that is not there.
    for line in [
        "--update",
        "--rollback",
        "install ../TFSAppTest",
        "open demo",
        "update demo",
        "remove demo",
        "export demo /tmp/demo",
        "import demo /tmp/demo.tar.gz",
        "run demo console",
    ] {
        assert!(
            !command(line).is_implemented(),
            "{line:?} is not implemented yet, and parsing must still recognise it"
        );
    }

    for line in ["--version", "--help", "list"] {
        assert!(command(line).is_implemented(), "{line:?} works today");
    }
}

/// A grammar line reduced to the invocation it demands: `[optional]` groups
/// dropped, every `<placeholder>` filled in with a plausible word.
fn mandatory_shape(form: &str) -> String {
    let mut depth = 0usize;
    form.split_whitespace()
        .filter(|word| {
            depth += word.matches('[').count();
            let inside = depth > 0;
            depth -= word.matches(']').count().min(depth);
            !inside
        })
        .map(|word| match word.starts_with('<') {
            true => "x",
            false => word,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn every_form_in_the_surface_parses_back_to_its_own_command() {
    // The table `--help` prints is not decoration: each line has to be an
    // invocation the parser accepts, and one that routes to the command it
    // names. A form whose placeholders were renamed without the parser
    // following fails here.
    for spec in SURFACE {
        let command = command(&mandatory_shape(spec.form));
        assert_eq!(
            command.name(),
            spec.name,
            "{:?} should route to {}",
            spec.form,
            spec.name
        );
        assert_eq!(
            command.is_implemented(),
            spec.availability == Availability::Implemented,
            "{:?}'s availability disagrees with its own row",
            spec.form
        );
    }
}

#[test]
fn the_hidden_subcommands_stay_reachable_and_unlisted() {
    assert_eq!(command("__platform"), Command::Platform);
    assert_eq!(
        command("__open --identity dev.local.demo --name Demo --icon /tmp/demo.png"),
        Command::OpenIdentity {
            identifier: "dev.local.demo".to_string(),
            product_name: Some("Demo".to_string()),
            icon_path: Some("/tmp/demo.png".to_string()),
        }
    );
    assert!(refusal("__open --name Demo").message.contains("--identity"));

    // Hidden means hidden: they are temporary, and a user who reads about one
    // in `--help` will reasonably expect it to keep working.
    for spec in SURFACE {
        assert!(
            !spec.name.starts_with("__"),
            "{} is a hidden subcommand and has no place in the surface",
            spec.name
        );
    }
}
