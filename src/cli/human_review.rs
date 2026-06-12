use std::collections::BTreeMap;
use std::fmt::{self, Write as _};

use crate::LaneOpSummary;
use crate::vfs::LaneFileChangeStatus;

use super::output::{
    BytePreview, ReviewActionKind, ReviewActionOutput, ReviewLaneSummary, ReviewOpOutput,
    ReviewOutput, ReviewPathOutput,
};
use super::review::resolve_op_action;

const HUMAN_PREVIEW_CHAR_LIMIT: usize = 160;
const CLEAN_ONLY_PATH_DETAIL_LIMIT: usize = 20;
const CLEAN_OP_DETAIL_LIMIT_PER_PATH: usize = 12;
const CLEAN_ONLY_PATH_SAMPLE_LIMIT: usize = 8;

pub(super) fn format(output: &ReviewOutput) -> String {
    let mut text = String::new();
    write_review(&mut text, output).expect("write to string cannot fail");
    text
}

fn write_review(text: &mut String, output: &ReviewOutput) -> fmt::Result {
    writeln!(text, "Lane review")?;
    writeln!(
        text,
        "scope: {}",
        output.lane.as_deref().unwrap_or("all lanes")
    )?;
    writeln!(text, "repo: {}", output.repo_root)?;
    writeln!(text, "storage: {}", output.storage_path)?;
    writeln!(
        text,
        "summary: {}, {}, {}, {}, {}",
        count_label(output.summary.lanes, "lane"),
        count_label(output.summary.changed_paths, "changed path"),
        count_label(output.summary.clean_ops, "clean op"),
        count_label(output.summary.conflicted_ops, "conflicted op"),
        count_label(output.summary.conflict_groups, "conflict group"),
    )?;

    write_lane_status(text, &output.lanes)?;
    write_promotable_now(text, &output.lanes, &output.paths)?;
    write_decision_queue(text, &output.paths)?;

    if output.paths.is_empty() {
        writeln!(text, "\nNo changed paths.")?;
    } else {
        let compact_clean_only_paths = should_compact_clean_only_paths(&output.paths);
        for path in &output.paths {
            if compact_clean_only_paths && is_clean_only_path(path) {
                continue;
            }
            writeln!(text, "\n{}", path.path)?;
            write_path(text, path)?;
        }
        if compact_clean_only_paths {
            write_clean_only_paths(text, output)?;
        }
    }

    write_discard_actions(text, &output.lanes)
}

fn write_path(text: &mut String, path: &ReviewPathOutput) -> fmt::Result {
    writeln!(text, "  |- lanes")?;
    if path.lanes.is_empty() {
        writeln!(text, "  |  - none")?;
    } else {
        for lane in &path.lanes {
            writeln!(
                text,
                "  |  - {} {}, {} ({} clean, {} conflicted), {} -> {}",
                lane.lane,
                change_status_label(lane.status),
                count_label(lane.total_ops, "op"),
                lane.clean_ops,
                lane.conflicted_ops,
                optional_bytes_label(lane.base_size),
                optional_bytes_label(lane.lane_size),
            )?;
        }
    }

    writeln!(text, "  |- clean ops")?;
    if path.clean_ops.is_empty() {
        writeln!(text, "  |  - none")?;
    } else {
        let detailed_count = path.clean_ops.len().min(CLEAN_OP_DETAIL_LIMIT_PER_PATH);
        for op in path.clean_ops.iter().take(detailed_count) {
            writeln!(text, "  |  - {}", op_label(&op.op))?;
            write_op_previews(text, "  |    ", op)?;
            writeln!(
                text,
                "  |    promote: {}",
                format_command([
                    "promote-ops",
                    op.op.lane.as_str(),
                    op.op.path.as_str(),
                    op.op.op_id.as_str(),
                ])
            )?;
        }
        if path.clean_ops.len() > detailed_count {
            write_omitted_clean_ops(text, path, &path.clean_ops[detailed_count..])?;
        }
    }

    writeln!(text, "  `- conflict groups")?;
    if path.conflicts.is_empty() {
        writeln!(text, "     - none")?;
    } else {
        for (index, conflict) in path.conflicts.iter().enumerate() {
            writeln!(
                text,
                "     - group {} [{}..{}), lanes: {}",
                index + 1,
                conflict.range_start,
                conflict.range_end,
                conflict.lanes.join(", "),
            )?;
            for op in &conflict.ops {
                writeln!(text, "       - {}", op_label(&op.op))?;
                write_op_previews(text, "         ", op)?;
                writeln!(
                    text,
                    "         resolve: {}",
                    format_action_command(&resolve_op_action(op))
                )?;
            }
        }
    }
    Ok(())
}

