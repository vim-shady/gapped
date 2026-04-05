use crate::helper::{TestFixture, create_file, run_gapped};
use std::fs::{remove_dir_all, remove_file};
use std::os::unix::fs::symlink;
use std::time::Duration;
use std::{fs, thread};

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

#[test]
fn test_kind_change() {
    let fixture = TestFixture::new();

    // Create inital state
    create_file(&fixture.source().join("subject1"), "I am a file\n");
    symlink("subject1", fixture.source().join("subject2")).unwrap();

    let snap1 = fixture.working_file("snap1");
    assert!(run_gapped(&[
        "snapshot",
        fixture.source().to_str().unwrap(),
        snap1.to_str().unwrap(),
    ]));
    fixture.sync_source_to_target();

    // Change kind
    thread::sleep(std::time::Duration::from_secs(1));
    remove_file(fixture.source().join("subject1")).unwrap();
    symlink("subject2", fixture.source().join("subject1")).unwrap();
    remove_file(fixture.source().join("subject2")).unwrap();
    create_file(&fixture.source().join("subject2"), "I am now a file\n");

    let diff1 = fixture.working_file("diff1");
    let snap2 = fixture.working_file("snap2");
    assert!(run_gapped(&[
        "diff",
        fixture.source().to_str().unwrap(),
        snap1.to_str().unwrap(),
        diff1.to_str().unwrap(),
        snap2.to_str().unwrap(),
    ]));

    assert!(run_gapped(&[
        "apply",
        fixture.target().to_str().unwrap(),
        diff1.to_str().unwrap(),
    ]));

    assert!(fixture.verify_rsync_identical());
}

#[test]
fn test_nesting() {
    let fixture = TestFixture::new();

    create_file(&fixture.source().join("a/b/c/d/deep.txt"), "deep\n");

    let snap1 = fixture.working_file("snap1");
    assert!(run_gapped(&[
        "snapshot",
        fixture.source().to_str().unwrap(),
        snap1.to_str().unwrap(),
    ]));
    fixture.sync_source_to_target();

    // Add deeper, modify existing
    thread::sleep(std::time::Duration::from_secs(1));
    create_file(
        &fixture.source().join("a/b/c/d/e/deeper.txt"),
        "even deeper\n",
    );
    create_file(&fixture.source().join("a/b/c/d/deep.txt"), "modified\n");

    let diff1 = fixture.working_file("diff1");
    let snap2 = fixture.working_file("snap2");
    assert!(run_gapped(&[
        "diff",
        fixture.source().to_str().unwrap(),
        snap1.to_str().unwrap(),
        diff1.to_str().unwrap(),
        snap2.to_str().unwrap(),
    ]));

    assert!(run_gapped(&[
        "apply",
        fixture.target().to_str().unwrap(),
        diff1.to_str().unwrap(),
    ]));

    assert!(fixture.verify_rsync_identical());
}

#[test]
fn test_directory_removal() {
    let fixture = TestFixture::new();

    create_file(&fixture.source().join("dir/file1.txt"), "content\n");
    create_file(&fixture.source().join("dir/subdir/file2.txt"), "nested\n");

    let snap1 = fixture.working_file("snap1");
    assert!(run_gapped(&[
        "snapshot",
        fixture.source().to_str().unwrap(),
        snap1.to_str().unwrap(),
    ]));
    fixture.sync_source_to_target();

    // delete entire directory tree
    thread::sleep(std::time::Duration::from_secs(1));
    remove_dir_all(fixture.source().join("dir")).unwrap();

    let diff1 = fixture.working_file("diff1");
    let snap2 = fixture.working_file("snap2");
    assert!(run_gapped(&[
        "diff",
        fixture.source().to_str().unwrap(),
        snap1.to_str().unwrap(),
        diff1.to_str().unwrap(),
        snap2.to_str().unwrap(),
    ]));

    assert!(run_gapped(&[
        "apply",
        fixture.target().to_str().unwrap(),
        diff1.to_str().unwrap(),
    ]));

    assert!(fixture.verify_rsync_identical());
}

