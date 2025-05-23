// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_io as fio;
use io_conformance_util::test_harness::TestHarness;
use io_conformance_util::*;

#[fuchsia::test]
async fn file_read_with_sufficient_rights() {
    let harness = TestHarness::new().await;
    let entries = vec![file(TEST_FILE, vec![])];
    let dir = harness.get_directory(entries, harness.dir_rights.all_flags());

    for flags in harness.file_rights.combinations_containing(fio::Rights::READ_BYTES) {
        let file = dir.open_node::<fio::FileMarker>(TEST_FILE, flags, None).await.unwrap();
        let _data: Vec<u8> = file
            .read(0)
            .await
            .expect("read failed")
            .map_err(zx::Status::from_raw)
            .expect("read error");
    }
}

#[fuchsia::test]
async fn file_read_with_insufficient_rights() {
    let harness = TestHarness::new().await;
    let entries = vec![file(TEST_FILE, vec![])];
    let dir = harness.get_directory(entries, harness.dir_rights.all_flags());

    for flags in harness.file_rights.combinations_without(fio::Rights::READ_BYTES) {
        let file = dir.open_node::<fio::FileMarker>(TEST_FILE, flags, None).await.unwrap();
        let result = file.read(0).await.expect("read failed").map_err(zx::Status::from_raw);
        assert_eq!(result, Err(zx::Status::BAD_HANDLE))
    }
}

#[fuchsia::test]
async fn file_read_with_max_transfer() {
    let harness = TestHarness::new().await;
    let contents = vec![0u8; fio::MAX_TRANSFER_SIZE as usize];
    let entries = vec![file(TEST_FILE, contents)];
    let dir = harness.get_directory(entries, harness.dir_rights.all_flags());

    for flags in harness.file_rights.combinations_containing(fio::Rights::READ_BYTES) {
        let file = dir.open_node::<fio::FileMarker>(TEST_FILE, flags, None).await.unwrap();
        let len = file
            .read(fio::MAX_TRANSFER_SIZE)
            .await
            .expect("read failed")
            .map_err(zx::Status::from_raw)
            .expect("read error")
            .len();
        assert_eq!(len, fio::MAX_TRANSFER_SIZE as usize);
    }
}

#[fuchsia::test]
async fn file_read_over_max_transfer() {
    let harness = TestHarness::new().await;
    let contents = vec![0u8; fio::MAX_TRANSFER_SIZE as usize + 1];
    let entries = vec![file(TEST_FILE, contents)];
    let dir = harness.get_directory(entries, harness.dir_rights.all_flags());

    for flags in harness.file_rights.combinations_containing(fio::Rights::READ_BYTES) {
        let file = dir.open_node::<fio::FileMarker>(TEST_FILE, flags, None).await.unwrap();
        let result = file
            .read(fio::MAX_TRANSFER_SIZE + 1)
            .await
            .expect("read failed")
            .map_err(zx::Status::from_raw);
        assert_eq!(result, Err(zx::Status::OUT_OF_RANGE))
    }
}

#[fuchsia::test]
async fn file_read_at_with_sufficient_rights() {
    let harness = TestHarness::new().await;
    let entries = vec![file(TEST_FILE, vec![])];
    let dir = harness.get_directory(entries, harness.dir_rights.all_flags());

    for flags in harness.file_rights.combinations_containing(fio::Rights::READ_BYTES) {
        let file = dir.open_node::<fio::FileMarker>(TEST_FILE, flags, None).await.unwrap();
        let _: Vec<u8> = file
            .read_at(0, 0)
            .await
            .expect("read_at failed")
            .map_err(zx::Status::from_raw)
            .expect("read_at error");
    }
}

#[fuchsia::test]
async fn file_read_at_with_insufficient_rights() {
    let harness = TestHarness::new().await;
    let entries = vec![file(TEST_FILE, vec![])];
    let dir = harness.get_directory(entries, harness.dir_rights.all_flags());

    for flags in harness.file_rights.combinations_without(fio::Rights::READ_BYTES) {
        let file = dir.open_node::<fio::FileMarker>(TEST_FILE, flags, None).await.unwrap();
        let result =
            file.read_at(0, 0).await.expect("read_at failed").map_err(zx::Status::from_raw);
        assert_eq!(result, Err(zx::Status::BAD_HANDLE))
    }
}

#[fuchsia::test]
async fn file_read_at_with_max_transfer() {
    let harness = TestHarness::new().await;
    let contents = vec![0u8; fio::MAX_TRANSFER_SIZE as usize];
    let entries = vec![file(TEST_FILE, contents)];
    let dir = harness.get_directory(entries, harness.dir_rights.all_flags());

    for flags in harness.file_rights.combinations_containing(fio::Rights::READ_BYTES) {
        let file = dir.open_node::<fio::FileMarker>(TEST_FILE, flags, None).await.unwrap();
        let len = file
            .read_at(fio::MAX_TRANSFER_SIZE, 0)
            .await
            .expect("read_at failed")
            .map_err(zx::Status::from_raw)
            .expect("read_at error")
            .len();
        assert_eq!(len, fio::MAX_TRANSFER_SIZE as usize)
    }
}

#[fuchsia::test]
async fn file_read_at_over_max_transfer() {
    let harness = TestHarness::new().await;
    let contents = vec![0u8; fio::MAX_TRANSFER_SIZE as usize + 1];
    let entries = vec![file(TEST_FILE, contents)];
    let dir = harness.get_directory(entries, harness.dir_rights.all_flags());

    for flags in harness.file_rights.combinations_containing(fio::Rights::READ_BYTES) {
        let file = dir.open_node::<fio::FileMarker>(TEST_FILE, flags, None).await.unwrap();
        let result = file
            .read_at(fio::MAX_TRANSFER_SIZE + 1, 0)
            .await
            .expect("read_at failed")
            .map_err(zx::Status::from_raw);
        assert_eq!(result, Err(zx::Status::OUT_OF_RANGE))
    }
}

#[fuchsia::test]
async fn file_read_in_subdirectory() {
    let harness = TestHarness::new().await;
    let entries = vec![directory("subdir", vec![file("testing.txt", vec![])])];
    let dir = harness.get_directory(entries, harness.dir_rights.all_flags());

    for flags in harness.file_rights.combinations_containing(fio::Rights::READ_BYTES) {
        let file =
            dir.open_node::<fio::FileMarker>("subdir/testing.txt", flags, None).await.unwrap();
        let _data: Vec<u8> = file
            .read(0)
            .await
            .expect("read failed")
            .map_err(zx::Status::from_raw)
            .expect("read error");
    }
}
