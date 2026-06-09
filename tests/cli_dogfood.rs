#![cfg(windows)]

mod common;

use common::*;

#[test]
fn cli_parent_dogfood_flow_reviews_promotes_resolves_and_discards_worker_lanes() {
    let repo = TempRepo::new();
    repo.write(
        "src/app.ts",
        b"export const title = 'Base';\nexport const mode = 'stable';\n",
    );
    repo.write("README.md", b"# Lane\n\nBase docs.\n");

    let docs_clean = repo.run_json([
        "exec",
        "docs-clean",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('README.md', \"# Lane`n`nDogfood docs.`n\"); [IO.File]::WriteAllText('src/analytics.ts', \"export const analytics = true;`n\")",
    ]);
    assert_eq!(docs_clean["exit_code"], 0);
    assert_eq!(docs_clean["worker_error"], Value::Null);

    let title_loud = repo.run_json([
        "exec",
        "title-loud",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/app.ts', \"export const title = 'Loud';`nexport const mode = 'stable';`n\"); [IO.File]::WriteAllText('src/banner.ts', \"export const banner = 'selected';`n\")",
    ]);
    assert_eq!(title_loud["exit_code"], 0);
    assert_eq!(title_loud["worker_error"], Value::Null);

    let title_grid = repo.run_json([
        "exec",
        "title-grid",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/app.ts', \"export const title = 'Grid';`nexport const mode = 'stable';`n\")",
    ]);
    assert_eq!(title_grid["exit_code"], 0);
    assert_eq!(title_grid["worker_error"], Value::Null);

    let scratch_build = repo.run_json([
        "exec",
        "scratch-build",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; New-Item -ItemType Directory -Path '.cache' -Force | Out-Null; [IO.File]::WriteAllText('.cache/agent-report.txt', \"noise`n\"); [IO.File]::WriteAllText('src/prototype.ts', \"export const prototype = true;`n\")",
    ]);
    assert_eq!(scratch_build["exit_code"], 0);
    assert_eq!(scratch_build["worker_error"], Value::Null);

    let failed_output = run_lane_exec(
        repo.path(),
        "failed-worker",
        "$ErrorActionPreference = \"Continue\"; New-Item -ItemType Directory -Path '.cache' -Force | Out-Null; [IO.File]::WriteAllText('.cache/failed.log', \"failed`n\"); [IO.File]::WriteAllText('src/partial.ts', \"export const partial = true;`n\"); Write-Error \"simulated dogfood failure\"; exit 9",
    );
    assert!(!failed_output.status.success());
    let failed_worker = output_json(&failed_output);
    assert_eq!(failed_worker["exit_code"], 9);
    assert_eq!(failed_worker["worker_error"], Value::Null);
    assert_eq!(
        string_array(&failed_worker["changed_paths"]),
        vec![".cache", ".cache/failed.log", "src/partial.ts"]
    );

    assert_eq!(
        fs::read(repo.path().join("src/app.ts")).unwrap(),
        b"export const title = 'Base';\nexport const mode = 'stable';\n"
    );
    assert!(!repo.path().join("src/banner.ts").exists());
    assert!(!repo.path().join("src/prototype.ts").exists());
    assert!(!repo.path().join("src/partial.ts").exists());

    let review = repo.run_json(["review"]);
    assert_eq!(review["lane"], Value::Null);
    assert_eq!(review["summary"]["lanes"], 5);
    assert_eq!(review["summary"]["changed_paths"], 8);
    assert_eq!(review["summary"]["clean_ops"], 7);
    assert_eq!(review["summary"]["conflicted_ops"], 2);
    assert_eq!(review["summary"]["conflict_groups"], 1);
    assert_eq!(review["lanes"].as_array().unwrap().len(), 5);
    assert_eq!(
        review_paths(&review),
        vec![
            ".cache/agent-report.txt",
            ".cache/failed.log",
            "README.md",
            "src/analytics.ts",
            "src/app.ts",
            "src/banner.ts",
            "src/partial.ts",
            "src/prototype.ts",
        ]
    );

    let failed_lane = review_lane(&review, "failed-worker");
    assert_eq!(failed_lane["changed_paths"], 2);
    assert_eq!(failed_lane["clean_ops"], 2);
    assert_eq!(failed_lane["conflicted_ops"], 0);
    assert_eq!(failed_lane["last_exec"]["exit_code"], 9);
    assert_eq!(failed_lane["last_exec"]["worker_error"], Value::Null);
    assert_eq!(
        string_array(&failed_lane["last_exec"]["changed_paths"]),
        vec![".cache", ".cache/failed.log", "src/partial.ts"]
    );
    assert!(
        failed_lane["last_exec"]["stderr"]["text"]
            .as_str()
            .unwrap()
            .contains("simulated dogfood failure")
    );
    assert_eq!(failed_lane["last_exec"]["stderr"]["truncated"], false);
    assert_eq!(
        review_action_kinds(&failed_lane["actions"]),
        vec!["promote_clean", "discard"]
    );
    assert_eq!(
        review_action_commands(&failed_lane["actions"]),
        vec![
            vec!["promote-clean", "failed-worker"],
            vec!["discard", "failed-worker"]
        ]
    );

    let selected_lane = review_lane(&review, "title-loud");
    assert_eq!(selected_lane["changed_paths"], 2);
    assert_eq!(selected_lane["clean_ops"], 1);
    assert_eq!(selected_lane["conflicted_ops"], 1);
    assert_eq!(selected_lane["last_exec"]["exit_code"], 0);
    assert_eq!(
        review_action_commands(&selected_lane["actions"]),
        vec![
            vec!["promote-clean", "title-loud"],
            vec!["discard", "title-loud"]
        ]
    );

    let app_review = review_path(&review, "src/app.ts");
    assert!(app_review["clean_ops"].as_array().unwrap().is_empty());
    let app_conflict = &app_review["conflicts"][0];
    assert_eq!(
        string_array(&app_conflict["lanes"]),
        vec!["title-grid", "title-loud"]
    );
    assert_eq!(
        review_op_ids(&app_conflict["ops"]),
        vec!["title-grid:1", "title-loud:1"]
    );
    assert_eq!(
        review_action_kinds(&app_conflict["actions"]),
        vec!["show_op", "resolve_op", "show_op", "resolve_op"]
    );
    assert_eq!(
        review_action_commands(&app_conflict["actions"]),
        vec![
            vec!["show-op", "title-grid", "src/app.ts", "title-grid:1"],
            vec![
                "resolve-op",
                "title-grid",
                "src/app.ts",
                "title-grid:1",
                "--with-file",
                "<replacement-file>"
            ],
            vec!["show-op", "title-loud", "src/app.ts", "title-loud:1"],
            vec![
                "resolve-op",
                "title-loud",
                "src/app.ts",
                "title-loud:1",
                "--with-file",
                "<replacement-file>"
            ],
        ]
    );
    assert_eq!(
        app_conflict["actions"][1]["required_inputs"][0]["name"],
        "with_file"
    );
    assert_eq!(
        app_conflict["actions"][1]["required_inputs"][0]["placeholder"],
        "<replacement-file>"
    );

    let failed_review = review_path(&review, "src/partial.ts");
    assert_eq!(failed_review["lanes"][0]["lane"], "failed-worker");
    assert_eq!(failed_review["lanes"][0]["status"], "created");
    assert_eq!(failed_review["lanes"][0]["clean_ops"], 1);

    let docs_promoted = run_review_action_json(
        &repo,
        review_action(
            &review_lane(&review, "docs-clean")["actions"],
            "promote_clean",
            "docs-clean",
        ),
    );
    assert_eq!(change_statuses_from_key(&docs_promoted, "promoted"), {
        let mut expected = BTreeMap::new();
        expected.insert("README.md".to_owned(), "modified".to_owned());
        expected.insert("src/analytics.ts".to_owned(), "created".to_owned());
        expected
    });
    assert!(docs_promoted["conflicts"].as_array().unwrap().is_empty());

    let selected_clean = run_review_action_json(
        &repo,
        review_action(
            &review_lane(&review, "title-loud")["actions"],
            "promote_clean",
            "title-loud",
        ),
    );
    assert_eq!(change_statuses_from_key(&selected_clean, "promoted"), {
        let mut expected = BTreeMap::new();
        expected.insert("src/banner.ts".to_owned(), "created".to_owned());
        expected
    });
    assert_eq!(change_statuses_from_key(&selected_clean, "conflicts"), {
        let mut expected = BTreeMap::new();
        expected.insert("src/app.ts".to_owned(), "modified".to_owned());
        expected
    });

    let after_clean = repo.run_json(["review", "title-loud"]);
    assert_eq!(after_clean["summary"]["clean_ops"], 0);
    assert_eq!(after_clean["summary"]["conflicted_ops"], 1);
    assert_eq!(
        review_action_commands(&review_lane(&after_clean, "title-loud")["actions"]),
        vec![vec!["discard", "title-loud"]]
    );
    let after_clean_conflict = &review_path(&after_clean, "src/app.ts")["conflicts"][0];

    let shown = run_review_action_json(
        &repo,
        review_action(&after_clean_conflict["actions"], "show_op", "title-loud"),
    );
    let selected_op_id = shown["op"]["op_id"].as_str().unwrap();
    assert_eq!(shown["op"]["op_id"], selected_op_id);
    assert_eq!(shown["base"]["utf8"], "Base");
    assert_eq!(shown["inserted"]["utf8"], "Loud");
    assert_eq!(
        string_array(&shown["op"]["conflicts_with"]),
        vec!["title-grid"]
    );

    let resolution = repo.path().join("title-resolution.txt");
    fs::write(&resolution, b"Launch").unwrap();
    let resolved = run_review_action_with_replacement_json(
        &repo,
        review_action(&after_clean_conflict["actions"], "resolve_op", "title-loud"),
        &resolution,
    );
    assert_eq!(resolved["replacement"]["utf8"], "Launch");
    assert!(resolved["remaining"].as_array().unwrap().is_empty());

    assert_eq!(
        fs::read(repo.path().join("src/app.ts")).unwrap(),
        b"export const title = 'Launch';\nexport const mode = 'stable';\n"
    );
    assert_eq!(
        fs::read(repo.path().join("src/banner.ts")).unwrap(),
        b"export const banner = 'selected';\n"
    );
    assert_eq!(
        fs::read(repo.path().join("src/analytics.ts")).unwrap(),
        b"export const analytics = true;\n"
    );
    assert!(!repo.path().join(".cache/agent-report.txt").exists());
    assert!(!repo.path().join(".cache/failed.log").exists());
    assert!(!repo.path().join("src/prototype.ts").exists());
    assert!(!repo.path().join("src/partial.ts").exists());

    for (lane, discarded_changes) in [
        ("docs-clean", 0),
        ("title-loud", 0),
        ("title-grid", 1),
        ("scratch-build", 2),
        ("failed-worker", 2),
    ] {
        let latest_review = repo.run_json(["review", lane]);
        let discarded = run_review_action_json(
            &repo,
            review_action(
                &review_lane(&latest_review, lane)["actions"],
                "discard",
                lane,
            ),
        );
        assert_eq!(discarded["removed"], true);
        assert_eq!(discarded["discarded_changes"], discarded_changes);
    }

    let final_review = repo.run_json(["review"]);
    assert_eq!(final_review["summary"]["lanes"], 0);
    assert_eq!(final_review["summary"]["changed_paths"], 0);
    assert!(final_review["lanes"].as_array().unwrap().is_empty());
    assert!(final_review["paths"].as_array().unwrap().is_empty());
}
