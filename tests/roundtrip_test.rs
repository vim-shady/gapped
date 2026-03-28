use crate::helper::{TestFixture, create_file, run_gapped};
use std::fs::remove_file;
use std::os::unix::fs::symlink;
use std::thread;

#[path = "helper.rs"]
mod helper;

#[test]
fn test_roundtrip() {
    let fixture = TestFixture::new();

    // Create source content
    create_file(&fixture.source().join("file1.txt"), "hello world\n");
    create_file(&fixture.source().join("file2.txt"), "hello world again\n");
    create_file(
        &fixture.source().join("subdir/nested.txt"),
        "hello world nested\n",
    );
    symlink("file1.txt", fixture.source().join("link1")).unwrap();

    // Inital snapshot
    let snap1 = fixture.working_file("snap1");
    assert!(run_gapped(&[
        "snapshot",
        fixture.source().to_str().unwrap(),
        snap1.to_str().unwrap(),
    ]));

    // Copy source to target
    fixture.sync_source_to_target();
    assert!(fixture.verify_rsync_identical());

    // Make changes
    thread::sleep(std::time::Duration::from_secs(1));
    create_file(&fixture.source().join("file1.txt"), "modified content\n");
    create_file(&fixture.source().join("file3.txt"), "new file content\n");
    remove_file(fixture.source().join("file2.txt")).unwrap();
    create_file(
        &fixture.source().join("subdir/nested.txt"),
        "modified nested\n",
    );
    remove_file(fixture.source().join("link1")).unwrap();
    symlink("file3.txt", fixture.source().join("link1")).unwrap();

    // DIff
    let diff1 = fixture.working_file("diff1");
    let snap2 = fixture.working_file("snap2");
    assert!(run_gapped(&[
        "diff",
        fixture.source().to_str().unwrap(),
        snap1.to_str().unwrap(),
        diff1.to_str().unwrap(),
        snap2.to_str().unwrap(),
    ]));

    // Apply
    assert!(run_gapped(&[
        "apply",
        fixture.target().to_str().unwrap(),
        diff1.to_str().unwrap(),
    ]));

    // Verify
    assert!(fixture.verify_rsync_identical());
}
