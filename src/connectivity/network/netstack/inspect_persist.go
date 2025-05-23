// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package netstack

import (
	"context"
	"fmt"
	"time"

	"go.fuchsia.dev/fuchsia/src/lib/component"
	syslog "go.fuchsia.dev/fuchsia/src/lib/syslog/go"

	"fidl/fuchsia/diagnostics/persist"

	"gvisor.dev/gvisor/pkg/tcpip"
)

const (
	persistenceTagName = "persist"

	// The amount of time to wait after requesting persistence before doing so
	// again.
	persistPeriod = 12 * time.Minute
)

var tags = []string{
	"counters",
	"fidl",
	"memstats",
	"nics",
	"routes",
	"runtime",
	"sockets",
}

// startPersistClient attempts to start the persistence polling client. Fatal
// startup errors will cause this function to panic; any other failures will be
// logged as warnings.
func startPersistClient(ctx context.Context, componentCtx *component.Context, clock tcpip.Clock) {
	_ = syslog.InfoTf(persistenceTagName, "starting persistence client")

	persistServerEnd, persistClientEnd, err := persist.NewDataPersistenceWithCtxInterfaceRequest()
	if err != nil {
		_ = syslog.FatalTf(persistenceTagName, "failed to create channel: %s", err)
	}
	if err := componentCtx.ConnectToProtocolAtPath(
		"fuchsia.diagnostics.persist.DataPersistence", persistServerEnd); err != nil {
		_ = syslog.WarnTf(persistenceTagName, "couldn't connect to persistence service: %s", err)
		return
	}
	runPersistClient(persistClientEnd, ctx, clock)
}

func runPersistClient(persistClientEnd *persist.DataPersistenceWithCtxInterface, ctx context.Context, clock tcpip.Clock) {
	_ = syslog.InfoTf(persistenceTagName, "starting persistence polling routine")
	// Request immediately, then wait for the timer.
	if err := makePersistenceRequest(persistClientEnd, ctx); err != nil {
		_ = syslog.WarnTf(persistenceTagName, "aborting persistence routine startup: %s", err)
		return
	}

	var timer tcpip.Timer
	timer = clock.AfterFunc(persistPeriod, func() {
		select {
		case <-ctx.Done():
			_ = syslog.InfoTf(persistenceTagName, "stopping persistence polling routine")
		default:
			if err := makePersistenceRequest(persistClientEnd, ctx); err != nil {
				_ = syslog.WarnTf(persistenceTagName, "stopping persistence routine: %s", err)
				return
			}
			timer.Reset(persistPeriod)
		}
	})
}

func makePersistenceRequest(persistClientEnd *persist.DataPersistenceWithCtxInterface, ctx context.Context) error {
	_ = syslog.DebugTf(persistenceTagName, "requesting persistence for netstack")
	results, err := persistClientEnd.PersistTags(ctx, tags)
	if err != nil {
		return fmt.Errorf("failed to request persistence: %w", err)
	}
	for i, got := range results {
		if want := persist.PersistResultQueued; got != want {
			_ = syslog.WarnTf(persistenceTagName, "unexpected persist result for %s; want %s got %s", tags[i], want, got)
		}
	}
	return nil
}
