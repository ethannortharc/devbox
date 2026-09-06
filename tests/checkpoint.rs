//! Overlay checkpoints: the parts that do not need a VM.
//!
//! Everything a checkpoint gets wrong, it gets wrong in one of three places —
//! reading the guest's tree listing, deciding what changed between two trees,
//! and choosing what to throw away. All three are pure functions here, tested
//! against output captured verbatim from the devtest guest (NixOS, kernel
//! 6.19, GNU findutils 4.10.0, coreutils 9.8) rather than from imagination.

use devbox::obs::run::ActiveRun;
use devbox::sandbox::checkpoint::{
    Checkpoint, CheckpointId, RunPin, diff_trees, expired_run_pins, parse_age, parse_manifests,
    prune_plan, refuse_pinned_delete, refuse_while_running, resolve_id,
};
use devbox::sandbox::overlay::{
    ChangeStatus, EntryKind, OverlayChange, TreeEntry, parse_tree_listing,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A regular file entry.
fn file(path: &str, size: u64, mtime: &str) -> TreeEntry {
    TreeEntry {
        path: path.into(),
        kind: EntryKind::File,
        size,
        mtime: mtime.into(),
        is_whiteout: false,
        is_opaque: false,
    }
}

/// A directory entry. Its own size and mtime are deliberately arbitrary: the
/// diff must not read them.
fn dir(path: &str, opaque: bool) -> TreeEntry {
    TreeEntry {
        path: path.into(),
        kind: EntryKind::Dir,
        size: 4096,
        mtime: "1788594193.0984610150".into(),
        is_whiteout: false,
        is_opaque: opaque,
    }
}

/// An OverlayFS whiteout: a character device with rdev 0/0.
fn whiteout(path: &str) -> TreeEntry {
    TreeEntry {
        path: path.into(),
        kind: EntryKind::Char,
        size: 0,
        mtime: "1788594193.1031617900".into(),
        is_whiteout: true,
        is_opaque: false,
    }
}

/// The (status, path) pairs a diff produced, in order.
fn summarize(changes: &[OverlayChange]) -> Vec<(ChangeStatus, &str)> {
    changes
        .iter()
        .map(|c| (c.status.clone(), c.path.as_str()))
        .collect()
}

fn manifest(id: &str, run_id: Option<&str>) -> Checkpoint {
    Checkpoint {
        id: CheckpointId::parse(id).expect("a valid id"),
        label: None,
        created_at: "2026-09-05T07:42:11Z".into(),
        run_id: run_id.map(str::to_string),
        files: 0,
        bytes: 0,
    }
}

fn live_run(id: &str) -> ActiveRun {
    ActiveRun {
        run_id: id.to_string(),
        cgroup_id: 17557,
        root_pid: 900,
        started_at: "2026-09-05T07:42:11.000Z".into(),
        ended_at: None,
    }
}

// ---------------------------------------------------------------------------
// Reading the guest's tree listing
// ---------------------------------------------------------------------------

/// Captured from devtest by running the three passes against a tree holding a
/// whiteout, a genuine character device, an opaque directory, a plain
/// directory and a file.
const LISTING: &str = "E\tc\t0\t1788594193.1031617900\tgone
E\td\t4096\t1788594193.0984610150\tsub
E\tc\t0\t1788594193.1031617900\trealdev
E\td\t4096\t1788594193.1031617900\topaquedir
E\tf\t6\t1788594193.1031617900\tf.txt
W 0 0 /tmp/cpprobe/tree/gone
W 1 3 /tmp/cpprobe/tree/realdev
O /tmp/cpprobe/tree/opaquedir
";

#[test]
fn the_listing_parses_into_entries() {
    let entries = parse_tree_listing(LISTING, "/tmp/cpprobe/tree");
    let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, vec!["gone", "sub", "realdev", "opaquedir", "f.txt"]);

    let f = entries.iter().find(|e| e.path == "f.txt").expect("f.txt");
    assert_eq!(f.kind, EntryKind::File);
    assert_eq!(f.size, 6);
    assert_eq!(f.mtime, "1788594193.1031617900");
}

