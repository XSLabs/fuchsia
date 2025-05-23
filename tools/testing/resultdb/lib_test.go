// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package resultdb

import (
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"testing"
	"time"

	resultpb "go.chromium.org/luci/resultdb/proto/v1"

	"github.com/google/go-cmp/cmp"
	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/integration/testsharder/metadata"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

func TestParseSummary(t *testing.T) {
	const testCount = 10
	summary := createTestSummary(testCount)
	testResults, _ := SummaryToResultSink(summary, []*resultpb.StringPair{}, "")
	if len(testResults) != testCount {
		t.Errorf(
			"Parsed incorrect number of resultdb tests in TestSummary, got %d, want %d",
			len(testResults), testCount)
	}
	requests := createTestResultsRequests(testResults, testCount)
	if len(requests) != 1 {
		t.Errorf(
			"Grouped incorrect chunks of ResultDB sink requests, got %d, want 1",
			len(requests))
	}
	if len(requests[0].TestResults) != testCount {
		t.Errorf(
			"Incorrect number of TestResult in the first chunk, got %d, want %d",
			len(requests[0].TestResults), testCount)
	}
	if requests[0].TestResults[0].TestId != "test_0" {
		t.Errorf("Incorrect TestId parsed for first suite. got %s, want test_0", requests[0].TestResults[0].TestId)
	}
}

func checkTagValue(t *testing.T, tags map[string]string, key, want string) {
	if got, ok := tags[key]; !ok {
		t.Errorf("Did not find %q in tags", key)
	} else if got != want {
		t.Errorf("Wrong value for tag %q: got %q, wanted %q", key, got, want)
	}
}

func TestSetTestDetailsToResultSink(t *testing.T) {
	outputRoot := t.TempDir()
	detail := createTestDetailWithPassedAndFailedTestCase(5, 2, outputRoot)
	// include 7 owners to test truncation of owner list
	detail.Metadata = metadata.TestMetadata{
		Owners: []string{
			"testgoogler1@google.com",
			"testgoogler2@google.com",
			"testgoogler3@google.com",
			"testgoogler4@google.com",
			"testgoogler5@google.com",
			"testgoogler6@google.com",
			"testgoogler7@google.com",
		},
		ComponentID: 1478143,
	}
	extraTags := []*resultpb.StringPair{
		{Key: "key1", Value: "value1"},
	}
	result, _, err := testDetailsToResultSink(extraTags, detail, outputRoot)
	if err != nil {
		t.Fatalf("Cannot parse test detail. got %s", err)
	}

	expectedTopLevelTestFailureReason := "bar_0: test case failed\nbar_1: test case failed"
	if !(result.Status == resultpb.TestStatus_FAIL && result.FailureReason.PrimaryErrorMessage == expectedTopLevelTestFailureReason) {
		t.Errorf("If a test failed, the top level test should have a failure reason with a list of the failed tests.\n The primary error message is %q.\n The expected failure reason is %q.", result.FailureReason.PrimaryErrorMessage, expectedTopLevelTestFailureReason)
	}

	tags := make(map[string]string)
	for _, tag := range result.Tags {
		tags[tag.Key] = tag.Value
	}

	if len(extraTags) != 1 {
		t.Errorf("extraTags(%v) got mutated, this value should not be changed.", extraTags)
	}
	// We only expect 5 tags
	// 1. key1:value1
	// 2. gn_label:value
	// 3. test_case_count:value
	// 4. affected:value
	// 5. is_top_level_test:value
	// 6. owners:value
	if len(tags) != 6 {
		t.Errorf("tags(%v) contains unexpected values.", tags)
	}

	checkTagValue(t, tags, "key1", "value1")
	checkTagValue(t, tags, "gn_label", detail.GNLabel)
	checkTagValue(t, tags, "test_case_count", "7")
	checkTagValue(t, tags, "affected", "false")
	checkTagValue(t, tags, "is_top_level_test", "true")
	checkTagValue(t, tags, "owners", "testgoogler1@google.com,testgoogler2@google.com,testgoogler3@google.com,testgoogler4@google.com,testgoogler5@google.com")

	if len(result.Artifacts) != 2 {
		t.Errorf("Got %d artifacts, want 2", len(result.Artifacts))
	}
	artifactNames := []string{}
	for name := range result.Artifacts {
		artifactNames = append(artifactNames, name)
	}
	sort.Strings(artifactNames)
	if diff := cmp.Diff(artifactNames, []string{"dir-1/outputfile", "dir_2/outputfile"}); diff != "" {
		t.Errorf("Diff in output files (-got +want):\n%s", diff)
	}
	expectedMetadata := resultpb.TestMetadata{
		Name: detail.Name,
		BugComponent: &resultpb.BugComponent{
			System: &resultpb.BugComponent_IssueTracker{
				IssueTracker: &resultpb.IssueTrackerComponent{
					ComponentId: 1478143,
				},
			},
		},
	}
	if diff := cmp.Diff(expectedMetadata.Name, result.TestMetadata.Name); diff != "" {
		t.Errorf("Diff in metadata name (-got +want):\n%s", diff)
	}
	if diff := cmp.Diff(expectedMetadata.BugComponent.GetIssueTracker().ComponentId, result.TestMetadata.BugComponent.GetIssueTracker().ComponentId); diff != "" {
		t.Errorf("Diff in the bug component's component id (-got +want):\n%s", diff)
	}
}

func TestSetTestDetailsToResultSink_DefaultFailureReason_ExceedsMaxSize(t *testing.T) {
	outputRoot := t.TempDir()
	detail := createTestDetailWithPassedAndFailedTestCase(5, 200, outputRoot)
	extraTags := []*resultpb.StringPair{
		{Key: "key1", Value: "value1"},
	}
	result, _, err := testDetailsToResultSink(extraTags, detail, outputRoot)
	if err != nil {
		t.Fatalf("Cannot parse test detail. got %s", err)
	}

	expectedTopLevelTestFailureReason := "200 test cases failed"
	if !(result.Status == resultpb.TestStatus_FAIL && result.FailureReason.PrimaryErrorMessage == expectedTopLevelTestFailureReason) {
		t.Errorf("If a test failed, the top level test should have a failure reason with a list of the failed tests.\n The primary error message is %q.\n The expected failure reason is %q.", result.FailureReason.PrimaryErrorMessage, expectedTopLevelTestFailureReason)
	}

	tags := make(map[string]string)
	for _, tag := range result.Tags {
		tags[tag.Key] = tag.Value
	}

	if len(extraTags) != 1 {
		t.Errorf("extraTags(%v) got mutated, this value should not be changed.", extraTags)
	}
	// We only expect 5 tags
	// 1. key1:value1
	// 2. gn_label:value
	// 3. test_case_count:value
	// 4. affected:value
	// 5. is_top_level_test:value
	if len(tags) != 5 {
		t.Errorf("tags(%v) contains unexpected values.", tags)
	}

	checkTagValue(t, tags, "key1", "value1")
	checkTagValue(t, tags, "gn_label", detail.GNLabel)
	checkTagValue(t, tags, "test_case_count", "205")
	checkTagValue(t, tags, "affected", "false")
	checkTagValue(t, tags, "is_top_level_test", "true")

	if len(result.Artifacts) != 2 {
		t.Errorf("Got %d artifacts, want 2", len(result.Artifacts))
	}
	artifactNames := []string{}
	for name := range result.Artifacts {
		artifactNames = append(artifactNames, name)
	}
	sort.Strings(artifactNames)
	if diff := cmp.Diff(artifactNames, []string{"dir-1/outputfile", "dir_2/outputfile"}); diff != "" {
		t.Errorf("Diff in output files (-got +want):\n%s", diff)
	}
}

func TestSetTestCaseToResultSink(t *testing.T) {
	outputRoot := t.TempDir()
	detail := createTestDetailWithTestCase(5, outputRoot)
	results, _ := testCaseToResultSink(detail.Cases, []*resultpb.StringPair{}, detail, outputRoot)
	if len(results) != 5 {
		t.Errorf("Got %d test case results, want 5", len(results))
	}

	for i, result := range results {
		tags := make(map[string]string)
		for _, tag := range result.Tags {
			tags[tag.Key] = tag.Value
		}
		// We only expect 3 tags
		// 1. format:value
		// 2. is_test_case:value
		// 3. key1:value1
		if len(tags) != 3 {
			t.Errorf("tags(%v) contains unexpected values.", tags)
		}
		checkTagValue(t, tags, "format", detail.Cases[i].Format)
		checkTagValue(t, tags, "is_test_case", "true")
		checkTagValue(t, tags, "key1", "value1")
		if len(result.Artifacts) != 2 {
			t.Errorf("Got %d artifacts for test case %d, want 2", len(result.Artifacts), i+1)
		}
		artifactNames := []string{}
		for name := range result.Artifacts {
			artifactNames = append(artifactNames, name)
		}
		sort.Strings(artifactNames)
		if diff := cmp.Diff(artifactNames, []string{"case/outputfile1", "case/outputfile2"}); diff != "" {
			t.Errorf("Diff in output files (-got +want):\n%s", diff)
		}
	}
}

func createTestSummary(testCount int) *runtests.TestSummary {
	t := []runtests.TestDetails{}
	for i := 0; i < testCount; i++ {
		t = append(t, runtests.TestDetails{
			Name:                 fmt.Sprintf("test_%d", i),
			GNLabel:              "some label",
			OutputFiles:          []string{"some file path"},
			Result:               runtests.TestSuccess,
			StartTime:            time.Now(),
			DurationMillis:       39797,
			IsTestingFailureMode: false,
		})
	}
	return &runtests.TestSummary{Tests: t}
}

func createTestDetailWithTestCase(testCase int, outputRoot string) *runtests.TestDetails {
	t := []runtests.TestCaseResult{}
	if outputRoot != "" {
		for _, f := range []string{"foo/dir-1/outputfile", "foo/dir#2/outputfile", "foo/case/outputfile1", "foo/case/outputfile2"} {
			outputfile := filepath.Join(outputRoot, f)
			os.MkdirAll(filepath.Dir(outputfile), os.ModePerm)
			os.WriteFile(outputfile, []byte("output"), os.ModePerm)
		}
	}
	for i := 0; i < testCase; i++ {
		t = append(t, runtests.TestCaseResult{
			DisplayName: fmt.Sprintf("foo/bar_%d", i),
			SuiteName:   "foo",
			CaseName:    fmt.Sprintf("bar_%d", i),
			Status:      runtests.TestSuccess,
			Format:      "Rust",
			OutputFiles: []string{"case/outputfile1", "case/outputfile2"},
			Tags:        []build.TestTag{{"key1", "value1"}},
		})
	}
	return &runtests.TestDetails{
		Name:                 "foo",
		GNLabel:              "some label",
		OutputFiles:          []string{"dir-1/outputfile", "dir#2/outputfile"},
		OutputDir:            "foo",
		Result:               runtests.TestSuccess,
		StartTime:            time.Now(),
		DurationMillis:       39797,
		IsTestingFailureMode: false,
		Cases:                t,
	}
}

func createTestDetailWithPassedAndFailedTestCase(passedTestCase int, failedTestCase int, outputRoot string) *runtests.TestDetails {
	t := []runtests.TestCaseResult{}
	if outputRoot != "" {
		for _, f := range []string{"dir-1/outputfile", "dir#2/outputfile", "case/outputfile1", "case/outputfile2"} {
			outputfile := filepath.Join(outputRoot, f)
			os.MkdirAll(filepath.Dir(outputfile), os.ModePerm)
			os.WriteFile(outputfile, []byte("output"), os.ModePerm)
		}
	}
	for i := 0; i < passedTestCase; i++ {
		t = append(t, runtests.TestCaseResult{
			DisplayName: fmt.Sprintf("foo/bar_%d", i),
			SuiteName:   "foo",
			CaseName:    fmt.Sprintf("bar_%d", i),
			Status:      runtests.TestSuccess,
			Format:      "Rust",
			OutputFiles: []string{"case/outputfile1", "case/outputfile2"},
			Tags:        []build.TestTag{{"key1", "value1"}},
		})
	}
	for i := 0; i < failedTestCase; i++ {
		t = append(t, runtests.TestCaseResult{
			DisplayName: fmt.Sprintf("foo/bar_%d", i),
			SuiteName:   "foo",
			CaseName:    fmt.Sprintf("bar_%d", i),
			Status:      runtests.TestFailure,
			Format:      "Rust",
			OutputFiles: []string{"case/outputfile1", "case/outputfile2"},
			Tags:        []build.TestTag{{"key1", "value1"}},
		})
	}
	finalResult := runtests.TestSuccess
	if failedTestCase > 0 {
		finalResult = runtests.TestFailure
	}
	return &runtests.TestDetails{
		Name:                 "foo",
		GNLabel:              "some label",
		OutputFiles:          []string{"dir-1/outputfile", "dir#2/outputfile"},
		Result:               finalResult,
		StartTime:            time.Now(),
		DurationMillis:       39797,
		IsTestingFailureMode: false,
		Cases:                t,
	}
}

func TestIsReadable(t *testing.T) {
	if r := isReadable(""); r {
		t.Errorf("Empty string cannot be readable. got %t, want false", r)
	}
	if r := isReadable(*testDataDir); r {
		t.Errorf("Directory should not be readable. got %t, want false", r)
	}
	luciCtx := filepath.Join(*testDataDir, "lucictx.json")
	if r := isReadable(luciCtx); !r {
		t.Errorf("File %v should be readable. got %t, want true", luciCtx, r)
	}
}

func TestInvocationLevelArtifacts(t *testing.T) {
	invocationLogs := []string{"syslog.txt", "serial_log.txt", "nonexistent_log.txt"}
	artifacts := InvocationLevelArtifacts(*testDataDir, invocationLogs)
	foundSyslog := false
	foundSerial := false
	for logName := range artifacts {
		switch logName {
		case "syslog.txt":
			foundSyslog = true
		case "serial_log.txt":
			foundSerial = true
		default:
			t.Errorf("Found unexpected log (%s), expect only syslog.txt or serial_log.txt", logName)
		}
	}
	if !foundSyslog {
		t.Errorf("Did not find syslog.txt in output")
	}
	if !foundSerial {
		t.Errorf("Did not find serial_log.txt in output")
	}
}

func TestDetermineExpected(t *testing.T) {
	testCases := []struct {
		testStatus     resultpb.TestStatus
		testCaseStatus resultpb.TestStatus
		expected       bool
	}{
		{
			// test passed, test case result is ignored.
			testStatus:     resultpb.TestStatus_PASS,
			testCaseStatus: resultpb.TestStatus_FAIL,
			expected:       true,
		},
		{
			// test failed and has test case status,
			// report on test case result.
			testStatus:     resultpb.TestStatus_FAIL,
			testCaseStatus: resultpb.TestStatus_PASS,
			expected:       true,
		},
		{
			// test failed and no test case status,
			// report test result.
			testStatus:     resultpb.TestStatus_FAIL,
			testCaseStatus: resultpb.TestStatus_STATUS_UNSPECIFIED,
			expected:       false,
		},
		{
			// cannot determine test status,
			// report on test cast result.
			testStatus:     resultpb.TestStatus_STATUS_UNSPECIFIED,
			testCaseStatus: resultpb.TestStatus_PASS,
			expected:       true,
		},
		{
			// cannot determine both test and test case result
			testStatus:     resultpb.TestStatus_STATUS_UNSPECIFIED,
			testCaseStatus: resultpb.TestStatus_STATUS_UNSPECIFIED,
			expected:       false,
		},
		{
			testStatus:     resultpb.TestStatus_PASS,
			testCaseStatus: resultpb.TestStatus_PASS,
			expected:       true,
		},
		{
			testStatus:     resultpb.TestStatus_FAIL,
			testCaseStatus: resultpb.TestStatus_FAIL,
			expected:       false,
		},
	}
	for _, tc := range testCases {
		r := determineExpected(tc.testStatus, tc.testCaseStatus)
		if r != tc.expected {
			t.Errorf("TestDetermineExpected failed:\ntestSuite Status: %v, testCase Status: %v, got %t, want %t",
				tc.testStatus, tc.testCaseStatus, r, tc.expected)
		}
	}
}

func TestTruncateString(t *testing.T) {
	testCases := []struct {
		testStr string
		want    string
		limit   int // bytes
	}{
		{
			testStr: "ab£cdefg",
			want:    "",
			limit:   1,
		}, {
			testStr: "ab£cdefg",
			want:    "ab...",
			limit:   5,
		}, {
			testStr: "ab£cdefg",
			want:    "ab...",
			limit:   6,
		}, {
			testStr: "ab£cdefg",
			want:    "ab£...",
			limit:   7,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "",
			limit:   3,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "",
			limit:   4,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "",
			limit:   5,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "♥...",
			limit:   6,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "♥L...",
			limit:   7,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "♥LoveFuc...",
			limit:   13,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "♥LoveFuchsia",
			limit:   14,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "♥LoveFuchsia",
			limit:   100,
		},
	}
	for _, tc := range testCases {
		r := truncateString(tc.testStr, tc.limit)
		if r != tc.want {
			t.Errorf("TestTruncateString failed for input: %q(%d), got %q, want %q",
				tc.testStr, tc.limit, r, tc.want)
		}
	}
}
