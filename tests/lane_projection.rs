use lane::projection::SourceProjection;

#[test]
fn extensionless_utf8_text_uses_text_projection() {
    let worktree = crlf(b"FROM scratch\nRUN echo ok\n");
    let projection = SourceProjection::from_worktree(worktree.clone());

    assert_eq!(projection.bytes(), b"FROM scratch\nRUN echo ok\n");
    assert_eq!(
        projection.project_edit(b"RUN echo edited\r\n".to_vec()),
        b"RUN echo edited\n"
    );
    assert_eq!(projection.materialize(projection.bytes()), worktree);
}

#[test]
fn nul_containing_bytes_use_raw_projection() {
    let worktree = b"binary\0chunk\r\n".to_vec();
    let projection = SourceProjection::from_worktree(worktree.clone());

    assert_eq!(projection.bytes(), worktree.as_slice());
    assert_eq!(
        projection.project_edit(b"raw edit\r\n".to_vec()),
        b"raw edit\r\n"
    );
    assert_eq!(projection.materialize(projection.bytes()), worktree);
}

fn crlf(bytes: &[u8]) -> Vec<u8> {
    let mut converted = Vec::new();
    for byte in bytes {
        if *byte == b'\n' {
            converted.extend_from_slice(b"\r\n");
        } else {
            converted.push(*byte);
        }
    }
    converted
}
