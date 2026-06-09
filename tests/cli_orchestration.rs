#![cfg(windows)]

mod common;

use common::*;

#[test]
fn cli_exec_preserves_parallel_lane_outputs() {
    let repo = TempRepo::new();
    repo.write("src/feature.ts", b"export const approach = 'base';");

    let root_a = repo.path().to_path_buf();
    let root_b = repo.path().to_path_buf();
    let job_a = thread::spawn(move || {
        run_lane_exec(
            &root_a,
            "approach-a",
            "$ErrorActionPreference = \"Stop\"; Start-Sleep -Milliseconds 400; Set-Content -Path src/feature.ts -Value \"export const approach = 'a';\" -NoNewline; Set-Content -Path src/a.ts -Value \"export const a = true;\" -NoNewline",
        )
    });
    let job_b = thread::spawn(move || {
        run_lane_exec(
            &root_b,
            "approach-b",
            "$ErrorActionPreference = \"Stop\"; Start-Sleep -Milliseconds 400; Set-Content -Path src/feature.ts -Value \"export const approach = 'b';\" -NoNewline; Set-Content -Path src/b.ts -Value \"export const b = true;\" -NoNewline",
        )
    });

    let output_a = assert_success(job_a.join().unwrap());
    let output_b = assert_success(job_b.join().unwrap());
    let exec_a = output_json(&output_a);
    let exec_b = output_json(&output_b);
    assert_eq!(exec_a["lane"], "approach-a");
    assert_eq!(exec_b["lane"], "approach-b");
    assert_eq!(exec_a["exit_code"], 0);
    assert_eq!(exec_b["exit_code"], 0);
    assert_eq!(exec_a["worker_error"], Value::Null);
    assert_eq!(exec_b["worker_error"], Value::Null);

    assert_eq!(
        fs::read(repo.path().join("src/feature.ts")).unwrap(),
        b"export const approach = 'base';"
    );
    assert_eq!(
        review_change_statuses(&repo.run_json(["review", "approach-a"]), "approach-a"),
        {
            let mut expected = BTreeMap::new();
            expected.insert("src/a.ts".to_owned(), "created".to_owned());
            expected.insert("src/feature.ts".to_owned(), "modified".to_owned());
            expected
        }
    );
    assert_eq!(
        review_change_statuses(&repo.run_json(["review", "approach-b"]), "approach-b"),
        {
            let mut expected = BTreeMap::new();
            expected.insert("src/b.ts".to_owned(), "created".to_owned());
            expected.insert("src/feature.ts".to_owned(), "modified".to_owned());
            expected
        }
    );

    let approach_b = repo.run_json(["review", "approach-b"]);
    let promoted_clean = run_review_action_json(
        &repo,
        review_action(
            &review_lane(&approach_b, "approach-b")["actions"],
            "promote_clean",
            "approach-b",
        ),
    );
    assert_eq!(change_statuses_from_key(&promoted_clean, "promoted"), {
        let mut expected = BTreeMap::new();
        expected.insert("src/b.ts".to_owned(), "created".to_owned());
        expected
    });

    let conflict_review = repo.run_json(["review"]);
    let conflict_actions =
        &review_path(&conflict_review, "src/feature.ts")["conflicts"][0]["actions"];
    let shown = run_review_action_json(
        &repo,
        review_action(conflict_actions, "show_op", "approach-b"),
    );
    let resolution = repo.path().join("approach-b-resolution.txt");
    fs::write(
        &resolution,
        shown["inserted"]["utf8"].as_str().unwrap().as_bytes(),
    )
    .unwrap();
    let resolved = run_review_action_with_replacement_json(
        &repo,
        review_action(conflict_actions, "resolve_op", "approach-b"),
        &resolution,
    );
    assert!(resolved["remaining"].as_array().unwrap().is_empty());

    assert_eq!(
        fs::read(repo.path().join("src/feature.ts")).unwrap(),
        b"export const approach = 'b';"
    );
    assert!(!repo.path().join("src/a.ts").exists());
    assert_eq!(
        fs::read(repo.path().join("src/b.ts")).unwrap(),
        b"export const b = true;"
    );
}

