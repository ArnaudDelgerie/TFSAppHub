use super::{
    help_text, parse, second_instance_files, Availability, Command, OpenChildSource, RunInvocation,
    UsageError, NOT_YET_MARKER, SURFACE,
};

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
            assume_yes: false,
            no_desktop_entry: false,
        }
    );
    assert_eq!(
        command("install https://example.test/app.git --as demo --ref v1.4.0 --yes"),
        Command::Install {
            source: "https://example.test/app.git".to_string(),
            id: Some("demo".to_string()),
            reference: Some("v1.4.0".to_string()),
            assume_yes: true,
            no_desktop_entry: false,
        }
    );
    assert_eq!(
        command("install ../TFSAppTest --no-desktop-entry"),
        Command::Install {
            source: "../TFSAppTest".to_string(),
            id: None,
            reference: None,
            assume_yes: false,
            no_desktop_entry: true,
        }
    );
    assert_eq!(command("list"), Command::List);
    assert_eq!(
        command("open demo"),
        Command::Open {
            id: "demo".to_string(),
            files: Vec::new()
        }
    );
    assert_eq!(
        command("dev ../TFSAppTest"),
        Command::Dev {
            path: "../TFSAppTest".to_string()
        }
    );
    assert_eq!(
        command("publish ../TFSAppTest"),
        Command::Publish {
            path: "../TFSAppTest".to_string(),
            repo: None,
            assume_yes: false,
        }
    );
    assert_eq!(
        command("publish ../TFSAppTest --repo owner/repo --yes"),
        Command::Publish {
            path: "../TFSAppTest".to_string(),
            repo: Some("owner/repo".to_string()),
            assume_yes: true,
        }
    );
    assert_eq!(
        command("update demo"),
        Command::Update {
            id: "demo".to_string(),
            reference: None,
            force: false,
            assume_yes: false,
        }
    );
    assert_eq!(
        command("update demo --ref main --force --yes"),
        Command::Update {
            id: "demo".to_string(),
            reference: Some("main".to_string()),
            force: true,
            assume_yes: true,
        }
    );
    assert_eq!(
        command("rollback demo"),
        Command::Rollback {
            id: "demo".to_string(),
            assume_yes: false,
        }
    );
    assert_eq!(
        command("rollback demo --yes"),
        Command::Rollback {
            id: "demo".to_string(),
            assume_yes: true,
        }
    );
    assert_eq!(
        command("remove demo"),
        Command::Remove {
            id: "demo".to_string(),
            purge: false,
            assume_yes: false,
        }
    );
    assert_eq!(
        command("remove demo --purge -y"),
        Command::Remove {
            id: "demo".to_string(),
            purge: true,
            assume_yes: true,
        }
    );
    assert_eq!(
        command("purge"),
        Command::Purge {
            identifier: None,
            assume_yes: false,
        }
    );
    assert_eq!(
        command("purge com.example.demo"),
        Command::Purge {
            identifier: Some("com.example.demo".to_string()),
            assume_yes: false,
        }
    );
    assert_eq!(
        command("purge com.example.demo --yes"),
        Command::Purge {
            identifier: Some("com.example.demo".to_string()),
            assume_yes: true,
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
            assume_yes: false,
        }
    );
    assert_eq!(
        command("import demo /tmp/demo.tar.gz --force -y"),
        Command::Import {
            id: "demo".to_string(),
            path: "/tmp/demo.tar.gz".to_string(),
            force: true,
            assume_yes: true,
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
            id: "demo".to_string(),
            alias: None,
        })
    );
    assert_eq!(
        command("run --stop demo console"),
        Command::Run(RunInvocation::Stop {
            id: "demo".to_string(),
            alias: Some("console".to_string()),
        })
    );
}

