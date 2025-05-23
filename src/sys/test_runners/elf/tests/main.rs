// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error};
use fidl_fuchsia_test_manager as ftest_manager;
use ftest_manager::{CaseStatus, RunSuiteOptions, SuiteStatus};
use pretty_assertions::assert_eq;
use test_manager_test_lib::{GroupRunEventByTestCase, RunEvent};

fn default_options() -> RunSuiteOptions {
    RunSuiteOptions::default()
}

pub async fn run_test(
    test_url: &str,
    run_options: RunSuiteOptions,
) -> Result<Vec<RunEvent>, Error> {
    let suite_runner = test_runners_test_lib::connect_to_suite_runner().await?;
    let runner = test_manager_test_lib::SuiteRunner::new(suite_runner);
    let suite_instance =
        runner.start_suite_run(test_url, run_options).context("suite runner execution failed")?;
    let (events, _logs) = test_runners_test_lib::process_events(suite_instance, false).await?;
    Ok(events)
}

#[fuchsia_async::run_singlethreaded(test)]
async fn launch_and_run_passing_test() {
    let test_url = "fuchsia-pkg://fuchsia.com/elf-test-runner-example-tests#meta/passing_test.cm";

    let events = run_test(test_url, default_options()).await.unwrap().into_iter().group();

    let expected_events = vec![
        RunEvent::suite_started(),
        RunEvent::case_found("main"),
        RunEvent::case_started("main"),
        RunEvent::case_stdout("main", "stdout msg1"),
        RunEvent::case_stdout("main", "stdout msg2"),
        RunEvent::case_stderr("main", "stderr msg1"),
        RunEvent::case_stderr("main", "stderr msg2"),
        RunEvent::case_stopped("main", CaseStatus::Passed),
        RunEvent::case_finished("main"),
        RunEvent::suite_stopped(SuiteStatus::Passed),
    ]
    .into_iter()
    .group();
    assert_eq!(events, expected_events);
}

#[fuchsia_async::run_singlethreaded(test)]
async fn launch_and_run_failing_test() {
    let test_url = "fuchsia-pkg://fuchsia.com/elf-test-runner-example-tests#meta/failing_test.cm";

    let events = run_test(test_url, default_options()).await.unwrap();

    let expected_events = vec![
        RunEvent::suite_started(),
        RunEvent::case_found("main"),
        RunEvent::case_started("main"),
        RunEvent::case_stopped("main", CaseStatus::Failed),
        RunEvent::case_finished("main"),
        RunEvent::suite_stopped(SuiteStatus::Failed),
    ];
    assert_eq!(events, expected_events);
}

#[fuchsia_async::run_singlethreaded(test)]
async fn launch_and_run_test_with_custom_args() {
    let test_url = "fuchsia-pkg://fuchsia.com/elf-test-runner-example-tests#meta/arg_test.cm";
    let mut options = default_options();
    options.arguments = Some(vec!["expected_arg".to_owned()]);
    let events =
        run_test(test_url, options).await.unwrap().into_iter().group_by_test_case_unordered();

    let expected_events = vec![
        RunEvent::suite_started(),
        RunEvent::case_found("main"),
        RunEvent::case_started("main"),
        RunEvent::case_stdout("main", "Got argv[1]=\"expected_arg\""),
        RunEvent::case_stopped("main", CaseStatus::Passed),
        RunEvent::case_finished("main"),
        RunEvent::suite_stopped(SuiteStatus::Passed),
    ]
    .into_iter()
    .group_by_test_case_unordered();
    assert_eq!(expected_events, events);
}

#[fuchsia_async::run_singlethreaded(test)]
async fn launch_and_run_test_with_environ() {
    let test_url = "fuchsia-pkg://fuchsia.com/elf-test-runner-example-tests#meta/environ_test.cm";
    let events = run_test(test_url, default_options()).await.unwrap();

    let expected_events = vec![
        RunEvent::suite_started(),
        RunEvent::case_found("main"),
        RunEvent::case_started("main"),
        RunEvent::case_stopped("main", CaseStatus::Passed),
        RunEvent::case_finished("main"),
        RunEvent::suite_stopped(SuiteStatus::Passed),
    ];
    assert_eq!(expected_events, events);
}

#[fuchsia_async::run_singlethreaded(test)]
async fn launch_and_run_ambient_exec_test_without_ambient_exec_should_fail() {
    // Ambient exec test should fail under elf-test-runner
    let test_url =
        "fuchsia-pkg://fuchsia.com/elf-test-runner-example-tests#meta/ambient_exec_test_fail.cm";

    let events = run_test(test_url, default_options()).await.unwrap();

    let expected_events = vec![
        RunEvent::suite_started(),
        RunEvent::case_found("main"),
        RunEvent::case_started("main"),
        RunEvent::case_stopped("main", CaseStatus::Failed),
        RunEvent::case_finished("main"),
        RunEvent::suite_stopped(SuiteStatus::Failed),
    ];
    assert_eq!(events, expected_events);
}

#[fuchsia_async::run_singlethreaded(test)]
async fn launch_and_run_ambient_exec_test_with_ambient_exec_runner_should_succeed() {
    let test_url =
        "fuchsia-pkg://fuchsia.com/elf-test-runner-example-tests#meta/ambient_exec_test.cm";

    let events = run_test(test_url, default_options()).await.unwrap();

    let expected_events = vec![
        RunEvent::suite_started(),
        RunEvent::case_found("main"),
        RunEvent::case_started("main"),
        RunEvent::case_stopped("main", CaseStatus::Passed),
        RunEvent::case_finished("main"),
        RunEvent::suite_stopped(SuiteStatus::Passed),
    ];
    assert_eq!(events, expected_events);
}
