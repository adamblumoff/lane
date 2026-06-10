use std::fmt::Write as _;

use crate::LaneOpSummary;
use crate::vfs::LaneFileChangeStatus;

use super::{
    ReviewActionKind, ReviewActionOutput, ReviewLaneSummary, ReviewOutput, ReviewPathOutput,
    resolve_op_action, show_op_action,
};

pub(super) fn format(output: &ReviewOutput) -> String {
    let mut text = String::new();
    writeln!(&mut text, "Lane review").expect("write to string cannot fail");
    writeln!(
        &mut text,
        "scope: {}",
        output.lane.as_deref().unwrap_or("all lanes")
    )
    .expect("write to string cannot fail");
    writeln!(&mut text, "repo: {}", output.repo_root).expect("write to string cannot fail");
    writeln!(&mut text, "storage: {}", output.storage_path).expect("write to string cannot fail");
    writeln!(
        &mut text,
        "summary: {}, {}, {}, {}, {}",
        count_label(output.summary.lanes, "lane"),
        count_label(output.summary.changed_paths, "changed path"),
        count_label(output.summary.clean_ops, "clean op"),
        count_label(output.summary.conflicted_ops, "conflicted op"),
        count_label(output.summary.conflict_groups, "conflict group"),
    )
    .expect("write to string cannot fail");

    if output.paths.is_empty() {
        writeln!(&mut text, "\nNo changed paths.").expect("write to string cannot fail");
    } else {
        for path in &output.paths {
            writeln!(&mut text, "\n{}", path.path).expect("write to string cannot fail");
            write_path(&mut text, path);
        }
    }

    write_lane_actions(&mut text, &output.lanes);
    text
}

fn write_path(text: &mut String, path: &ReviewPathOutput) {
    writeln!(text, "  |- lanes").expect("write to string cannot fail");
    if path.lanes.is_empty() {
        writeln!(text, "  |  - none").expect("write to string cannot fail");
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
            )
            .expect("write to string cannot fail");
        }
    }

    writeln!(text, "  |- clean ops").expect("write to string cannot fail");
    if path.clean_ops.is_empty() {
        writeln!(text, "  |  - none").expect("write to string cannot fail");
    } else {
        for op in &path.clean_ops {
            writeln!(text, "  |  - {}", op_label(&op.op)).expect("write to string cannot fail");
            writeln!(
                text,
                "  |    promote: {}",
                format_command([
                    "promote-ops",
                    op.op.lane.as_str(),
                    op.op.path.as_str(),
                    op.op.op_id.as_str(),
                ])
            )
            .expect("write to string cannot fail");
            writeln!(
                text,
                "  |    inspect: {}",
                format_action_command(&show_op_action(op))
            )
            .expect("write to string cannot fail");
        }
    }

    writeln!(text, "  `- conflict groups").expect("write to string cannot fail");
    if path.conflicts.is_empty() {
        writeln!(text, "     - none").expect("write to string cannot fail");
    } else {
        for (index, conflict) in path.conflicts.iter().enumerate() {
            writeln!(
                text,
                "     - group {} [{}..{}), lanes: {}",
                index + 1,
                conflict.range_start,
                conflict.range_end,
                conflict.lanes.join(", "),
            )
            .expect("write to string cannot fail");
            for op in &conflict.ops {
                writeln!(text, "       - {}", op_label(&op.op))
                    .expect("write to string cannot fail");
                writeln!(
                    text,
                    "         inspect: {}",
                    format_action_command(&show_op_action(op))
                )
                .expect("write to string cannot fail");
                writeln!(
                    text,
                    "         resolve: {}",
                    format_action_command(&resolve_op_action(op))
                )
                .expect("write to string cannot fail");
            }
        }
    }
}

fn write_lane_actions(text: &mut String, lanes: &[ReviewLaneSummary]) {
    if lanes.is_empty() {
        return;
    }

    writeln!(text, "\nLane actions").expect("write to string cannot fail");
    for lane in lanes {
        writeln!(text, "  {}:", lane.lane).expect("write to string cannot fail");
        if lane.actions.is_empty() {
            writeln!(text, "    - none").expect("write to string cannot fail");
        } else {
            for action in &lane.actions {
                writeln!(
                    text,
                    "    - {}: {}",
                    action_label(action.kind),
                    format_action_command(action),
                )
                .expect("write to string cannot fail");
            }
        }
    }
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
        write!(label, ", conflicts with {}", op.conflicts_with.join(", "))
            .expect("write to string cannot fail");
    }
    label
}

fn format_action_command(action: &ReviewActionOutput) -> String {
    format_command(action.command.iter().map(String::as_str))
}

fn format_command<'a>(args: impl IntoIterator<Item = &'a str>) -> String {
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

fn action_label(kind: ReviewActionKind) -> &'static str {
    match kind {
        ReviewActionKind::PromoteClean => "promote clean ops",
        ReviewActionKind::ShowOp => "inspect op",
        ReviewActionKind::ResolveOp => "resolve op",
        ReviewActionKind::Discard => "discard lane",
    }
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
