#![cfg(windows)]

use lane::virtual_exec::{VirtualExecError, last_exec_warnings};

#[test]
fn last_exec_record_failure_becomes_warning() {
    let warnings = last_exec_warnings(Err(VirtualExecError::message("storage busy")));

    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].kind, "last_exec_not_recorded");
    assert!(
        warnings[0]
            .message
            .contains("failed to record advisory last_exec metadata: storage busy")
    );
}

#[test]
fn last_exec_record_success_has_no_warnings() {
    assert!(last_exec_warnings(Ok(())).is_empty());
}