/// The whole reason the listing takes three passes: `find -printf '%y'` calls
/// a whiteout and a real device node the same thing, and only the rdev tells
/// them apart. Getting this wrong would report `/dev/null` copied into the
/// workspace as a deletion.
#[test]
fn only_a_zero_zero_character_device_is_a_whiteout() {
    let entries = parse_tree_listing(LISTING, "/tmp/cpprobe/tree");

    let gone = entries.iter().find(|e| e.path == "gone").expect("gone");
    assert_eq!(gone.kind, EntryKind::Char);
    assert!(gone.is_whiteout, "rdev 0/0 is a whiteout");

    let real = entries
        .iter()
        .find(|e| e.path == "realdev")
        .expect("realdev");
    assert_eq!(real.kind, EntryKind::Char);
    assert!(!real.is_whiteout, "rdev 1/3 is /dev/null, not a whiteout");
}

#[test]
fn opaque_directories_are_marked_and_others_are_not() {
    let entries = parse_tree_listing(LISTING, "/tmp/cpprobe/tree");
    assert!(
        entries
            .iter()
            .find(|e| e.path == "opaquedir")
            .expect("opaquedir")
            .is_opaque
    );
    assert!(
        !entries
            .iter()
            .find(|e| e.path == "sub")
            .expect("sub")
            .is_opaque
    );
}

/// The path is the last field of every record precisely so that this works.
#[test]
fn paths_with_spaces_survive_every_pass() {
    let listing = "E\tf\t3\t1.0\tmy notes.txt
E\tc\t0\t2.0\ta b/c d
E\td\t4096\t3.0\ta b
W 0 0 /root/a b/c d
O /root/a b
";
    let entries = parse_tree_listing(listing, "/root");
    let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, vec!["my notes.txt", "a b/c d", "a b"]);
    assert!(entries[1].is_whiteout);
    assert!(entries[2].is_opaque);
}

#[test]
fn an_empty_tree_parses_to_nothing() {
    assert!(parse_tree_listing("", "/var/devbox/overlay/upper").is_empty());
}

// ---------------------------------------------------------------------------
// Diffing two trees
// ---------------------------------------------------------------------------

#[test]
fn a_file_only_in_the_newer_tree_is_added() {
    let from = vec![file("kept.txt", 4, "10.0")];
    let to = vec![file("kept.txt", 4, "10.0"), file("cp-1.txt", 2, "20.0")];
    assert_eq!(
        summarize(&diff_trees(&from, &to)),
        vec![(ChangeStatus::Added, "cp-1.txt")]
    );
}

#[test]
fn a_rewritten_file_is_modified_even_when_its_size_is_unchanged() {
    let from = vec![file("a.txt", 2, "10.0000000000")];
    let to = vec![file("a.txt", 2, "20.0000000000")];
    assert_eq!(
        summarize(&diff_trees(&from, &to)),
        vec![(ChangeStatus::Modified, "a.txt")],
        "same length, different content: only the mtime shows it"
    );
}

/// `cp -a` preserves the mtime to the nanosecond, so a checkpoint compares
/// equal to the upper it was taken from and a fresh checkpoint diffs clean.
#[test]
fn an_untouched_file_is_not_reported() {
    let tree = vec![file("a.txt", 2, "1788594193.1031617900")];
    assert!(diff_trees(&tree, &tree).is_empty());
}

/// A file the box created lives only in the upper, so losing it from the upper
/// means it is gone from `/workspace`.
#[test]
fn a_file_dropped_from_the_upper_is_deleted() {
    let from = vec![file("from-agent.txt", 5, "10.0")];
    let to = vec![];
    assert_eq!(
        summarize(&diff_trees(&from, &to)),
        vec![(ChangeStatus::Deleted, "from-agent.txt")]
    );
}

