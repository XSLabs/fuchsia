// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package expectation

import "go.fuchsia.dev/fuchsia/src/connectivity/network/testing/conformance/expectation/outcome"

var dhcpServerExpectations map[AnvlCaseNumber]outcome.Outcome = map[AnvlCaseNumber]outcome.Outcome{
	{1, 1}:  Pass,
	{1, 2}:  Pass,
	{2, 1}:  Pass,
	{2, 2}:  Pass,
	{2, 3}:  Pass,
	{2, 4}:  Pass,
	{2, 5}:  Pass,
	{2, 6}:  Pass,
	{3, 1}:  Pass,
	{4, 1}:  Pass,
	{4, 2}:  Pass,
	{4, 3}:  Pass,
	{4, 4}:  Pass,
	{5, 1}:  Pass,
	{5, 2}:  Pass,
	{5, 3}:  Pass,
	{5, 4}:  Pass,
	{5, 5}:  Inconclusive,
	{6, 1}:  Pass,
	{6, 2}:  Fail,
	{6, 3}:  Pass,
	{6, 4}:  Pass,
	{7, 1}:  Pass,
	{8, 1}:  Pass,
	{8, 2}:  Pass,
	{8, 3}:  Pass,
	{8, 4}:  Pass,
	{8, 5}:  Pass,
	{8, 6}:  Pass,
	{9, 1}:  Pass,
	{9, 2}:  Pass,
	{10, 1}: Pass,
	{10, 2}: Inconclusive,
	// TODO(https://fxbug.dev/42056692): Address the ANVL crash.
	{10, 3}:  Skip,
	{10, 4}:  Pass,
	{10, 5}:  Pass,
	{10, 6}:  Pass,
	{10, 7}:  Pass,
	{10, 8}:  Pass,
	{10, 9}:  Pass,
	{10, 10}: Pass,
	{10, 11}: Pass,
	{10, 12}: Pass,
	{10, 13}: Pass,
	{10, 14}: Pass,
	{10, 15}: Pass,
	{10, 16}: Pass,
	{10, 17}: Pass,
	{10, 18}: Pass,
	{11, 1}:  Fail,
	{11, 2}:  Pass,
	{12, 1}:  Pass,
	{12, 2}:  Pass,
	{12, 3}:  Pass,
	{12, 4}:  Pass,
	{12, 5}:  Fail,
	{12, 6}:  Pass,
	{12, 7}:  Pass,
	{12, 8}:  Fail,
	{12, 9}:  Pass,
	{12, 10}: Fail,
	{12, 11}: Pass,
	{12, 12}: Fail,
	{12, 13}: Pass,
	{13, 1}:  Fail,
	{13, 2}:  Pass,
	{13, 3}:  Inconclusive,
	{13, 4}:  Pass,
	{14, 1}:  Pass,
	{15, 1}:  Pass,
	{15, 2}:  Pass,
	{16, 1}:  Pass,
	{16, 2}:  Pass,
	{16, 3}:  Pass,
}

// No difference from netstack2!
var dhcpServerExpectationsNS3 map[AnvlCaseNumber]outcome.Outcome = dhcpServerExpectations