#[test]
fn cli_promote_ops_promotes_selected_same_file_op_and_preserves_other_lane_ops() {
    let repo = TempRepo::new();
    repo.write("src/math.txt", b"alpha=1\nbeta=2\ngamma=3\n");

    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/math.txt', \"alpha=10`nbeta=2`ngamma=30`n\")",
    ]);
    repo.run_json([
        "exec",
        "agent-b",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/math.txt', \"alpha=1`nbeta=20`ngamma=3`n\")",
    ]);

    let review = repo.run_json(["review", "agent-a"]);
    let ops = review_path(&review, "src/math.txt")["clean_ops"]
        .as_array()
        .unwrap();
    assert_eq!(ops.len(), 2);
    let op_id = ops[0]["op"]["op_id"].as_str().unwrap().to_owned();

    let promoted = repo.run_json(["promote-ops", "agent-a", "src/math.txt", op_id.as_str()]);
    assert_eq!(string_array(&promoted["promoted_ops"]), vec![op_id.clone()]);
    assert_eq!(promoted["promoted"][0]["ops"].as_array().unwrap().len(), 1);
    assert_eq!(promoted["promoted"][0]["ops"][0]["op_id"], op_id);
    assert_eq!(
        fs::read(repo.path().join("src/math.txt")).unwrap(),
        b"alpha=10\nbeta=2\ngamma=3\n"
    );

    let remaining_a = repo.run_json(["review", "agent-a"]);
    let remaining_a_ops = review_path(&remaining_a, "src/math.txt")["clean_ops"]
        .as_array()
        .unwrap();
    assert_eq!(remaining_a_ops.len(), 1);
    assert_eq!(remaining_a_ops[0]["op"]["op_id"], "agent-a:2");
    let remaining_b = repo.run_json(["review", "agent-b"]);
    assert_eq!(review_change_statuses(&remaining_b, "agent-b"), {
        let mut expected = BTreeMap::new();
        expected.insert("src/math.txt".to_owned(), "modified".to_owned());
        expected
    });

    repo.run_json(["promote-clean", "agent-b"]);
    assert_eq!(
        fs::read(repo.path().join("src/math.txt")).unwrap(),
        b"alpha=10\nbeta=20\ngamma=3\n"
    );
}

#[test]
fn cli_conflicts_and_promote_clean_drive_op_level_orchestration() {
    let repo = TempRepo::new();
    repo.write("src/vars.txt", b"a=1\nb=2\nc=3\n");

    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/vars.txt', \"a=A`nb=B`nc=C`n\")",
    ]);
    repo.run_json([
        "exec",
        "agent-b",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/vars.txt', \"a=1`nb=X`nc=3`n\")",
    ]);

    let conflicts = repo.run_json(["review", "agent-a"]);
    let conflict_groups = review_path(&conflicts, "src/vars.txt")["conflicts"]
        .as_array()
        .unwrap();
    assert_eq!(conflict_groups.len(), 1);
    let conflict_ops = conflict_groups[0]["ops"].as_array().unwrap();
    assert_eq!(conflict_ops.len(), 1);
    assert_eq!(conflict_ops[0]["op"]["op_id"], "agent-a:2");
    assert_eq!(
        string_array(&conflict_ops[0]["op"]["conflicts_with"]),
        vec!["agent-b"]
    );

    let promoted = repo.run_json(["promote-clean", "agent-a"]);
    assert_eq!(promoted["promoted_ops"].as_array().unwrap().len(), 1);
    assert_eq!(promoted["promoted_ops"][0]["path"], "src/vars.txt");
    assert_eq!(
        string_array(&promoted["promoted_ops"][0]["ops"]),
        vec!["agent-a:1", "agent-a:3"]
    );
    assert_eq!(promoted["conflicts"][0]["ops"][0]["op_id"], "agent-a:2");
    assert_eq!(
        fs::read(repo.path().join("src/vars.txt")).unwrap(),
        b"a=A\nb=2\nc=C\n"
    );

    let remaining_a = repo.run_json(["review", "agent-a"]);
    let remaining_a_ops = review_path(&remaining_a, "src/vars.txt")["conflicts"][0]["ops"]
        .as_array()
        .unwrap();
    assert_eq!(remaining_a_ops.len(), 1);
    assert_eq!(remaining_a_ops[0]["op"]["op_id"], "agent-a:2");
    assert_eq!(
        string_array(&remaining_a_ops[0]["op"]["conflicts_with"]),
        vec!["agent-b"]
    );

    let conflict_review = repo.run_json(["review"]);
    let conflict_actions =
        &review_path(&conflict_review, "src/vars.txt")["conflicts"][0]["actions"];
    let shown =
        run_review_action_json(&repo, review_action(conflict_actions, "show_op", "agent-b"));
    let resolution = repo.path().join("agent-b-resolution.txt");
    fs::write(
        &resolution,
        shown["inserted"]["utf8"].as_str().unwrap().as_bytes(),
    )
    .unwrap();
    let resolved = run_review_action_with_replacement_json(
        &repo,
        review_action(conflict_actions, "resolve_op", "agent-b"),
        &resolution,
    );
    assert!(resolved["remaining"].as_array().unwrap().is_empty());
    assert_eq!(
        fs::read(repo.path().join("src/vars.txt")).unwrap(),
        b"a=A\nb=X\nc=C\n"
    );
}