#[test]
fn run_stop_or_replace_with_no_id_lists_every_active_command() {
    // Plan 047 step 5: with no id, both typed forms list every active `run`
    // command across every installed app rather than refuse for want of one
    // — the usage line printed after it is which one the user actually
    // typed.
    assert_eq!(
        command("run --stop"),
        Command::Run(RunInvocation::ListActive {
            usage: "run --stop <id> [alias]",
        })
    );
    assert_eq!(
        command("run --replace"),
        Command::Run(RunInvocation::ListActive {
            usage: "run --replace <id> <alias> [args...]",
        })
    );
}

#[test]
fn run_with_no_alias_lists_the_apps_declared_aliases() {
    // The hub's own addition to the station's grammar: the station discovers
    // aliases through `--help`, which the hub cannot do since they belong to
    // an app and not to the binary.
    assert_eq!(
        command("run demo"),
        Command::Run(RunInvocation::List {
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
        ("open -- /tmp/a.md", "open <id> [-- <file>...]"),
        ("dev", "dev <local-path>"),
        ("dev one two", "dev <local-path>"),
        ("dev /tmp/demo -- /tmp/a.md", "dev <local-path>"),
        ("install", "install <source>"),
        ("install ../app --as", "install <source>"),
        ("list --all", "list"),
        ("purge one two", "purge [<identifier>] [--yes]"),
        ("export demo", "export <id> <path>"),
        ("import demo /tmp/x.tar.gz --purge", "import <id> <path>"),
        ("update demo --ref", "update <id>"),
        ("run", "run <id>"),
        ("run --stop demo console extra", "run <id>"),
        ("run --stop --replace demo console", "run <id>"),
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
fn every_declared_command_is_implemented() {
    // `export`/`import` were the last two `Availability::NotYet` entries
    // (plan 022) — nothing in `SURFACE` answers `EXIT_UNIMPLEMENTED` any
    // more. Kept as one test naming every command rather than reading
    // `SURFACE` itself, so a future regression to `NotYet` fails here rather
    // than silently changing what this test covers.
    for line in [
        "--version",
        "--help",
        "--update",
        "--rollback",
        "list",
        "install ../TFSAppTest",
        "open demo",
        "publish ../TFSAppTest",
        "update demo",
        "rollback demo",
        "remove demo",
        "purge",
        "export demo /tmp/demo",
        "import demo /tmp/demo.tar.gz",
        "run demo",
        "run demo console",
        "run --stop demo",
        "run --replace demo console",
    ] {
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
fn the_hidden_open_subcommand_stays_reachable_and_unlisted() {
    assert!(
        refusal("__platform")
            .message
            .contains("unknown command __platform"),
        "the retired platform probe must not stay parseable"
    );
    assert_eq!(
        command("__open --id demo --identity dev.local.demo --name Demo --icon /tmp/demo.png"),
        Command::OpenChild {
            source: OpenChildSource::Id("demo".to_string()),
            identifier: "dev.local.demo".to_string(),
            product_name: "Demo".to_string(),
            icon_path: Some("/tmp/demo.png".to_string()),
            files: Vec::new(),
        }
    );
    // `dev`'s own source, the same form otherwise — one hidden subcommand
    // serving both callers rather than a twin (see `OpenChildSource`).
    assert_eq!(
        command("__open --project /tmp/demo --identity dev.local.demo --name Demo"),
        Command::OpenChild {
            source: OpenChildSource::Project("/tmp/demo".to_string()),
            identifier: "dev.local.demo".to_string(),
            product_name: "Demo".to_string(),
            icon_path: None,
            files: Vec::new(),
        }
    );
    // `--identity`/`--name` are required and never defaulted: this form is
    // written by `open <id>` or `dev <path>` alone, so a missing one is a bug
    // in the parent and filling it in would hide exactly that.
    for (missing, invocation) in [
        ("--identity", "__open --id demo --name Demo"),
        ("--name", "__open --id demo --identity dev.local.demo"),
    ] {
        assert!(
            refusal(invocation).message.contains(missing),
            "{invocation:?} should name the missing {missing}"
        );
    }
    // Neither source, or both: refused rather than guessed at.
    assert!(refusal("__open --identity dev.local.demo --name Demo")
        .message
        .contains("--id or --project"));
    assert!(
        refusal("__open --id demo --project /tmp/demo --identity dev.local.demo --name Demo")
            .message
            .contains("not both")
    );

    // Hidden means hidden: a user who reads about one in `--help` will
    // reasonably expect it to be theirs to type, and neither of these is.
    for spec in SURFACE {
        assert!(
            !spec.name.starts_with("__"),
            "{} is a hidden subcommand and has no place in the surface",
            spec.name
        );
    }
}

#[test]
fn the_help_describes_every_form_the_grammar_has() {
    // The point of rendering the help from the surface table rather than
    // writing it out: a command added to the grammar and forgotten in the help
    // fails here instead of shipping undocumented.
    let help = help_text();

    for spec in SURFACE {
        assert!(
            help.contains(spec.form),
            "{:?} is missing from --help:\n{help}",
            spec.form
        );
        assert!(
            help.contains(spec.summary),
            "{:?} has no summary in --help:\n{help}",
            spec.form
        );
    }
}

#[test]
fn the_help_leads_with_the_rule_and_groups_by_subject() {
    let help = help_text();

    // The rule is what makes the rest of the list predictable rather than
    // memorised, so it comes before any of it.
    let rule = "--flags act on the hub. Bare words act on an app.";
    let hub_heading = help.find("The hub itself:").expect("a hub group");
    let app_heading = help.find("An app:").expect("an app group");
    assert!(
        help.find(rule).expect("the rule") < hub_heading,
        "the rule comes first:\n{help}"
    );
    assert!(hub_heading < app_heading, "hub before app:\n{help}");

    // Each form sits under the level it acts on — the rule stated a second
    // time, in layout.
    for spec in SURFACE {
        let at = help.find(spec.form).expect("a form");
        let expected = match spec.level {
            super::Level::Hub => hub_heading,
            super::Level::App => app_heading,
        };
        assert!(
            at > expected,
            "{:?} is grouped under the wrong subject:\n{help}",
            spec.form
        );
    }
}

#[test]
fn the_help_says_which_commands_do_not_work_yet() {
    // A help text that lists a command the hub refuses, without saying so, is
    // worse than one that omits it: the user types it and gets an error for
    // something they were just told they could do.
    let help = help_text();

    for line in help.lines() {
        for spec in SURFACE {
            if !line.contains(spec.summary) {
                continue;
            }
            let marked = line.contains(NOT_YET_MARKER);
            assert_eq!(
                marked,
                spec.availability == Availability::NotYet,
                "{:?}'s line disagrees with its availability: {line:?}",
                spec.form
            );
        }
    }
}

#[test]
fn the_version_has_one_source_and_it_is_cargo_toml() {
    // `--version` prints `tauri::Context`'s package info, which falls back to
    // Cargo.toml only while `tauri.conf.json` declares no `version` of its own.
    // A version added there would silently become a second one to keep in step
    // — including with the release tag `--update` will compare against.
    let config: serde_json::Value =
        serde_json::from_str(include_str!("../tauri.conf.json")).expect("tauri.conf.json parses");

    assert!(
        config.get("version").is_none(),
        "tauri.conf.json must not declare a version — Cargo.toml is the source"
    );
}

// --- open's file operands ------------------------------------------------

#[test]
fn a_file_bearing_open_keeps_its_operands_verbatim() {
    // The separator is the whole contract: after it, nothing is a flag, so
    // option-looking names and Unicode are one argument each, in the
    // invocation's own order — the order the request's paths keep.
    assert_eq!(
        command("open demo -- /tmp/a.md --help --résumé.md"),
        Command::Open {
            id: "demo".to_string(),
            files: vec![
                "/tmp/a.md".to_string(),
                "--help".to_string(),
                "--résumé.md".to_string()
            ],
        }
    );
    // Spaces are argv, not syntax: a path containing them arrives as one
    // argument and leaves as one path, quotes included.
    let spaced = [
        "open".to_string(),
        "demo".to_string(),
        "--".to_string(),
        "/tmp/my file.md".to_string(),
        "\"quoted.md\"".to_string(),
    ];
    assert_eq!(
        parse(&spaced),
        Ok(Command::Open {
            id: "demo".to_string(),
            files: vec!["/tmp/my file.md".to_string(), "\"quoted.md\"".to_string()],
        })
    );
    // A separator with nothing after it is the no-file form: a declaring
    // app's desktop entry ends in `-- %F`, and a bare menu launch expands
    // `%F` to zero arguments.
    assert_eq!(
        command("open demo --"),
        Command::Open {
            id: "demo".to_string(),
            files: Vec::new(),
        }
    );
}

#[test]
fn help_does_not_win_past_the_operand_separator() {
    // A file named `--help` is a file; the question `--help` asks has to come
    // before the caller's own operands start.
    assert_eq!(
        command("open demo -- --help"),
        Command::Open {
            id: "demo".to_string(),
            files: vec!["--help".to_string()],
        }
    );
    assert_eq!(
        command("install ../app --help"),
        Command::Help,
        "without a separator, --help keeps winning anywhere"
    );
}

#[test]
fn the_hidden_open_subcommand_carries_the_batch_after_its_separator() {
    assert_eq!(
        command("__open --id demo --identity dev.local.demo --name Demo -- /tmp/a.md b.md"),
        Command::OpenChild {
            source: OpenChildSource::Id("demo".to_string()),
            identifier: "dev.local.demo".to_string(),
            product_name: "Demo".to_string(),
            icon_path: None,
            files: vec!["/tmp/a.md".to_string(), "b.md".to_string()],
        }
    );
    // A dev session is not a file receiver: the separator is refused with the
    // reason, not by silently dropping the operands.
    let refusal =
        refusal("__open --project /tmp/demo --identity dev.local.demo --name Demo -- /tmp/a.md");
    assert!(
        refusal.message.contains("takes files only with --id"),
        "said {refusal}"
    );
}

#[test]
fn a_second_instance_argv_is_read_back_by_the_same_parser() {
    // The pinned plugin hands the live instance the second process's whole
    // argv, program name included — the same argv the parent wrote, read back
    // by the same parser.
    let argv = |line: &str| -> Vec<String> {
        std::iter::once("/path/to/tfsapp-hub".to_string())
            .chain(line.split_whitespace().map(str::to_string))
            .collect()
    };
    assert_eq!(
        second_instance_files(&argv(
            "__open --id demo --identity dev.local.demo --name Demo -- /tmp/a.md"
        )),
        vec!["/tmp/a.md".to_string()],
        "a file-bearing child argv is an arrival"
    );
    assert_eq!(
        second_instance_files(&argv(
            "__open --id demo --identity dev.local.demo --name Demo"
        )),
        Vec::<String>::new(),
        "a no-file child argv is a plain second window"
    );
    assert_eq!(
        second_instance_files(&argv(
            "__open --id demo --identity dev.local.demo --name Demo --"
        )),
        Vec::<String>::new(),
        "a trailing separator the desktop entry's zero-file %F expansion \
         leaves behind is still a plain second window"
    );
    assert_eq!(
        second_instance_files(&argv("__open --project /tmp/demo --identity dev.local.demo --name Demo -- /tmp/a.md")),
        Vec::<String>::new(),
        "a hand-typed dev-with-files argv never parses: the parser refuses it, and the window admission is the fallback"
    );
    assert_eq!(
        second_instance_files(&argv("list")),
        Vec::<String>::new(),
        "an argv that is not an __open child is no arrival at all"
    );
    assert_eq!(
        second_instance_files(&[]),
        Vec::<String>::new(),
        "an empty argv cannot be an arrival"
    );
}
