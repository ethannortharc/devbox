//! The one place a command says "which box".
//!
//! v3 spelled the box as `--name`, v4 shipped its new commands with a
//! positional, and the two lived side by side long enough that `devbox policy
//! show --name devtest` — a reasonable guess from the older half of the CLI —
//! was a parse error. Copying the same two fields into twenty argument structs
//! is how that drift happened, so there is exactly one definition of them here
//! and every command flattens it in.
//!
//! The positional is the documented form. `--name` stays as a hidden alias so
//! scripts written against v3 keep working; hidden because `--help` should
//! teach one spelling, and conflicting because a command line that says the box
//! twice has no obviously right answer.

use clap::Args;

/// The box a command acts on: `[NAME]`, with `--name` as a hidden legacy alias.
///
/// Flatten it *after* a command's own positionals (`devbox policy set open
/// [NAME]`), so that omitting the box name stays unambiguous. Where the command
/// has none, it lands first (`devbox status [NAME]`).
#[derive(Args, Debug, Default, Clone)]
pub struct BoxArg {
    /// Box name; defaults to the box registered for the current directory
    #[arg(value_name = "NAME")]
    pub name_pos: Option<String>,

    /// Deprecated spelling of the NAME positional; kept for pre-v5 scripts.
    #[arg(
        long = "name",
        value_name = "NAME",
        hide = true,
        conflicts_with = "name_pos"
    )]
    pub name_flag: Option<String>,
}

impl BoxArg {
    /// The box the user named, or `None` to let the manager resolve the
    /// current directory's box.
    ///
    /// The two fields conflict at parse time, so at most one is ever set and
    /// the order of this `or` never decides anything.
    pub fn name(&self) -> Option<&str> {
        self.name_pos.as_deref().or(self.name_flag.as_deref())
    }
}

/// `devbox snapshot`'s box argument.
///
/// Same shape as [`BoxArg`], plus the `--sandbox` spelling that only ever
/// existed here — `snapshot save NAME` had already taken `--name` for the
/// snapshot, so the box was `--sandbox`. It is hidden and conflicting on the
/// same terms as `--name`, rather than aliased onto [`BoxArg`], so that
/// `--sandbox` does not silently become valid on the other nineteen commands
/// that never accepted it.
#[derive(Args, Debug, Default, Clone)]
pub struct SnapshotBoxArg {
    #[command(flatten)]
    pub inner: BoxArg,

    /// Deprecated spelling of the NAME positional; kept for pre-v5 scripts.
    #[arg(
        long = "sandbox",
        value_name = "NAME",
        hide = true,
        conflicts_with_all = ["name_pos", "name_flag"]
    )]
    pub sandbox_flag: Option<String>,
}

impl SnapshotBoxArg {
    pub fn name(&self) -> Option<&str> {
        self.inner.name().or(self.sandbox_flag.as_deref())
    }
}