#[test]
fn cli_review_groups_clean_ops_and_conflict_decisions_json_first() {
    let repo = TempRepo::new();
    repo.write("src/vars.txt", b"a=1\nb=2\nc=3\nd=4\n");

    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/vars.txt', \"a=A`nb=B`nc=3`nd=4`n\")",
    ]);
    repo.run_json([
        "exec",
        "agent-b",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/vars.txt', \"a=1`nb=X`nc=C`nd=4`n\")",
    ]);
    repo.run_json([
        "exec",
        "agent-c",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/vars.txt', \"a=1`nb=2`nc=3`nd=D`n\")",
    ]);

    let review = repo.run_json(["review"]);
    assert_eq!(review["lane"], Value::Null);
    assert_eq!(review["summary"]["lanes"], 3);
    assert_eq!(review["summary"]["changed_paths"], 1);
    assert_eq!(review["summary"]["clean_ops"], 3);
    assert_eq!(review["summary"]["conflicted_ops"], 2);
    assert_eq!(review["summary"]["conflict_groups"], 1);

    let path = &review["paths"][0];
    assert_eq!(path["path"], "src/vars.txt");
    assert_eq!(path["lanes"].as_array().unwrap().len(), 3);
    assert_eq!(
        review_op_ids(&path["clean_ops"]),
        vec!["agent-a:1", "agent-b:2", "agent-c:1"]
    );

    let conflict = &path["conflicts"][0];
    assert_eq!(conflict["range_start"], 6);
    assert_eq!(conflict["range_end"], 7);
    assert_eq!(string_array(&conflict["lanes"]), vec!["agent-a", "agent-b"]);
    assert_eq!(
        review_op_ids(&conflict["ops"]),
        vec!["agent-a:2", "agent-b:1"]
    );
    assert_eq!(conflict["ops"][0]["base"]["utf8"], "2");
    assert_eq!(conflict["ops"][0]["inserted"]["utf8"], "B");
    assert_eq!(conflict["ops"][1]["inserted"]["utf8"], "X");

    let agent_a_review = repo.run_json(["review", "agent-a"]);
    assert_eq!(agent_a_review["lane"], "agent-a");
    assert_eq!(agent_a_review["summary"]["lanes"], 1);
    assert_eq!(agent_a_review["summary"]["clean_ops"], 1);
    assert_eq!(agent_a_review["summary"]["conflicted_ops"], 1);
    assert_eq!(
        review_op_ids(&agent_a_review["paths"][0]["conflicts"][0]["ops"]),
        vec!["agent-a:2"]
    );

    repo.run_json(["promote-clean", "agent-a"]);
    let after_clean = repo.run_json(["review", "agent-a"]);
    assert_eq!(after_clean["summary"]["clean_ops"], 0);
    assert_eq!(after_clean["summary"]["conflicted_ops"], 1);
    assert_eq!(
        review_op_ids(&after_clean["paths"][0]["conflicts"][0]["ops"]),
        vec!["agent-a:2"]
    );
}

#[test]
fn cli_show_op_and_resolve_op_complete_conflicted_operation_flow() {
    let repo = TempRepo::new();
    repo.write("src/vars.txt", b"a=1\nb=2\nc=3\n");

    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/vars.txt', \"a=A`nb=B`nc=C`n\")",
    ]);
    repo.run_json([
        "exec",
        "agent-b",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/vars.txt', \"a=1`nb=X`nc=3`n\")",
    ]);

    repo.run_json(["promote-clean", "agent-a"]);

    let shown = repo.run_json(["show-op", "agent-a", "src/vars.txt", "agent-a:2"]);
    assert_eq!(shown["op"]["op_id"], "agent-a:2");
    assert_eq!(shown["base"]["utf8"], "2");
    assert_eq!(shown["inserted"]["utf8"], "B");
    assert_eq!(
        string_array(&shown["op"]["conflicts_with"]),
        vec!["agent-b"]
    );

    let replacement = repo.path().join("resolution.txt");
    fs::write(&replacement, b"Y").unwrap();
    let resolved = output_json(&repo.run_vec(vec![
        "resolve-op".to_owned(),
        "agent-a".to_owned(),
        "src/vars.txt".to_owned(),
        "agent-a:2".to_owned(),
        "--with-file".to_owned(),
        replacement.display().to_string().to_owned(),
    ]));

    assert_eq!(resolved["resolved_op"]["op_id"], "agent-a:2");
    assert_eq!(resolved["replacement"]["utf8"], "Y");
    assert!(resolved["remaining"].as_array().unwrap().is_empty());
    assert_eq!(
        fs::read(repo.path().join("src/vars.txt")).unwrap(),
        b"a=A\nb=Y\nc=C\n"
    );

    let agent_b = repo.run_json(["review", "agent-b"]);
    let agent_b_path = review_path(&agent_b, "src/vars.txt");
    let agent_b_ops = agent_b_path["clean_ops"].as_array().unwrap();
    assert_eq!(agent_b_ops.len(), 1);
    assert_eq!(agent_b_ops[0]["op"]["inserted_len"], 1);
    assert!(agent_b_path["conflicts"].as_array().unwrap().is_empty());
}