fn should_compact_clean_only_paths(paths: &[ReviewPathOutput]) -> bool {
    paths.iter().filter(|path| is_clean_only_path(path)).count() > CLEAN_ONLY_PATH_DETAIL_LIMIT
}

fn is_clean_only_path(path: &ReviewPathOutput) -> bool {
    !path.clean_ops.is_empty() && path.conflicts.is_empty()
}

fn write_clean_only_paths(text: &mut String, output: &ReviewOutput) -> fmt::Result {
    let clean_only_paths = output
        .paths
        .iter()
        .filter(|path| is_clean_only_path(path))
        .collect::<Vec<_>>();
    if clean_only_paths.is_empty() {
        return Ok(());
    }

    writeln!(text, "\nClean-only paths")?;
    writeln!(
        text,
        "  - {} omitted from detailed listing",
        count_label(clean_only_paths.len(), "clean-only path")
    )?;
    for lane in &output.lanes {
        let clean_ops = clean_only_paths
            .iter()
            .flat_map(|path| &path.clean_ops)
            .filter(|op| op.op.lane == lane.lane)
            .count();
        if clean_ops == 0 {
            continue;
        }
        let path_count = clean_only_paths
            .iter()
            .filter(|path| path.clean_ops.iter().any(|op| op.op.lane == lane.lane))
            .count();
        writeln!(
            text,
            "  - {}: {} across {}",
            lane.lane,
            count_label(clean_ops, "clean op"),
            count_label(path_count, "path")
        )?;
        if let Some(action) = lane
            .actions
            .iter()
            .find(|action| matches!(action.kind, ReviewActionKind::PromoteClean))
        {
            writeln!(text, "    command: {}", format_action_command(action))?;
        }
    }
    writeln!(
        text,
        "  - full JSON details: {}",
        format_review_command(output.lane.as_deref())
    )?;
    writeln!(text, "  - sample paths")?;
    for path in clean_only_paths.iter().take(CLEAN_ONLY_PATH_SAMPLE_LIMIT) {
        writeln!(text, "    - {}", path.path)?;
    }
    if clean_only_paths.len() > CLEAN_ONLY_PATH_SAMPLE_LIMIT {
        writeln!(
            text,
            "    - ... {}",
            count_label(
                clean_only_paths.len() - CLEAN_ONLY_PATH_SAMPLE_LIMIT,
                "more path"
            )
        )?;
    }

    Ok(())
}

fn write_omitted_clean_ops(
    text: &mut String,
    path: &ReviewPathOutput,
    omitted: &[ReviewOpOutput],
) -> fmt::Result {
    writeln!(
        text,
        "  |  - ... {} omitted from this path",
        count_label(omitted.len(), "clean op")
    )?;

    let mut omitted_by_lane = BTreeMap::<&str, Vec<&str>>::new();
    for op in omitted {
        omitted_by_lane
            .entry(op.op.lane.as_str())
            .or_default()
            .push(op.op.op_id.as_str());
    }
    for (lane, op_ids) in omitted_by_lane {
        let mut command = vec!["promote-ops", lane, path.path.as_str()];
        command.extend(op_ids.iter().copied());
        writeln!(
            text,
            "  |    {}: {} omitted; promote: {}",
            lane,
            count_label(op_ids.len(), "clean op"),
            format_command(command)
        )?;
    }

    Ok(())
}

fn write_lane_status(text: &mut String, lanes: &[ReviewLaneSummary]) -> fmt::Result {
    writeln!(text, "\nLane status")?;
    if lanes.is_empty() {
        writeln!(text, "  - none")?;
        return Ok(());
    }

    for lane in lanes {
        writeln!(
            text,
            "  - {}: {}, {}, {}, {}",
            lane.lane,
            count_label(lane.changed_paths, "changed path"),
            count_label(lane.clean_ops, "clean op"),
            count_label(lane.conflicted_ops, "conflicted op"),
            last_exec_label(lane.last_exec.as_ref()),
        )?;
        if let Some(detail) = last_exec_detail(lane.last_exec.as_ref()) {
            writeln!(text, "    {detail}")?;
        }
    }
    Ok(())
}