#[test]
fn test_verify() {
    let fixture = TestFixture::new();

    create_file(&fixture.source().join("file1.txt"), "content\n");

    let snap1 = fixture.working_file("snap1");
    assert!(run_gapped(&[
        "snapshot",
        fixture.source().to_str().unwrap(),
        snap1.to_str().unwrap(),
    ]));
    fixture.sync_source_to_target();

    thread::sleep(std::time::Duration::from_secs(1));
    create_file(&fixture.source().join("file1.txt"), "modfified\n");
    create_file(&fixture.source().join("file2.txt"), "new file\n");

    let diff1 = fixture.working_file("diff1");
    let snap2 = fixture.working_file("snap2");
    assert!(run_gapped(&[
        "diff",
        fixture.source().to_str().unwrap(),
        snap1.to_str().unwrap(),
        diff1.to_str().unwrap(),
        snap2.to_str().unwrap(),
    ]));

    // Apply diff first
    assert!(run_gapped(&[
        "apply",
        fixture.target().to_str().unwrap(),
        diff1.to_str().unwrap(),
    ]));

    // Take snapshot of target after apply
    let target_snap = fixture.working_file("target_snap");
    assert!(run_gapped(&[
        "snapshot",
        fixture.target().to_str().unwrap(),
        target_snap.to_str().unwrap(),
    ]));

    // make some more changes
    thread::sleep(std::time::Duration::from_secs(1));
    create_file(&fixture.source().join("file1.txt"), "modified again\n");
    remove_file(fixture.source().join("file2.txt")).unwrap();

    let diff2 = fixture.working_file("diff2");
    let snap3 = fixture.working_file("snap3");
    assert!(run_gapped(&[
        "diff",
        fixture.source().to_str().unwrap(),
        snap2.to_str().unwrap(),
        diff2.to_str().unwrap(),
        snap3.to_str().unwrap(),
    ]));

    // applying diff2 to current target should produce snap3
    assert!(run_gapped(&[
        "verify",
        fixture.target().to_str().unwrap(),
        diff2.to_str().unwrap(),
        snap3.to_str().unwrap(),
    ]))
}

#[test]
fn test_iterative_sync() {
    let fixture = TestFixture::new();

    // Setup
    create_file(&fixture.source().join("file1.txt"), "v1\n");

    let snap1 = fixture.working_file("snap1");
    assert!(run_gapped(&[
        "snapshot",
        fixture.source().to_str().unwrap(),
        snap1.to_str().unwrap(),
    ]));
    fixture.sync_source_to_target();

    // round 1
    thread::sleep(std::time::Duration::from_secs(1));
    create_file(&fixture.source().join("file1.txt"), "v2\n");
    create_file(&fixture.source().join("file2.txt"), "new\n");

    let diff1 = fixture.working_file("diff1");
    let snap2 = fixture.working_file("snap2");
    assert!(run_gapped(&[
        "diff",
        fixture.source().to_str().unwrap(),
        snap1.to_str().unwrap(),
        diff1.to_str().unwrap(),
        snap2.to_str().unwrap(),
    ]));
    assert!(run_gapped(&[
        "apply",
        fixture.target().to_str().unwrap(),
        diff1.to_str().unwrap(),
    ]));
    assert!(fixture.verify_rsync_identical());

    // round 2
    thread::sleep(std::time::Duration::from_secs(1));
    remove_file(fixture.source().join("file2.txt")).unwrap();
    create_file(&fixture.source().join("file3.txt"), "new\n");

    let diff2 = fixture.working_file("diff2");
    let snap3 = fixture.working_file("snap3");
    assert!(run_gapped(&[
        "diff",
        fixture.source().to_str().unwrap(),
        snap2.to_str().unwrap(),
        diff2.to_str().unwrap(),
        snap3.to_str().unwrap(),
    ]));
    assert!(run_gapped(&[
        "apply",
        fixture.target().to_str().unwrap(),
        diff2.to_str().unwrap(),
    ]));
    assert!(fixture.verify_rsync_identical());
}