/// Deleting a file that came from the host does not remove anything from the
/// upper — it *adds* a whiteout.
#[test]
fn a_new_whiteout_is_a_deletion() {
    let from = vec![];
    let to = vec![whiteout("go.mod")];
    assert_eq!(
        summarize(&diff_trees(&from, &to)),
        vec![(ChangeStatus::Deleted, "go.mod")]
    );
}

/// Undoing a deletion lets the host's file show through again, which is an
/// addition as far as `/workspace` is concerned.
#[test]
fn a_whiteout_that_goes_away_is_an_addition() {
    let from = vec![whiteout("go.mod")];

    assert_eq!(
        summarize(&diff_trees(&from, &[])),
        vec![(ChangeStatus::Added, "go.mod")],
        "the lower layer shows through again"
    );
    assert_eq!(
        summarize(&diff_trees(&from, &[file("go.mod", 24, "30.0")])),
        vec![(ChangeStatus::Added, "go.mod")],
        "or the box wrote its own copy"
    );
}

#[test]
fn a_whiteout_that_stays_is_not_a_change() {
    let tree = vec![whiteout("go.mod")];
    assert!(diff_trees(&tree, &tree).is_empty());
}

/// `rm -rf somedir` on a directory that also exists on the host makes the
/// upper's copy opaque. Nothing else about the directory moves, so this is the
/// one directory attribute the diff reads.
#[test]
fn a_directory_that_becomes_opaque_is_modified() {
    let from = vec![dir("vendor", false)];
    let to = vec![dir("vendor", true)];
    let changes = diff_trees(&from, &to);
    assert_eq!(
        summarize(&changes),
        vec![(ChangeStatus::Modified, "vendor")]
    );
    assert!(changes[0].is_dir, "the change is on a directory");
}

/// A directory's own mtime moves whenever a child is written. Reporting that
/// as a change would put a `~ src` line above every real edit.
#[test]
fn a_directory_whose_children_changed_is_not_itself_a_change() {
    let from = vec![
        TreeEntry {
            mtime: "10.0".into(),
            ..dir("src", false)
        },
        file("src/a.txt", 2, "10.0"),
    ];
    let to = vec![
        TreeEntry {
            mtime: "99.0".into(),
            size: 8192,
            ..dir("src", false)
        },
        file("src/a.txt", 9, "99.0"),
    ];
    assert_eq!(
        summarize(&diff_trees(&from, &to)),
        vec![(ChangeStatus::Modified, "src/a.txt")]
    );
}

#[test]
fn a_new_directory_is_added_and_a_removed_one_is_deleted() {
    let from = vec![dir("old", false)];
    let to = vec![dir("new", false)];
    assert_eq!(
        summarize(&diff_trees(&from, &to)),
        vec![(ChangeStatus::Added, "new"), (ChangeStatus::Deleted, "old")]
    );
}

/// A path that changes what it *is* — a file replaced by a directory, say — is
/// a change even when nothing else about it moved.
#[test]
fn swapping_a_file_for_a_directory_is_modified() {
    let from = vec![file("thing", 4096, "10.0")];
    let to = vec![dir("thing", false)];
    assert_eq!(
        summarize(&diff_trees(&from, &to)),
        vec![(ChangeStatus::Modified, "thing")]
    );
}