#[test]
fn cli_parent_can_orchestrate_five_parallel_lane_execs_directly() {
    let repo = TempRepo::new();
    repo.write("src/login.tsx", b"export const design = 'base';");

    let variants = [
        ("login-minimal", "minimal"),
        ("login-enterprise", "enterprise"),
        ("login-playful", "playful"),
        ("login-split", "split"),
        ("login-focused", "focused"),
    ];
    let jobs = variants
        .iter()
        .map(|(lane, design)| {
            let root = repo.path().to_path_buf();
            let lane = (*lane).to_owned();
            let design = (*design).to_owned();
            thread::spawn(move || {
                let script = format!(
                    "$ErrorActionPreference = \"Stop\"; Start-Sleep -Milliseconds 250; Set-Content -Path src/login.tsx -Value \"export const design = '{}';\" -NoNewline; Set-Content -Path src/{}.tsx -Value \"export const marker = '{}';\" -NoNewline",
                    design, design, design
                );
                run_lane_exec(&root, &lane, &script)
            })
        })
        .collect::<Vec<_>>();

    let exec_outputs = jobs
        .into_iter()
        .map(|job| output_json(&assert_success(job.join().unwrap())))
        .collect::<Vec<_>>();
    assert_eq!(exec_outputs.len(), 5);
    let mount_roots = exec_outputs
        .iter()
        .map(|output| output["workspace_root"].as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        mount_roots.len(),
        exec_outputs.len(),
        "parallel lane execs must use distinct virtual mount roots"
    );
    for output in &exec_outputs {
        assert_exec_contract(output);
        assert_eq!(output["exit_code"], 0);
        assert_eq!(output["worker_error"], Value::Null);
        assert_eq!(output["changes"].as_array().unwrap().len(), 2);
    }

    assert_eq!(
        fs::read(repo.path().join("src/login.tsx")).unwrap(),
        b"export const design = 'base';"
    );

    for (lane, design) in variants {
        assert_eq!(
            review_change_statuses(&repo.run_json(["review", lane]), lane),
            {
                let mut expected = BTreeMap::new();
                expected.insert(format!("src/{design}.tsx"), "created".to_owned());
                expected.insert("src/login.tsx".to_owned(), "modified".to_owned());
                expected
            }
        );
    }

    let enterprise_review = repo.run_json(["review", "login-enterprise"]);
    let promoted_clean = run_review_action_json(
        &repo,
        review_action(
            &review_lane(&enterprise_review, "login-enterprise")["actions"],
            "promote_clean",
            "login-enterprise",
        ),
    );
    assert_eq!(change_statuses_from_key(&promoted_clean, "promoted"), {
        let mut expected = BTreeMap::new();
        expected.insert("src/enterprise.tsx".to_owned(), "created".to_owned());
        expected
    });

    let conflict_review = repo.run_json(["review"]);
    let conflict_actions =
        &review_path(&conflict_review, "src/login.tsx")["conflicts"][0]["actions"];
    let shown = run_review_action_json(
        &repo,
        review_action(conflict_actions, "show_op", "login-enterprise"),
    );
    let resolution = repo.path().join("login-enterprise-resolution.txt");
    fs::write(
        &resolution,
        shown["inserted"]["utf8"].as_str().unwrap().as_bytes(),
    )
    .unwrap();
    let resolved = run_review_action_with_replacement_json(
        &repo,
        review_action(conflict_actions, "resolve_op", "login-enterprise"),
        &resolution,
    );
    assert!(resolved["remaining"].as_array().unwrap().is_empty());

    assert_eq!(
        fs::read(repo.path().join("src/login.tsx")).unwrap(),
        b"export const design = 'enterprise';"
    );
    assert_eq!(
        fs::read(repo.path().join("src/enterprise.tsx")).unwrap(),
        b"export const marker = 'enterprise';"
    );
    assert!(!repo.path().join("src/minimal.tsx").exists());
    assert!(!repo.path().join("src/playful.tsx").exists());
    assert!(!repo.path().join("src/split.tsx").exists());
    assert!(!repo.path().join("src/focused.tsx").exists());

    for (lane, _) in variants {
        if lane != "login-enterprise" {
            let discarded = repo.run_json(["discard", lane]);
            assert_eq!(discarded["removed"], true);
        }
    }
}