fn write_promotable_now(
    text: &mut String,
    lanes: &[ReviewLaneSummary],
    paths: &[ReviewPathOutput],
) -> fmt::Result {
    writeln!(text, "\nPromotable now")?;
    let mut wrote_lane = false;
    for lane in lanes.iter().filter(|lane| lane.clean_ops > 0) {
        wrote_lane = true;
        writeln!(
            text,
            "  - {}: {} across {}, {} total, {}",
            lane.lane,
            count_label(lane.clean_ops, "clean op"),
            count_label(promotable_path_count(paths, &lane.lane), "path"),
            count_label(lane.changed_paths, "changed path"),
            last_exec_label(lane.last_exec.as_ref()),
        )?;
        if let Some(action) = lane
            .actions
            .iter()
            .find(|action| matches!(action.kind, ReviewActionKind::PromoteClean))
        {
            writeln!(text, "    command: {}", format_action_command(action))?;
        }
    }

    if !wrote_lane {
        writeln!(text, "  - none")?;
    }
    Ok(())
}

fn promotable_path_count(paths: &[ReviewPathOutput], lane: &str) -> usize {
    paths
        .iter()
        .filter(|path| path.clean_ops.iter().any(|op| op.op.lane == lane))
        .count()
}

fn write_decision_queue(text: &mut String, paths: &[ReviewPathOutput]) -> fmt::Result {
    writeln!(text, "\nNeeds decision")?;
    let mut wrote_conflict = false;
    for path in paths {
        for (index, conflict) in path.conflicts.iter().enumerate() {
            wrote_conflict = true;
            writeln!(
                text,
                "  - {} group {} [{}..{}), {}, lanes: {}",
                path.path,
                index + 1,
                conflict.range_start,
                conflict.range_end,
                count_label(conflict.ops.len(), "op"),
                conflict.lanes.join(", "),
            )?;
        }
    }

    if !wrote_conflict {
        writeln!(text, "  - none")?;
    }
    Ok(())
}

fn write_discard_actions(text: &mut String, lanes: &[ReviewLaneSummary]) -> fmt::Result {
    if lanes.is_empty() {
        return Ok(());
    }

    writeln!(text, "\nDiscard lanes")?;
    for lane in lanes {
        if let Some(action) = lane
            .actions
            .iter()
            .find(|action| matches!(action.kind, ReviewActionKind::Discard))
        {
            writeln!(text, "  - {}: {}", lane.lane, format_action_command(action),)?;
        }
    }
    Ok(())
}

fn write_op_previews(text: &mut String, indent: &str, op: &ReviewOpOutput) -> fmt::Result {
    writeln!(text, "{indent}base: {}", preview_label(&op.base))?;
    writeln!(text, "{indent}inserted: {}", preview_label(&op.inserted))
}

fn op_label(op: &LaneOpSummary) -> String {
    let mut label = format!(
        "{} {} {} [{}..{}), inserts {}",
        op.lane,
        op.op_id,
        op_kind_label(op.kind),
        op.base_start,
        op.base_end,
        bytes_label(op.inserted_len),
    );
    if !op.conflicts_with.is_empty() {
        label.push_str(", conflicts with ");
        label.push_str(&op.conflicts_with.join(", "));
    }
    label
}

fn format_action_command(action: &ReviewActionOutput) -> String {
    format_command(action.command.iter().map(String::as_str))
}

fn format_review_command(lane: Option<&str>) -> String {
    if let Some(lane) = lane {
        format_command(["review", lane])
    } else {
        format_command(["review"])
    }
}

pub(super) fn format_command<'a>(args: impl IntoIterator<Item = &'a str>) -> String {
    std::iter::once("lane")
        .chain(args)
        .map(quote_command_arg)
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_command_arg(arg: &str) -> String {
    if arg == "<replacement-file>" || is_plain_command_arg(arg) {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', "''"))
    }
}

fn is_plain_command_arg(arg: &str) -> bool {
    !arg.is_empty()
        && arg.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'-' | b'_' | b'.' | b'/' | b'\\' | b':' | b'=' | b'+' | b',' | b'@' | b'%'
                )
        })
}