/// Everything at once, several levels down, in sorted order.
#[test]
fn a_nested_tree_diffs_end_to_end() {
    let from = vec![
        dir("src", false),
        dir("src/deep", false),
        file("src/deep/kept.txt", 3, "10.0"),
        file("src/deep/edited.txt", 3, "10.0"),
        file("src/deep/vanished.txt", 3, "10.0"),
        dir("vendor", false),
    ];
    let to = vec![
        dir("src", false),
        dir("src/deep", false),
        file("src/deep/kept.txt", 3, "10.0"),
        file("src/deep/edited.txt", 7, "20.0"),
        file("src/deep/added.txt", 1, "20.0"),
        dir("vendor", true),
        whiteout("README.md"),
    ];

    assert_eq!(
        summarize(&diff_trees(&from, &to)),
        vec![
            (ChangeStatus::Deleted, "README.md"),
            (ChangeStatus::Added, "src/deep/added.txt"),
            (ChangeStatus::Modified, "src/deep/edited.txt"),
            (ChangeStatus::Deleted, "src/deep/vanished.txt"),
            (ChangeStatus::Modified, "vendor"),
        ]
    );
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// The whole point of the leading timestamp: `list` sorts on the string.
#[test]
fn ids_sort_by_creation_time() {
    let earlier = CheckpointId::at(1_788_594_193_000, 0xffff_ffff);
    let later = CheckpointId::at(1_788_594_193_001, 0);
    assert!(earlier < later, "{earlier} should sort before {later}");
}

#[test]
fn ids_taken_in_the_same_millisecond_differ() {
    let a = CheckpointId::at(1_788_594_193_000, 1);
    let b = CheckpointId::at(1_788_594_193_000, 2);
    assert_ne!(a, b);
}

#[test]
fn an_id_is_short_and_free_of_ambiguous_letters() {
    let id = CheckpointId::generate().to_string();
    assert_eq!(id.len(), 14, "{id} should be 10 time + 4 random characters");
    for bad in ['i', 'l', 'o', 'u'] {
        assert!(!id.contains(bad), "{id} contains the ambiguous '{bad}'");
    }
}

/// Ids are interpolated straight into guest shell commands, so the parser is
/// the boundary that keeps a shell out of them.
#[test]
fn only_the_id_alphabet_parses() {
    for good in ["01kfx9m2q78a4f", "01kfx9", "0"] {
        assert!(CheckpointId::parse(good).is_ok(), "{good} should parse");
    }
    for bad in [
        "",
        "  ",
        "../../etc",
        "01kfx9'; rm -rf /; '",
        "01kfx9m2q78a4f/upper",
        "01KFX9M2Q78A4F",
        "id-with-i",
        "01kfx9m2q78a4f01kfx9m2q78a4f01kfx9m2q78a4f",
    ] {
        assert!(
            CheckpointId::parse(bad).is_err(),
            "{bad:?} should not parse as an id"
        );
    }
}

#[test]
fn a_unique_prefix_resolves_and_an_ambiguous_one_does_not() {
    let known = vec![
        manifest("01kfx9m2q78a4f", None),
        manifest("01kfx9m3zz1111", None),
    ];

    let full = CheckpointId::parse("01kfx9m2q78a4f").unwrap();
    assert_eq!(resolve_id(&known, &full).unwrap(), full);

    let unique = CheckpointId::parse("01kfx9m2").unwrap();
    assert_eq!(resolve_id(&known, &unique).unwrap(), full);

    let ambiguous = CheckpointId::parse("01kfx9m").unwrap();
    let error = resolve_id(&known, &ambiguous).unwrap_err().to_string();
    assert!(error.contains("matches 2 checkpoints"), "{error}");

    let missing = CheckpointId::parse("zzzz").unwrap();
    assert!(resolve_id(&known, &missing).is_err());
}

// ---------------------------------------------------------------------------
// Manifests and retention
// ---------------------------------------------------------------------------

/// A half-written checkpoint, or a stray file someone dropped in the
/// directory, must not sink the whole listing.
#[test]
fn manifests_parse_and_junk_is_skipped() {
    let stdout = concat!(
        r#"{"id":"01kfx9m2q78a4f","label":"base","created_at":"2026-09-05T07:42:11Z","run_id":null,"files":3,"bytes":12}"#,
        "\n",
        "not json at all\n",
        "\n",
        r#"{"id":"01kfx9m3zz1111","label":null,"created_at":"2026-09-05T07:43:02Z","run_id":"run-7","files":0,"bytes":0}"#,
        "\n",
    );

    let parsed = parse_manifests(stdout);
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].label.as_deref(), Some("base"));
    assert_eq!(parsed[0].files, 3);
    assert_eq!(parsed[0].bytes, 12);
    assert_eq!(parsed[0].run_id, None);
    assert_eq!(parsed[1].run_id.as_deref(), Some("run-7"));
}