/// The help line every `[NAME]` shows.
///
/// The doc comment on [`BoxArg::name_pos`] is what clap actually renders; this
/// is the value the tests below hold it to, across every command that has one.
/// The whole point of the shared struct is that the description cannot drift
/// per command, and a constant is how that stays true after someone adds a
/// twenty-first command.
pub const BOX_NAME_HELP: &str =
    "Box name; defaults to the box registered for the current directory";

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use crate::cli::{Cli, Command};

    use super::BOX_NAME_HELP;

    /// One command that selects an existing box.
    struct Case {
        /// Everything before the box name, including the command's own
        /// positionals: `devbox policy set open [NAME]` puts `open` here.
        prefix: &'static [&'static str],
        /// Everything after it — required flags, and `exec`'s `-- CMD`.
        suffix: &'static [&'static str],
        /// The pre-v5 flag spellings this command still accepts.
        legacy: &'static [&'static str],
    }

    const fn case(
        prefix: &'static [&'static str],
        suffix: &'static [&'static str],
        legacy: &'static [&'static str],
    ) -> Case {
        Case {
            prefix,
            suffix,
            legacy,
        }
    }

    /// Every command whose box name went positional in v5.
    ///
    /// `devbox create` is absent on purpose: its `--name` names a box that does
    /// not exist yet, so it is not the same argument. `devbox use <NAME>`
    /// requires its box and was already positional. `devbox policy allow` and
    /// `devbox report` have their own tests below.
    ///
    /// `devbox secret` and `devbox broker status|start|stop` are absent
    /// because they name no box at all: the broker and its credentials are
    /// host state shared by every box, and only `broker reach` probes from
    /// inside one.
    ///
    /// This list is not maintained by hand any more than it has to be:
    /// `every_command_with_a_box_positional_is_covered` walks the real command
    /// tree and fails when a new subcommand flattens [`BoxArg`] without
    /// appearing here.
    fn cases() -> Vec<Case> {
        vec![
            case(&["shell"], &[], &["--name"]),
            case(&["exec"], &["--", "ls"], &["--name"]),
            case(&["stop"], &[], &["--name"]),
            case(&["destroy"], &[], &["--name"]),
            case(&["status"], &[], &["--name"]),
            case(&["upgrade"], &["--tools", "rust"], &["--name"]),
            case(&["commit"], &[], &["--name"]),
            case(&["diff"], &[], &["--name"]),
            case(&["discard"], &[], &["--name"]),
            case(&["reprovision"], &[], &["--name"]),
            case(&["code"], &[], &["--name"]),
            case(&["watch"], &[], &["--name"]),
            case(
                &["snapshot", "save", "snap1"],
                &[],
                &["--name", "--sandbox"],
            ),
            case(
                &["snapshot", "restore", "snap1"],
                &[],
                &["--name", "--sandbox"],
            ),
            case(&["snapshot", "list"], &[], &["--name", "--sandbox"]),
            case(&["nix", "add", "ripgrep"], &[], &["--name"]),
            case(&["nix", "remove", "ripgrep"], &[], &["--name"]),
            case(&["layer", "status"], &[], &["--name"]),
            case(&["layer", "diff"], &[], &["--name"]),
            case(&["layer", "commit"], &[], &["--name"]),
            case(&["layer", "discard"], &[], &["--name"]),
            case(&["layer", "refresh"], &[], &["--name"]),
            case(&["layer", "conflicts"], &[], &["--name"]),
            case(&["layer", "stash"], &[], &["--name"]),
            case(&["layer", "stash-pop"], &[], &["--name"]),
            case(&["layer", "checkpoint"], &[], &["--name"]),
            case(&["layer", "checkpoints"], &[], &["--name"]),
            // The checkpoint id is the first positional on both of these, so
            // the box is the second one — the `snapshot save` shape.
            case(&["layer", "restore", "01kfx9m2"], &[], &["--name"]),
            case(&["layer", "checkpoint-rm", "01kfx9m2"], &[], &["--name"]),
            case(&["layer", "prune"], &[], &["--name"]),
            // `run`'s trailing command is `last = true`, so the box still has
            // to be readable from in front of the `--`.
            case(&["run"], &["--", "true"], &["--name"]),
            case(&["runs"], &[], &["--name"]),
            case(&["store", "redact"], &[], &["--name"]),
            case(&["export"], &["--format", "jsonl"], &["--name"]),
            case(&["broker", "reach"], &[], &["--name"]),
            case(&["repair", "stale-home"], &[], &["--name"]),
            case(&["sets", "list"], &[], &["--name"]),
            case(&["sets", "apply"], &["--set", "system"], &["--name"]),
            case(&["behavior", "summary"], &[], &["--name"]),
            case(
                &["behavior", "diff"],
                &["--from", "2026-01-01T00:00:00Z"],
                &["--name"],
            ),
            case(
                &["behavior", "pcap"],
                &["--proto", "tcp", "--daddr", "192.0.2.7", "--dport", "443"],
                &["--name"],
            ),
            case(&["policy", "show"], &[], &["--name"]),
            case(&["policy", "rules"], &[], &["--name"]),
            case(&["policy", "set", "open"], &[], &["--name"]),
            case(&["policy", "test", "example.com"], &[], &["--name"]),
        ]
    }

    impl Case {
        fn label(&self) -> String {
            format!("devbox {}", self.prefix.join(" "))
        }

        /// The subcommand path inside `prefix`, dropping the command's own
        /// positional values (`policy set open` is the command `policy set`).
        fn command_path(&self) -> &'static [&'static str] {
            let mut depth = 0;
            let mut current = Cli::command();
            for segment in self.prefix {
                let Some(next) = current
                    .get_subcommands()
                    .find(|c| c.get_name() == *segment)
                    .cloned()
                else {
                    break;
                };
                current = next;
                depth += 1;
            }
            &self.prefix[..depth]
        }

        fn argv(&self, middle: &[&str]) -> Vec<String> {
            std::iter::once("devbox")
                .chain(self.prefix.iter().copied())
                .chain(middle.iter().copied())
                .chain(self.suffix.iter().copied())
                .map(str::to_string)
                .collect()
        }
    }

    /// The box the parsed command will hand to `SandboxManager::resolve_name`.
    ///
    /// Reading it back out of the parsed command — rather than only asserting
    /// that the line parses — is what catches a box name that landed in the
    /// wrong slot, such as `snapshot save nightly devtest` binding `nightly`
    /// as the box.
    fn selected_box(argv: &[String]) -> Option<String> {
        use crate::cli::{behavior, broker, nix_cmd, policy, repair, sets, snapshot, store};

        let cli = Cli::try_parse_from(argv).expect("argv should parse");
        match cli.command.expect("a subcommand") {
            Command::Shell(a) => a.boxarg.name().map(str::to_string),
            Command::Exec(a) => a.boxarg.name().map(str::to_string),
            Command::Stop(a) => a.boxarg.name().map(str::to_string),
            Command::Destroy(a) => a.boxarg.name().map(str::to_string),
            Command::Status(a) => a.boxarg.name().map(str::to_string),
            Command::Upgrade(a) => a.boxarg.name().map(str::to_string),
            Command::Commit(a) => a.boxarg.name().map(str::to_string),
            Command::Diff(a) => a.boxarg.name().map(str::to_string),
            Command::Discard(a) => a.boxarg.name().map(str::to_string),
            Command::Reprovision(a) => a.boxarg.name().map(str::to_string),
            Command::Run(a) => a.boxarg.name().map(str::to_string),
            Command::Runs(a) => a.boxarg.name().map(str::to_string),
            Command::Export(a) => a.boxarg.name().map(str::to_string),
            Command::Repair(a) => match a.command {
                repair::RepairCommand::StaleHome(a) => a.boxarg.name().map(str::to_string),
            },
            Command::BrokerCmd(a) => match a.command {
                broker::BrokerCommand::Reach(a) => a.boxarg.name().map(str::to_string),
                other => panic!("devbox broker {other:?} does not select a box"),
            },
            Command::Code(a) => a.boxarg.name().map(str::to_string),
            Command::Watch(a) => a.boxarg.name().map(str::to_string),
            Command::Snapshot(a) => match a.action {
                snapshot::SnapshotAction::Save { boxarg, .. }
                | snapshot::SnapshotAction::Restore { boxarg, .. }
                | snapshot::SnapshotAction::List { boxarg } => boxarg.name().map(str::to_string),
            },
            Command::Nix(a) => match a.action {
                nix_cmd::NixAction::Add { boxarg, .. }
                | nix_cmd::NixAction::Remove { boxarg, .. } => boxarg.name().map(str::to_string),
            },
            Command::Layer(a) => a.action.boxarg().name().map(str::to_string),
            Command::Store(a) => match a.command {
                store::StoreCommand::Redact(a) => a.boxarg.name().map(str::to_string),
            },
            Command::Sets(a) => match a.command {
                sets::SetsCommand::List(a) => a.boxarg.name().map(str::to_string),
                sets::SetsCommand::Apply(a) => a.boxarg.name().map(str::to_string),
            },
            Command::Behavior(a) => match a.command {
                behavior::BehaviorCommand::Summary(a) => a.boxarg.name().map(str::to_string),
                behavior::BehaviorCommand::Diff(a) => a.boxarg.name().map(str::to_string),
                behavior::BehaviorCommand::Pcap(a) => a.boxarg.name().map(str::to_string),
            },
            Command::Policy(a) => match a.command {
                policy::PolicyCommand::Show(a) | policy::PolicyCommand::Rules(a) => {
                    a.boxarg.name().map(str::to_string)
                }
                policy::PolicyCommand::Set(a) => a.boxarg.name().map(str::to_string),
                policy::PolicyCommand::Test(a) => a.boxarg.name().map(str::to_string),
                policy::PolicyCommand::Allow(a) => a.name,
            },
            other => panic!("{other:?} does not select an existing box"),
        }
    }

    #[test]
    fn every_box_command_takes_the_name_as_a_positional() {
        for case in cases() {
            let argv = case.argv(&["devtest"]);
            assert_eq!(
                selected_box(&argv).as_deref(),
                Some("devtest"),
                "{} did not read the positional box name from {argv:?}",
                case.label(),
            );
        }
    }

    #[test]
    fn every_box_command_still_accepts_the_legacy_flag() {
        for case in cases() {
            for flag in case.legacy {
                let argv = case.argv(&[flag, "devtest"]);
                assert_eq!(
                    selected_box(&argv).as_deref(),
                    Some("devtest"),
                    "{} lost the {flag} alias; argv was {argv:?}",
                    case.label(),
                );
            }
        }
    }

    #[test]
    fn naming_the_box_twice_is_an_error() {
        for case in cases() {
            for flag in case.legacy {
                let argv = case.argv(&["devtest", flag, "other"]);
                let error = Cli::try_parse_from(&argv).expect_err(&format!(
                    "{} accepted two box names: {argv:?}",
                    case.label()
                ));
                assert_eq!(
                    error.kind(),
                    clap::error::ErrorKind::ArgumentConflict,
                    "{} rejected {argv:?} for the wrong reason: {error}",
                    case.label(),
                );
            }
        }
    }

    #[test]
    fn omitting_the_box_leaves_it_for_the_current_directory() {
        for case in cases() {
            let argv = case.argv(&[]);
            assert_eq!(
                selected_box(&argv),
                None,
                "{} invented a box name from {argv:?}",
                case.label(),
            );
        }
    }

    /// `exec` is the one command whose box name shares a line with a trailing
    /// command, so the `--` boundary is what makes it unambiguous.
    /// `devbox stop` repairs a box that could not start again, and refuses if
    /// that repair fails. `--force` is the only way past it, so it has to be
    /// spelled out and it has to default to off.
    #[test]
    fn stop_needs_force_spelled_out() {
        let parse = |argv: &[&str]| {
            let full: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            let cli = Cli::try_parse_from(&full).expect("argv should parse");
            let Some(Command::Stop(args)) = cli.command else {
                panic!("not a stop");
            };
            args.force
        };
        assert!(!parse(&["devbox", "stop", "devtest"]));
        assert!(parse(&["devbox", "stop", "devtest", "--force"]));
    }

    #[test]
    fn exec_keeps_its_bare_form() {
        let argv: Vec<String> = ["devbox", "exec", "--", "ls", "-la"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let cli = Cli::try_parse_from(&argv).expect("devbox exec -- ls -la");
        let Some(Command::Exec(args)) = cli.command else {
            panic!("not an exec");
        };
        assert_eq!(args.boxarg.name(), None);
        assert_eq!(args.command, vec!["ls".to_string(), "-la".to_string()]);
    }

    #[test]
    fn exec_reads_the_box_before_the_dash_dash() {
        let argv: Vec<String> = ["devbox", "exec", "devtest", "--", "make", "test"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let cli = Cli::try_parse_from(&argv).expect("devbox exec devtest -- make test");
        let Some(Command::Exec(args)) = cli.command else {
            panic!("not an exec");
        };
        assert_eq!(args.boxarg.name(), Some("devtest"));
        assert_eq!(args.command, vec!["make".to_string(), "test".to_string()]);
    }

    /// `snapshot save NAME` had already taken the first positional, so the box
    /// is the second one — and must not steal the snapshot's name.
    #[test]
    fn snapshot_keeps_its_own_name_in_the_first_slot() {
        use crate::cli::snapshot::SnapshotAction;

        let argv: Vec<String> = ["devbox", "snapshot", "save", "nightly", "devtest"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let cli = Cli::try_parse_from(&argv).expect("devbox snapshot save nightly devtest");
        let Some(Command::Snapshot(args)) = cli.command else {
            panic!("not a snapshot");
        };
        let SnapshotAction::Save { snapshot, boxarg } = args.action else {
            panic!("not a save");
        };
        assert_eq!(snapshot, "nightly");
        assert_eq!(boxarg.name(), Some("devtest"));
    }

    /// `policy allow` is the documented exception: a required variadic
    /// `<ENTRIES>...` leaves no room for an optional positional, so its box
    /// stays a visible `--name`. If that ever changes, this test should be the
    /// thing that notices.
    #[test]
    fn policy_allow_keeps_a_visible_name_flag() {
        let argv: Vec<String> = [
            "devbox",
            "policy",
            "allow",
            "--name",
            "devtest",
            "example.com",
            "pypi.org",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(selected_box(&argv).as_deref(), Some("devtest"));

        let bare: Vec<String> = ["devbox", "policy", "allow", "example.com"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(selected_box(&bare), None);

        let flag = find_subcommand(&Cli::command(), &["policy", "allow"])
            .get_arguments()
            .find(|a| a.get_id() == "name")
            .expect("policy allow keeps --name")
            .clone();
        assert!(
            !flag.is_hide_set(),
            "--name is the only form here, so it has to be visible"
        );
        assert_eq!(
            flag.get_help().map(|h| h.to_string()).as_deref(),
            Some(BOX_NAME_HELP),
        );
    }

    /// `devbox report <RUN_ID>` is the second documented exception.
    ///
    /// Its `--name` looks like the others and is not: omitting it means
    /// "search every box for this run", not "use the box registered for the
    /// current directory". Turning it into a `[NAME]` positional would make
    /// [`BOX_NAME_HELP`] — which is the shared promise — a lie, so it keeps a
    /// visible flag with its own description. If the default ever becomes the
    /// current directory's box, this test is what should stop working.
    #[test]
    fn report_keeps_its_own_name_flag_because_its_default_is_different() {
        let cmd = find_subcommand(&Cli::command(), &["report"]);
        assert!(
            cmd.get_arguments().all(|a| a.get_id() != "name_pos"),
            "report grew a [NAME] positional; it needs a case() entry, not an exception"
        );
        let flag = cmd
            .get_arguments()
            .find(|a| a.get_id() == "name")
            .expect("report keeps --name");
        assert!(!flag.is_hide_set(), "--name is the only form here");
        assert_ne!(
            flag.get_help().map(|h| h.to_string()).as_deref(),
            Some(BOX_NAME_HELP),
            "report's default is not the current directory's box, so it must not \
             claim the shared description",
        );
    }

    /// The one guard that does not need updating when a command is added.
    ///
    /// The hand-written list above is the thing that drifts — `layer
    /// checkpoint`, `run`, `runs`, `export` and `broker reach` all shipped
    /// without it, so the shared-argument tests silently covered less of the
    /// CLI every release. This walks the command tree instead: anything that
    /// flattens `BoxArg` gets a `name_pos`, and anything with a `name_pos`
    /// must appear in `cases()`.
    #[test]
    fn every_command_with_a_box_positional_is_covered() {
        fn walk(cmd: &clap::Command, path: Vec<String>, found: &mut Vec<Vec<String>>) {
            if cmd.get_arguments().any(|a| a.get_id() == "name_pos") {
                found.push(path.clone());
            }
            for sub in cmd.get_subcommands() {
                let mut deeper = path.clone();
                deeper.push(sub.get_name().to_string());
                walk(sub, deeper, found);
            }
        }

        let root = Cli::command();
        let mut found: Vec<Vec<String>> = vec![];
        for sub in root.get_subcommands() {
            walk(sub, vec![sub.get_name().to_string()], &mut found);
        }

        let covered: Vec<Vec<String>> = cases()
            .iter()
            .map(|c| c.command_path().iter().map(|s| s.to_string()).collect())
            .collect();

        let missing: Vec<String> = found
            .iter()
            .filter(|path| !covered.contains(path))
            .map(|path| format!("devbox {}", path.join(" ")))
            .collect();
        assert!(
            missing.is_empty(),
            "these commands take a [NAME] positional but are not in cases(): {missing:?}"
        );
    }

    fn find_subcommand(root: &clap::Command, path: &[&str]) -> clap::Command {
        let mut current = root.clone();
        for segment in path {
            let next = current
                .get_subcommands()
                .find(|c| c.get_name() == *segment)
                .unwrap_or_else(|| panic!("no subcommand {segment} under {}", current.get_name()))
                .clone();
            current = next;
        }
        current
    }

    /// Item 4 of the brief: one description, everywhere.
    #[test]
    fn the_name_argument_reads_the_same_in_every_help() {
        let root = Cli::command();
        for case in cases() {
            let cmd = find_subcommand(&root, case.command_path());
            let arg = cmd
                .get_arguments()
                .find(|a| a.get_id() == "name_pos")
                .unwrap_or_else(|| panic!("{} has no [NAME]", case.label()));
            assert_eq!(
                arg.get_help().map(|h| h.to_string()).as_deref(),
                Some(BOX_NAME_HELP),
                "{} describes its box name differently",
                case.label(),
            );
            assert_eq!(
                arg.get_value_names().map(|v| v.to_vec()),
                Some(vec!["NAME".into()]),
                "{} does not render its box name as [NAME]",
                case.label(),
            );

            let legacy = cmd
                .get_arguments()
                .find(|a| a.get_id() == "name_flag")
                .unwrap_or_else(|| panic!("{} dropped the --name alias", case.label()));
            assert!(
                legacy.is_hide_set(),
                "{} advertises the deprecated --name in its help",
                case.label(),
            );
        }
    }
}