fn last_exec_label(last_exec: Option<&crate::LaneExecState>) -> String {
    let Some(last_exec) = last_exec else {
        return "last exec unavailable".to_owned();
    };

    let status = if last_exec.worker_error.is_some() {
        "last exec worker error".to_owned()
    } else {
        match last_exec.exit_code {
            Some(0) => "last exec ok".to_owned(),
            Some(code) => format!("last exec exit {code}"),
            None => "last exec no exit code".to_owned(),
        }
    };

    format!(
        "{status}, exec touched {}",
        count_label(last_exec.changed_paths.len(), "path")
    )
}

fn last_exec_detail(last_exec: Option<&crate::LaneExecState>) -> Option<String> {
    let last_exec = last_exec?;
    if let Some(error) = &last_exec.worker_error {
        return Some(format!("worker error: {}", one_line_preview(error)));
    }

    if last_exec.exit_code == Some(0) {
        return None;
    }

    if !last_exec.stderr.text.trim().is_empty() {
        let truncated = if last_exec.stderr.truncated {
            " (truncated)"
        } else {
            ""
        };
        return Some(format!(
            "stderr: {}{truncated}",
            one_line_preview(&last_exec.stderr.text),
        ));
    }

    if !last_exec.stdout.text.trim().is_empty() {
        let truncated = if last_exec.stdout.truncated {
            " (truncated)"
        } else {
            ""
        };
        return Some(format!(
            "stdout: {}{truncated}",
            one_line_preview(&last_exec.stdout.text),
        ));
    }

    None
}

fn preview_label(preview: &BytePreview) -> String {
    if preview.len == 0 {
        return "<empty>".to_owned();
    }

    let Some(text) = &preview.utf8 else {
        return format!(
            "<binary {}, sha256 {}>",
            bytes_label(preview.len as u64),
            short_sha(&preview.sha256)
        );
    };

    let (escaped, shortened) = escaped_text_preview(text);
    if shortened || preview.truncated {
        format!(
            "\"{escaped}...\" ({}, sha256 {})",
            bytes_label(preview.len as u64),
            short_sha(&preview.sha256)
        )
    } else {
        format!("\"{escaped}\"")
    }
}

fn escaped_text_preview(text: &str) -> (String, bool) {
    let mut escaped = String::new();
    for character in text.chars() {
        let sequence = escaped_character(character);
        if escaped.len() + sequence.len() > HUMAN_PREVIEW_CHAR_LIMIT {
            return (escaped, true);
        }
        escaped.push_str(&sequence);
    }
    (escaped, false)
}

fn escaped_character(character: char) -> String {
    if character == '"' {
        "\\\"".to_owned()
    } else {
        character.escape_default().collect()
    }
}

fn one_line_preview(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    let (escaped, shortened) = escaped_text_preview(line);
    if shortened {
        format!("\"{escaped}...\"")
    } else {
        format!("\"{escaped}\"")
    }
}

fn short_sha(sha256: &str) -> &str {
    sha256.get(..12).unwrap_or(sha256)
}

fn op_kind_label(kind: crate::LaneOpKind) -> &'static str {
    match kind {
        crate::LaneOpKind::Create => "create",
        crate::LaneOpKind::Insert => "insert",
        crate::LaneOpKind::Delete => "delete",
        crate::LaneOpKind::Replace => "replace",
    }
}

fn change_status_label(status: LaneFileChangeStatus) -> &'static str {
    match status {
        LaneFileChangeStatus::Created => "created",
        LaneFileChangeStatus::Modified => "modified",
        LaneFileChangeStatus::Deleted => "deleted",
    }
}

fn count_label(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {singular}s")
    }
}

fn optional_bytes_label(bytes: Option<usize>) -> String {
    bytes
        .map(|bytes| bytes_label(bytes as u64))
        .unwrap_or_else(|| "missing".to_owned())
}

fn bytes_label(bytes: u64) -> String {
    if bytes == 1 {
        "1 B".to_owned()
    } else {
        format!("{bytes} B")
    }
}