/// A label with a newline in it would close the heredoc that writes the
/// manifest into the guest. Compact JSON escapes it, which is why the manifest
/// is written with `to_string` and not `to_string_pretty`.
#[test]
fn a_manifest_is_one_line_whatever_the_label_says() {
    let awkward = Checkpoint {
        id: CheckpointId::parse("01kfx9m2q78a4f").unwrap(),
        label: Some("first\nDEVBOX_CP_EOF\nrm -rf /".into()),
        created_at: "2026-09-05T07:42:11Z".into(),
        run_id: None,
        files: 1,
        bytes: 2,
    };
    let json = serde_json::to_string(&awkward).unwrap();
    assert!(!json.contains('\n'), "{json} would break the heredoc");
    assert_eq!(parse_manifests(&json)[0], awkward);
}

#[test]
fn prune_keeps_the_newest_and_drops_the_rest() {
    let checkpoints: Vec<Checkpoint> = ["01kfx9m1", "01kfx9m2", "01kfx9m3", "01kfx9m4"]
        .iter()
        .map(|id| manifest(id, None))
        .collect();

    let doomed: Vec<String> = prune_plan(&checkpoints, 2)
        .iter()
        .map(|id| id.to_string())
        .collect();
    assert_eq!(doomed, vec!["01kfx9m1", "01kfx9m2"]);

    assert!(prune_plan(&checkpoints, 4).is_empty());
    assert!(prune_plan(&checkpoints, 20).is_empty());
    assert_eq!(prune_plan(&checkpoints, 0).len(), 4);
}

/// A checkpoint a run report links to outlives retention, and it does not eat
/// into the budget either — otherwise pinning one would quietly evict an
/// unpinned one that the user could still have restored.
#[test]
fn a_checkpoint_pinned_to_a_run_is_never_pruned() {
    let checkpoints = vec![
        manifest("01kfx9m1", Some("run-1")),
        manifest("01kfx9m2", None),
        manifest("01kfx9m3", Some("run-2")),
        manifest("01kfx9m4", None),
        manifest("01kfx9m5", None),
    ];

    let doomed: Vec<String> = prune_plan(&checkpoints, 2)
        .iter()
        .map(|id| id.to_string())
        .collect();
    assert_eq!(doomed, vec!["01kfx9m2"]);
}

/// `checkpoint-rm` names one checkpoint, so it cannot skip a pinned one the
/// way `prune` does — it has to say no.
#[test]
fn deleting_a_checkpoint_a_run_depends_on_is_refused() {
    let pinned = manifest("01kfx9m1", Some("01kfx9m1zzzz"));
    let error = refuse_pinned_delete("devtest", &pinned)
        .expect_err("a run's evidence is not deleted by accident");
    let message = error.to_string();
    // The run id and the way out both have to be in the message: a refusal
    // that does not say which run, or how to insist, sends the reader nowhere.
    assert!(message.contains("01kfx9m1zzzz"), "{message}");
    assert!(message.contains("--force"), "{message}");
    assert!(message.contains("devtest"), "{message}");
    // rustfmt rejoins a `\`-continued literal and leaves the continuation's
    // indentation in the text; this is the assertion that notices.
    assert!(!message.contains("  "), "{message}");
}

#[test]
fn an_unpinned_checkpoint_is_deleted_without_argument() {
    assert!(refuse_pinned_delete("devtest", &manifest("01kfx9m2", None)).is_ok());
}