#[test]
fn test_split_diff_roundtrip() {
    let fixture = TestFixture::new();

    // seed enough files so a small split-size produces multiple chunks
    for i in 0..12 {
        create_file(
            &fixture.source().join(format!("file_{:02}.txt", i)),
            &format!("initial content {}\n", i).repeat(32),
        );
    }
    create_file(
        &fixture.source().join("subdir/nested.txt"),
        "nested content\n",
    );

    fixture.sync_source_to_target();
    assert!(fixture.verify_rsync_identical());

    // Snapshot the TARGET itself to get its true state (rsync doesn't
    // preserve sub-second mtimes reliably...
    let target_snap = fixture.working_file("target_snap");
    assert!(run_gapped(&[
        "snapshot",
        fixture.target().to_str().unwrap(),
        target_snap.to_str().unwrap(),
    ]));

    // mutate source
    thread::sleep(Duration::from_secs(1));
    for i in 0..12 {
        create_file(
            &fixture.source().join(format!("file_{:02}.txt", i)),
            &format!("updated content {}\n", i).repeat(64),
        );
    }
    create_file(&fixture.source().join("brand_new.txt"), "fresh\n");
    remove_file(fixture.source().join("file_00.txt")).unwrap();

    // diff against the target snapshot with a small split size
    let diff_base = fixture.working_file("diff.gapped");
    let snap2 = fixture.working_file("snap2");
    assert!(run_gapped(&[
        "diff",
        fixture.source().to_str().unwrap(),
        target_snap.to_str().unwrap(),
        diff_base.to_str().unwrap(),
        snap2.to_str().unwrap(),
        "--split-size",
        "4096",
    ]));

    assert!(
        !diff_base.exists(),
        "base diff file should not exist when splitting"
    );
    let chunk1 = fixture.working_dir.path().join(format!(
        "{}.001",
        diff_base.file_name().unwrap().to_string_lossy()
    ));
    let chunk2 = fixture.working_dir.path().join(format!(
        "{}.002",
        diff_base.file_name().unwrap().to_string_lossy()
    ));
    assert!(chunk1.exists(), "expected first chunk at {:?}", chunk1);
    assert!(chunk2.exists(), "expected at least two chunks");

    assert!(run_gapped(&[
        "verify",
        fixture.target().to_str().unwrap(),
        diff_base.to_str().unwrap(),
        snap2.to_str().unwrap(),
    ]));

    assert!(run_gapped(&[
        "apply",
        fixture.target().to_str().unwrap(),
        diff_base.to_str().unwrap(),
    ]));

    assert!(fixture.verify_rsync_identical());
}

#[test]
fn test_permission_change() {
    let fixture = TestFixture::new();

    create_file(&fixture.source().join("script.sh"), "#/bin/bash\n");

    let snap1 = fixture.working_file("snap1");
    assert!(run_gapped(&[
        "snapshot",
        fixture.source().to_str().unwrap(),
        snap1.to_str().unwrap(),
    ]));
    fixture.sync_source_to_target();

    // change permissisons only
    thread::sleep(Duration::from_secs(1));
    fs::set_permissions(
        fixture.source().join("script.sh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();

    let diff1 = fixture.working_file("diff1");
    let snap2 = fixture.working_file("snap2");
    assert!(run_gapped(&[
        "diff",
        fixture.source().to_str().unwrap(),
        snap1.to_str().unwrap(),
        diff1.to_str().unwrap(),
        snap2.to_str().unwrap(),
    ]));

    assert!(run_gapped(&[
        "apply",
        fixture.target().to_str().unwrap(),
        diff1.to_str().unwrap(),
    ]));

    assert!(fixture.verify_rsync_identical());
}