// ---------------------------------------------------------------------------
// The command line
// ---------------------------------------------------------------------------

mod cli {
    use clap::Parser;
    use devbox::cli::layer::LayerAction;
    use devbox::cli::{Cli, Command};

    fn layer(argv: &[&str]) -> LayerAction {
        let full: Vec<String> = std::iter::once("devbox")
            .chain(argv.iter().copied())
            .map(str::to_string)
            .collect();
        let cli = Cli::try_parse_from(&full).unwrap_or_else(|e| panic!("{full:?}: {e}"));
        match cli.command.expect("a subcommand") {
            Command::Layer(args) => args.action,
            other => panic!("{other:?} is not a layer command"),
        }
    }

    /// The box is a positional everywhere else in v5, and these four are no
    /// exception.
    #[test]
    fn the_box_is_a_positional_with_the_legacy_flag_still_working() {
        for argv in [
            vec!["layer", "checkpoint", "devtest"],
            vec!["layer", "checkpoints", "devtest"],
            vec!["layer", "diff", "devtest", "--from", "01kfx9"],
            vec!["layer", "restore", "01kfx9", "devtest"],
            vec!["layer", "checkpoint-rm", "01kfx9", "devtest"],
        ] {
            assert_eq!(
                layer(&argv).boxarg().name(),
                Some("devtest"),
                "{argv:?} did not read the positional box name"
            );
        }

        for argv in [
            vec!["layer", "checkpoint", "--name", "devtest"],
            vec!["layer", "checkpoints", "--name", "devtest"],
            vec!["layer", "restore", "01kfx9", "--name", "devtest"],
            vec!["layer", "checkpoint-rm", "01kfx9", "--name", "devtest"],
        ] {
            assert_eq!(
                layer(&argv).boxarg().name(),
                Some("devtest"),
                "{argv:?} lost the --name alias"
            );
        }
    }

    #[test]
    fn omitting_the_box_leaves_it_for_the_current_directory() {
        assert_eq!(layer(&["layer", "checkpoint"]).boxarg().name(), None);
        assert_eq!(layer(&["layer", "checkpoints"]).boxarg().name(), None);
        assert_eq!(
            layer(&["layer", "restore", "01kfx9"]).boxarg().name(),
            None,
            "the lone positional is the checkpoint, not the box"
        );
        assert_eq!(
            layer(&["layer", "checkpoint-rm", "01kfx9"]).boxarg().name(),
            None,
            "the lone positional is the checkpoint, not the box"
        );
    }

    /// `--force` is the whole difference between "delete this" and "delete
    /// this even though a run report cites it", so it defaults to off.
    #[test]
    fn checkpoint_rm_needs_force_spelled_out() {
        let LayerAction::CheckpointRm { id, force, boxarg } =
            layer(&["layer", "checkpoint-rm", "01kfx9m2", "devtest"])
        else {
            panic!("not a checkpoint-rm");
        };
        assert_eq!(id, "01kfx9m2");
        assert_eq!(boxarg.name(), Some("devtest"));
        assert!(!force);

        let LayerAction::CheckpointRm { force, .. } =
            layer(&["layer", "checkpoint-rm", "01kfx9m2", "devtest", "--force"])
        else {
            panic!("not a checkpoint-rm");
        };
        assert!(force);
    }

    #[test]
    fn checkpoint_takes_an_optional_label() {
        let LayerAction::Checkpoint { label, .. } = layer(&["layer", "checkpoint"]) else {
            panic!("not a checkpoint");
        };
        assert_eq!(label, None);

        let LayerAction::Checkpoint { label, boxarg } =
            layer(&["layer", "checkpoint", "devtest", "--label", "base"])
        else {
            panic!("not a checkpoint");
        };
        assert_eq!(label.as_deref(), Some("base"));
        assert_eq!(boxarg.name(), Some("devtest"));
    }

    /// `layer diff` keeps its old meaning when no checkpoint is named, so the
    /// checkpoint diff is the same command rather than a second one.
    #[test]
    fn diff_without_from_is_still_the_live_diff() {
        let LayerAction::Diff { from, to, .. } = layer(&["layer", "diff", "devtest"]) else {
            panic!("not a diff");
        };
        assert_eq!(from, None);
        assert_eq!(to, None);
    }

    #[test]
    fn diff_reads_from_and_to() {
        let LayerAction::Diff { from, to, boxarg } = layer(&[
            "layer", "diff", "devtest", "--from", "01kfx9m2", "--to", "01kfx9m3",
        ]) else {
            panic!("not a diff");
        };
        assert_eq!(from.as_deref(), Some("01kfx9m2"));
        assert_eq!(to.as_deref(), Some("01kfx9m3"));
        assert_eq!(boxarg.name(), Some("devtest"));
    }

    /// `--to` alone has no meaning: there would be nothing to compare against.
    #[test]
    fn to_without_from_is_rejected() {
        let argv = ["devbox", "layer", "diff", "--to", "01kfx9m3"];
        let error = Cli::try_parse_from(argv).expect_err("--to alone should not parse");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn restore_requires_a_checkpoint() {
        let argv = ["devbox", "layer", "restore"];
        let error = Cli::try_parse_from(argv).expect_err("restore needs an id");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn naming_the_box_twice_is_an_error() {
        for argv in [
            vec![
                "devbox",
                "layer",
                "checkpoint",
                "devtest",
                "--name",
                "other",
            ],
            vec![
                "devbox",
                "layer",
                "checkpoints",
                "devtest",
                "--name",
                "other",
            ],
            vec![
                "devbox", "layer", "restore", "01kfx9", "devtest", "--name", "other",
            ],
        ] {
            let error = Cli::try_parse_from(&argv).expect_err(&format!("{argv:?} took two boxes"));
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{argv:?} was rejected for the wrong reason: {error}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Restoring under a live run
// ---------------------------------------------------------------------------

#[test]
fn a_restore_is_refused_while_a_run_is_still_going() {
    // A restore replaces /workspace wholesale. Under a live run that destroys
    // the thing the run's report is about — its end checkpoint would describe
    // a tree the command never produced — and nothing afterwards can tell.
    assert!(refuse_while_running("devtest", &[]).is_ok());

    let one = refuse_while_running("devtest", &[live_run("01K4SZ0000000000000000ABCD")])
        .unwrap_err()
        .to_string();
    assert!(
        one.contains("01K4SZ0000000000000000ABCD"),
        "the refusal has to name the run, or the reader has nothing to go on: {one}"
    );
    assert!(one.contains("devbox runs devtest"), "{one}");
    assert!(
        !one.contains("  "),
        "a line-continued literal leaked its indentation into the message: {one}"
    );

    let two = refuse_while_running(
        "devtest",
        &[
            live_run("01K4SZ0000000000000000ABCD"),
            live_run("01K4SZ0000000000000000EFGH"),
        ],
    )
    .unwrap_err()
    .to_string();
    assert!(two.contains("2 run(s)"), "{two}");
    assert!(two.contains("01K4SZ0000000000000000EFGH"), "{two}");
}

#[test]
fn a_runs_checkpoints_are_never_pruned() {
    // The report links to them. A report whose evidence has been
    // garbage-collected is worse than a slightly larger directory.
    let checkpoints = vec![
        manifest("01k4sz000000000000", None),
        manifest("01k4sz000000000001", Some("01K4SZ0000000000000000ABCD")),
        manifest("01k4sz000000000002", None),
        manifest("01k4sz000000000003", Some("01K4SZ0000000000000000ABCD")),
        manifest("01k4sz000000000004", None),
    ];
    let doomed: Vec<String> = prune_plan(&checkpoints, 1)
        .iter()
        .map(|id| id.as_str().to_string())
        .collect();
    // Two of the three unpinned ones go; neither pinned one is touched, and
    // the pinned pair does not consume the keep budget either.
    assert_eq!(doomed, vec!["01k4sz000000000000", "01k4sz000000000002"]);
}

// ---------------------------------------------------------------------------
// Letting a finished run's checkpoints go
// ---------------------------------------------------------------------------

fn pin(run: &str, ended: Option<&str>, reported: bool) -> RunPin {
    RunPin {
        run_id: run.to_string(),
        ended_at: ended.map(str::to_string),
        start: Some(CheckpointId::at(1, 1)),
        end: Some(CheckpointId::at(2, 2)),
        reported,
    }
}

const CUTOFF: &str = "2026-09-01T00:00:00.000Z";

#[test]
fn a_run_that_ended_long_ago_with_a_report_may_let_its_checkpoints_go() {
    let old = pin("r1", Some("2026-08-01T00:00:00.000Z"), true);
    assert_eq!(
        expired_run_pins(std::slice::from_ref(&old), CUTOFF),
        vec![old]
    );
}

#[test]
fn a_recent_run_keeps_them() {
    let recent = pin("r1", Some("2026-09-05T00:00:00.000Z"), true);
    assert!(expired_run_pins(&[recent], CUTOFF).is_empty());
}

/// The condition that makes this safe at all. Until the report is on disk, the
/// checkpoints are the only thing it could still be built from — so a run that
/// never rendered one keeps them however old it is.
#[test]
fn a_run_whose_report_was_never_written_keeps_them_however_old_it_is() {
    let ancient = pin("r1", Some("2020-01-01T00:00:00.000Z"), false);
    assert!(expired_run_pins(&[ancient], CUTOFF).is_empty());
}

#[test]
fn a_run_that_has_not_ended_keeps_them() {
    assert!(expired_run_pins(&[pin("r1", None, true)], CUTOFF).is_empty());
    assert!(expired_run_pins(&[pin("r1", Some(""), true)], CUTOFF).is_empty());
}

#[test]
fn a_run_holding_no_checkpoints_is_not_reported_as_freeable() {
    let mut empty = pin("r1", Some("2020-01-01T00:00:00.000Z"), true);
    empty.start = None;
    empty.end = None;
    assert!(expired_run_pins(&[empty], CUTOFF).is_empty());
}

/// An aborted run has a start and no end. The one it does hold is still
/// collected.
#[test]
fn a_run_with_only_a_start_still_releases_it() {
    let mut half = pin("r1", Some("2020-01-01T00:00:00.000Z"), true);
    half.end = None;
    let freed = expired_run_pins(&[half], CUTOFF);
    assert_eq!(freed.len(), 1);
    assert!(freed[0].start.is_some() && freed[0].end.is_none());
}

#[test]
fn ages_parse_in_the_spellings_people_type() {
    assert_eq!(parse_age("7d").unwrap(), chrono::Duration::days(7));
    assert_eq!(parse_age("24h").unwrap(), chrono::Duration::hours(24));
    assert_eq!(parse_age("90m").unwrap(), chrono::Duration::minutes(90));
    assert_eq!(
        parse_age(" 3600s ").unwrap(),
        chrono::Duration::seconds(3600)
    );
    for bad in ["", "7", "d", "7w", "-1d", "seven days", "1.5h"] {
        assert!(parse_age(bad).is_err(), "{bad:?} was accepted");
    }
}

/// `--runs-older-than` is a separate, narrower decision. It must not become a
/// way for the count-based pass to start evicting a report's evidence.
#[test]
fn the_count_based_pass_still_never_touches_a_runs_checkpoints() {
    let claimed = Checkpoint {
        id: CheckpointId::at(1, 1),
        label: None,
        created_at: "2020-01-01T00:00:00Z".into(),
        run_id: Some("r1".into()),
        files: 0,
        bytes: 0,
    };
    assert!(prune_plan(&[claimed], 0).is_empty());
}
