// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"go.fuchsia.dev/fuchsia/tools/botanist"
	"go.fuchsia.dev/fuchsia/tools/botanist/constants"
	"go.fuchsia.dev/fuchsia/tools/botanist/targets"
	"go.fuchsia.dev/fuchsia/tools/lib/environment"
	"go.fuchsia.dev/fuchsia/tools/lib/ffxutil"
	"go.fuchsia.dev/fuchsia/tools/lib/flagmisc"
	"go.fuchsia.dev/fuchsia/tools/lib/logger"
	"go.fuchsia.dev/fuchsia/tools/lib/osmisc"
	"go.fuchsia.dev/fuchsia/tools/lib/serial"
	"go.fuchsia.dev/fuchsia/tools/lib/subprocess"
	"go.fuchsia.dev/fuchsia/tools/lib/syslog"
	"go.fuchsia.dev/fuchsia/tools/testing/testrunner"
	testrunnerconstants "go.fuchsia.dev/fuchsia/tools/testing/testrunner/constants"

	"github.com/google/subcommands"
	"golang.org/x/sync/errgroup"
)

// RunCommand is a Command implementation for booting a device and running a
// given command locally.
type RunCommand struct {
	// ConfigFile is the path to the target configurations.
	configFile string

	// DownloadManifest is the path we should write the package server's
	// download manifest to.
	downloadManifest string

	// ImageManifest is a path to an image manifest.
	imageManifest string

	// ProductBundle is a path to product_bundles.json file.
	productBundles string

	// ProductBundleName is a name of product bundle getting used.
	productBundleName string

	// IsBootTest tells whether the product bundle provided is for a boot test.
	isBootTest bool

	// Netboot tells botanist to netboot (and not to pave).
	netboot bool

	// ZirconArgs are kernel command-line arguments to pass on boot.
	zirconArgs flagmisc.StringsValue

	// Timeout is the duration allowed for the command to finish execution.
	timeout time.Duration

	// syslogDir, if nonempty, is the directory in which system syslogs will be written.
	syslogDir string

	// serialLogDir, if nonempty, is the directory in which system serial logs will be written.
	serialLogDir string

	// RepoURL specifies the URL of a package repository.
	repoURL string

	// BlobURL optionally specifies the URL of where a package repository's blobs may be served from.
	// Defaults to $repoURL/blobs.
	blobURL string

	// localRepo specifies the path to a local package repository. If set,
	// botanist will spin up a package server to serve packages from this
	// repository.
	localRepo string

	// The path to the ffx tool.
	ffxPath string

	// Experiments to enable. Supported experiments can be found at //tools/botanist/common.go.
	experiments flagmisc.StringsValue

	// When true skips setting up the targets.
	skipSetup bool

	// Args passed to testrunner
	testrunnerOptions testrunner.Options

	// The timeout to wait for an SSH connection after booting the target.
	bootupTimeout time.Duration

	// Whether the product bundle is expected to support SSH.
	expectsSSH bool

	// The scale factor to multiply test timeouts by. This may be set if the bot environment
	// is known to be slower than usual.
	testTimeoutScaleFactor int
}

func (*RunCommand) Name() string {
	return "run"
}

func (*RunCommand) Usage() string {
	return `
botanist run [flags...] tests-file

flags:
`
}

func (*RunCommand) Synopsis() string {
	return fmt.Sprintf("boots a device and executes all tests found in the JSON [tests-file].")
}

func (r *RunCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&r.configFile, "config", "", "path to file of device config")
	f.StringVar(&r.imageManifest, "images", "", "path to an image manifest")
	f.StringVar(&r.productBundles, "product-bundles", "", "path to product_bundles.json file")
	f.StringVar(&r.productBundleName, "product-bundle-name", "", "name of product bundle to use")
	f.BoolVar(&r.isBootTest, "boot-test", false, "whether the provided product bundle is for a boot test.")
	f.BoolVar(&r.netboot, "netboot", false, "if set, botanist will not pave; but will netboot instead")
	f.Var(&r.zirconArgs, "zircon-args", "kernel command-line arguments")
	f.DurationVar(&r.timeout, "timeout", 0, "duration allowed for the command to finish execution, a value of 0 (zero) will not impose a timeout.")
	f.StringVar(&r.syslogDir, "syslog-dir", "", "the directory to write all system logs to.")
	f.StringVar(&r.serialLogDir, "serial-log-dir", "", "the directory to write all serial logs to.")
	// TODO(https://fxbug.dev/407117303): Remove `repo` and `blobs` flag after recipes no longer sets it.
	f.StringVar(&r.repoURL, "repo", "", "URL at which to configure a package repository; if the placeholder of \"localhost\" will be resolved and scoped as appropriate")
	f.StringVar(&r.blobURL, "blobs", "", "URL at which to serve a package repository's blobs; if the placeholder of \"localhost\" will be resolved and scoped as appropriate")
	f.StringVar(&r.localRepo, "local-repo", "", "path to a local package repository; the repo and blobs flags are ignored when this is set")
	f.StringVar(&r.ffxPath, "ffx", "", "Path to the ffx tool.")
	f.StringVar(&r.downloadManifest, "download-manifest", "", "Path to a manifest containing all package server downloads")
	f.Var(&r.experiments, "experiment", fmt.Sprintf("The name of an experiment to enable. Supported experiments are: %v.", botanist.SupportedExperiments))
	f.BoolVar(&r.skipSetup, "skip-setup", false, "if set, botanist will not set up a target.")
	f.DurationVar(&r.bootupTimeout, "bootup-timeout", 0, "duration allowed for the command to finish execution, a value of 0 (zero) will fall back to the default.")
	f.BoolVar(&r.expectsSSH, "expects-ssh", false, "if set, botanist will try to establish an SSH connection before running tests.")
	f.IntVar(&r.testTimeoutScaleFactor, "test-timeout-scale-factor", 1, "Factor to scale test timeouts by (used for slow bot environments)")

	// Parsing of testrunner options.
	f.StringVar(&r.testrunnerOptions.OutDir, "out-dir", "", "Optional path where a directory containing test results should be created.")
	f.StringVar(&r.testrunnerOptions.NsjailPath, "nsjail", "", "Optional path to an NsJail binary to use for linux host test sandboxing.")
	f.StringVar(&r.testrunnerOptions.NsjailRoot, "nsjail-root", "", "Path to the directory to use as the NsJail root directory")
	f.StringVar(&r.testrunnerOptions.LocalWD, "C", "", "Working directory of local testing subprocesses; if unset the current working directory will be used.")
	f.StringVar(&r.testrunnerOptions.SnapshotFile, "snapshot-output", "", "The output filename for the snapshot. This will be created in the output directory.")
	f.BoolVar(&r.testrunnerOptions.UseSerial, "use-serial", false, "Use serial to run tests on the target.")
	f.StringVar(&r.testrunnerOptions.LLVMProfdataPath, "llvm-profdata", "", "Optional path to a llvm-profdata binary to use for merging profiles on the host in between tests.")
}

func setupFFXDaemon(ctx context.Context, ffx *ffxutil.FFXInstance) (*ffxutil.FFXInstance, func(), error) {
	var cleanup func()
	ffxOutputsDir := filepath.Join(os.Getenv(testrunnerconstants.TestOutDirEnvKey), "ffx_outputs")
	daemonLog, err := osmisc.CreateFile(filepath.Join(ffxOutputsDir, "daemon.log"))
	if err != nil {
		return ffx, cleanup, err
	}
	cmd := ffx.StartDaemon(ctx, daemonLog)
	if err := cmd.Start(); err != nil {
		return ffx, cleanup, err
	}
	// Use a new context so that the subprocess can only be terminated by
	// a direct call to the cancel function.
	daemonCtx, daemonCancel := context.WithCancel(context.Background())

	// Wait for the daemon process to terminate in a separate goroutine
	// and log when it finishes in order to detect if the process gets
	// terminated earlier than expected.
	cmdWait := make(chan error)
	go func() {
		// Using subprocess.WaitForCmd() instead of cmd.Wait() ensures that
		// the function returns when the context is done.
		if err := subprocess.WaitForCmd(daemonCtx, cmd); err != nil {
			logger.Errorf(ctx, "daemon process finished with err: %s", err)
		} else {
			logger.Debugf(ctx, "ffx daemon process finished")
		}
		cmdWait <- err
	}()
	cleanup = func() {
		// TODO(https://fxbug.dev/42071857): Clean up daemon by sending a SIGTERM to the
		// process once that is supported.
		if err := ffx.Stop(); err != nil {
			logger.Errorf(ctx, "failed to stop ffx daemon: %s", err)
		}

		// Wait for the daemon process to finish before closing the log.
		daemonCancel()
		<-cmdWait
		if err := daemonLog.Close(); err != nil {
			logger.Errorf(ctx, "failed to close ffx daemon log: %s", err)
		}
	}
	return ffx, cleanup, ffx.WaitForDaemon(ctx)
}

// This returns an `ffx` instance, a cleanup function (dispatched via `defer`), and an error.
func (r *RunCommand) setupFFX(ctx context.Context, invokeMode ffxutil.FFXInvokeMode) (*ffxutil.FFXInstance, func(), error) {
	if r.ffxPath == "" {
		return nil, nil, fmt.Errorf("ffx path must be provided with the -ffx flag.")
	}
	ffxOutputsDir := filepath.Join(os.Getenv(testrunnerconstants.TestOutDirEnvKey), "ffx_outputs")

	extraConfigs := ffxutil.ConfigSettings{
		Level: "global",
		Settings: map[string]any{
			"daemon.autostart":              false,
			"discovery.mdns.enabled":        false,
			"ffx.target-list.local-connect": true,
		},
	}
	// By default, the ssh.priv and ssh.pub values are in $HOME, which had earlier been configured to be a tmpdir.
	// But in case we're in strict mode, let's be explicit about the path. If there is no pub key, when we will
	// let the FFXInstance specify the default
	sshPriv := filepath.Join(os.Getenv("HOME"), ".ssh", "fuchsia_ed25519")
	sshKeys := ffxutil.SSHInfo{SshPriv: sshPriv}
	ffx, err := ffxutil.NewFFXInstance(ctx, r.ffxPath, "", []string{}, "", &sshKeys, ffxOutputsDir, invokeMode, extraConfigs)
	if err != nil {
		return nil, nil, err
	}
	stdout, stderr, flush := botanist.NewStdioWriters(ctx, "ffx")
	defer flush()
	ffx.SetStdoutStderr(stdout, stderr)
	if err := ffx.ConfigEnv(ctx); err != nil {
		return ffx, nil, err
	}
	if invokeMode == ffxutil.UseFFXStrict {
		// It should not be necessary to start the daemon when running --strict. Generally this
		// shouldn't be this file's responsibility anyway, but it's the next best thing.
		return ffx, nil, nil
	} else {
		return setupFFXDaemon(ctx, ffx)
	}
}

func (r *RunCommand) setupSerialLog(ctx context.Context, eg *errgroup.Group, fuchsiaTargets []targets.FuchsiaTarget) error {
	if r.serialLogDir == "" {
		return nil
	}

	if err := os.Mkdir(r.serialLogDir, os.ModePerm); err != nil {
		return err
	}

	for _, t := range fuchsiaTargets {
		t := t
		eg.Go(func() error {
			logger.Debugf(ctx, "starting serial collection for target %s", t.Nodename())

			// Create a new file to capture the serial log for this nodename.
			serialLogName := fmt.Sprintf("%s_serial_log.txt", t.Nodename())
			// TODO(https://fxbug.dev/42150891): Remove once there are no dependencies on this filename.
			if len(fuchsiaTargets) == 1 {
				serialLogName = "serial_log.txt"
			}
			serialLogPath := filepath.Join(r.serialLogDir, serialLogName)
			absPath, err := filepath.Abs(serialLogPath)
			if err != nil {
				return fmt.Errorf("failed to get abspath of serial log: %w", err)
			}
			if err := os.Setenv(constants.SerialLogEnvKey, absPath); err != nil {
				logger.Debugf(ctx, "failed to set %s to %s", constants.SerialLogEnvKey, absPath)
			}

			// Start capturing the serial log for this target.
			if err := t.CaptureSerialLog(serialLogPath); err != nil && ctx.Err() == nil {
				return err
			}
			return nil
		})
	}
	return nil
}

func (r *RunCommand) setupPackageServer(ctx context.Context) (*botanist.PackageServer, error) {
	if r.localRepo == "" {
		return nil, nil
	}

	var port int
	pkgSrvPort := os.Getenv(constants.PkgSrvPortKey)
	if pkgSrvPort == "" {
		logger.Warningf(ctx, "%s is empty, using default port %d", constants.PkgSrvPortKey, botanist.DefaultPkgSrvPort)
		port = botanist.DefaultPkgSrvPort
	} else {
		var err error
		port, err = strconv.Atoi(pkgSrvPort)
		if err != nil {
			return nil, err
		}
	}

	pkgSrv, err := botanist.NewPackageServer(ctx, r.localRepo, port)
	if err != nil {
		return pkgSrv, err
	}
	return pkgSrv, nil
}

func (r *RunCommand) dispatchTests(ctx context.Context, cancel context.CancelFunc, eg *errgroup.Group, baseTargets []targets.Base, fuchsiaTargets []targets.FuchsiaTarget, primaryTarget targets.FuchsiaTarget, pkgSrv *botanist.PackageServer, testsPath string) {
	// Log any failures after running tests.
	for _, t := range fuchsiaTargets {
		t := t
		eg.Go(func() error {
			if err := t.Wait(ctx); err != nil && err != targets.ErrUnimplemented && ctx.Err() == nil {
				return fmt.Errorf("target %s failed: %w", t.Nodename(), err)
			}
			return nil
		})
	}

	// Dispatch tests.
	eg.Go(func() error {
		// Signal other goroutines to exit when tests complete.
		defer cancel()

		if r.productBundles == "" {
			return fmt.Errorf("-product-bundles is required")
		}
		if r.productBundleName == "" {
			return fmt.Errorf("-product-bundle-name is required")
		}
		startOpts := targets.StartOptions{
			Netboot:           r.netboot,
			ImageManifest:     r.imageManifest,
			ZirconArgs:        r.zirconArgs,
			ProductBundles:    r.productBundles,
			ProductBundleName: r.productBundleName,
			IsBootTest:        r.isBootTest,
			BootupTimeout:     r.bootupTimeout,
		}

		if err := targets.StartTargets(ctx, startOpts, fuchsiaTargets); err != nil {
			return fmt.Errorf("%s: %w", constants.FailedToStartTargetMsg, err)
		}
		logger.Debugf(ctx, "successfully started all targets")

		defer func() {
			ctx, cancel := context.WithTimeout(context.Background(), time.Minute)
			defer cancel()
			targets.StopTargets(ctx, fuchsiaTargets)
		}()

		// Create a testbed config file. We have to do this after starting the
		// targets so that we can get their IP addresses.
		testbedConfig, err := r.createTestbedConfig(baseTargets)
		if err != nil {
			return err
		}
		defer os.Remove(testbedConfig)

		if r.expectsSSH {
			for _, t := range fuchsiaTargets {
				t := t
				client, err := t.SSHClient()
				if err != nil {
					if err := r.dumpSyslogOverSerial(ctx, t.SerialSocketPath()); err != nil {
						logger.Errorf(ctx, err.Error())
					}
					return err
				}
				if pkgSrv != nil {
					if err := t.AddPackageRepository(client, pkgSrv.RepoURL, pkgSrv.BlobURL); err != nil {
						return err
					}
					logger.Debugf(ctx, "added package repo to target %s", t.Nodename())
				}
				addr, err := targets.IPAddr(t)
				if err != nil {
					return err
				}
				t.GetFFX().SetTarget(addr.String())
				if r.syslogDir != "" {
					if _, err := os.Stat(r.syslogDir); errors.Is(err, os.ErrNotExist) {
						if err := os.Mkdir(r.syslogDir, os.ModePerm); err != nil {
							return err
						}
					}
					defer t.StopSyslog()
					go func() {
						syslogName := fmt.Sprintf("%s_syslog.txt", t.Nodename())
						// TODO(https://fxbug.dev/42150891): Remove when there are no dependencies on this filename.
						if len(fuchsiaTargets) == 1 {
							syslogName = "syslog.txt"
						}
						syslogPath := filepath.Join(r.syslogDir, syslogName)
						if err := t.CaptureSyslog(client, syslogPath, pkgSrv); err != nil && ctx.Err() == nil {
							logger.Errorf(ctx, "%s at %s: %s", constants.FailedToCaptureSyslogMsg, syslogPath, err)
						}
					}()
				}
			}
		}
		err = r.runAgainstTarget(ctx, primaryTarget, testsPath, testbedConfig)
		// Cancel ctx to notify other goroutines that this routine has completed.
		// If another goroutine gets an error and the context is canceled, it
		// should return nil so that we always prioritize the result from this
		// goroutine.
		cancel()
		return err
	})
}

func (r *RunCommand) execute(ctx context.Context, args []string) error {
	ctx, cancel := context.WithCancel(ctx)
	if r.timeout != 0 {
		ctx, cancel = context.WithTimeout(ctx, r.timeout)
	}

	go func() {
		<-ctx.Done()
		// Log the timeout for tefmocheck to detect it.
		if ctx.Err() == context.DeadlineExceeded {
			logger.Errorf(ctx, "%s (%s)", constants.CommandExceededTimeoutMsg, r.timeout)
		}
	}()
	defer cancel()

	testsPath := args[0]

	if r.skipSetup {
		if err := testrunner.SetupAndExecute(ctx, r.testrunnerOptions, testsPath); err != nil {
			return fmt.Errorf("testrunner with flags: %v, with timeout: %s, failed: %w", r.testrunnerOptions, r.timeout, err)
		}
		return nil
	}

	experiments := botanist.GetExperiments(r.experiments)
	invokeMode := ffxutil.UseFFXLegacy
	if experiments.Contains(botanist.UseFFXStrict) {
		invokeMode = ffxutil.UseFFXStrict
	}
	ffx, cleanup, err := r.setupFFX(ctx, invokeMode)
	if cleanup != nil {
		defer cleanup()
	}
	if err != nil {
		return err
	}
	sshKey := ffx.GetSshPrivateKey()
	authorizedKey := ffx.GetSshAuthorizedKeys()

	// Parse targets out from the target configuration file.
	baseTargets, fuchsiaTargets, err := r.deriveTargetsFromFile(ctx, targets.Options{
		Netboot:       r.netboot,
		ExpectsSSH:    r.expectsSSH,
		SSHKey:        sshKey,
		AuthorizedKey: authorizedKey,
	})
	if err != nil {
		return err
	}
	// Determine the target that a command will be run against and logs will be
	// streamed from.
	primaryTarget := fuchsiaTargets[0]

	for _, t := range fuchsiaTargets {
		// Start serial servers for all targets. Will no-op for targets that
		// already have serial servers.
		if err := t.StartSerialServer(); err != nil {
			return err
		}
		// Attach an ffx instance for all targets. All ffx instances will use the same
		// config and daemon, but run commands against its own specified target. The target
		// will be set after starting the target, so that we can resolve the IP address.
		ffxForTarget := ffxutil.FFXWithTarget(ffx, "")
		t.SetFFX(&targets.FFXInstance{ffxForTarget, experiments}, ffx.Env())
	}

	eg, ctx := errgroup.WithContext(ctx)

	if err := r.setupSerialLog(ctx, eg, fuchsiaTargets); err != nil {
		return err
	}

	// Run any preflights to prepare the testbed.
	if err := r.runPreflights(ctx); err != nil {
		return err
	}

	pkgSrv, err := r.setupPackageServer(ctx)
	if pkgSrv != nil {
		defer pkgSrv.Close()
	}
	if err != nil {
		return err
	}

	r.dispatchTests(ctx, cancel, eg, baseTargets, fuchsiaTargets, primaryTarget, pkgSrv, testsPath)

	if err := eg.Wait(); err != nil {
		return err
	}

	return nil
}

// runPreflights runs opaque preflight commands passed to botanist from
// the calling infrastructure.
func (r *RunCommand) runPreflights(ctx context.Context) error {
	logger.Debugf(ctx, "checking for preflights")
	botfilePath := os.Getenv("SWARMING_BOT_FILE")
	if botfilePath == "" {
		return nil
	}
	data, err := os.ReadFile(botfilePath)
	if err != nil {
		return err
	}
	if len(data) == 0 {
		// There were no commands in the botfile, exit out.
		return nil
	}
	type preflightCommands struct {
		Commands [][]string `json:"commands"`
	}
	var cmds preflightCommands
	if err := json.Unmarshal(data, &cmds); err != nil {
		return err
	}
	runner := subprocess.Runner{
		Env: os.Environ(),
	}
	for _, c := range cmds.Commands {
		logger.Debugf(ctx, "running preflight %s", c)
		if err := runner.Run(ctx, c, subprocess.RunOptions{}); err != nil {
			return err
		}
	}
	if len(cmds.Commands) > 0 {
		// Some preflight commands can cause side effects that take up to 30s.
		time.Sleep(30 * time.Second)
	}
	logger.Debugf(ctx, "done running preflights")
	return nil
}

// createTestbedConfig creates a configuration file that describes the targets
// attached and returns the path to the file.
func (r *RunCommand) createTestbedConfig(baseTargets []targets.Base) (string, error) {
	var testbedConfig []any
	for _, t := range baseTargets {
		c, err := t.TestConfig(r.expectsSSH)
		if err != nil {
			return "", err
		}
		testbedConfig = append(testbedConfig, c)
	}

	data, err := json.Marshal(testbedConfig)
	if err != nil {
		return "", err
	}

	f, err := os.CreateTemp("", "testbed_config")
	if err != nil {
		return "", err
	}
	defer f.Close()
	if _, err := f.Write(data); err != nil {
		return "", err
	}
	return f.Name(), nil
}

// dumpSyslogOverSerial runs log_listener over serial to collect logs that may
// help with debugging. This is intended to be used when SSH connection fails to
// get some information about the failure mode prior to exiting.
func (r *RunCommand) dumpSyslogOverSerial(ctx context.Context, socketPath string) error {
	socket, err := serial.NewSocket(ctx, socketPath)
	if err != nil {
		return fmt.Errorf("newSerialSocket failed: %w", err)
	}
	defer socket.Close()
	if err := serial.RunDiagnostics(ctx, socket); err != nil {
		return fmt.Errorf("failed to run serial diagnostics: %w", err)
	}
	// Dump the existing syslog buffer. This may not work if pkg-resolver is not
	// up yet, in which case it will just print nothing.
	cmds := []serial.Command{
		{Cmd: syslog.LogListenerWithArgs("--dump_logs", "yes"), SleepDuration: 5 * time.Second},
	}
	if err := serial.RunCommands(ctx, socket, cmds); err != nil {
		return fmt.Errorf("failed to dump syslog over serial: %w", err)
	}
	return nil
}

func (r *RunCommand) runAgainstTarget(ctx context.Context, t targets.FuchsiaTarget, testsPath string, testbedConfig string) error {
	testrunnerEnv := map[string]string{
		constants.NodenameEnvKey:                   t.Nodename(),
		constants.SerialSocketEnvKey:               t.SerialSocketPath(),
		constants.ECCableEnvKey:                    os.Getenv(constants.ECCableEnvKey),
		constants.TestbedConfigEnvKey:              testbedConfig,
		testrunnerconstants.TestTimeoutScaleFactor: strconv.Itoa(r.testTimeoutScaleFactor),
	}

	if r.expectsSSH {
		ipv6, err := t.IPv6()
		if err != nil {
			return err
		}
		ipv4, err := t.IPv4()
		if err != nil {
			return err
		}
		addr, err := targets.IPAddr(t)
		if err != nil {
			return err
		}

		testrunnerEnv[constants.DeviceAddrEnvKey] = addr.String()
		testrunnerEnv[constants.IPv4AddrEnvKey] = ipv4.String()
		testrunnerEnv[constants.IPv6AddrEnvKey] = ipv6.String()
	}

	// One would assume this should only be provisioned when paving, but
	// there are some tests that attempt to SSH into a netbooted image that
	// has our SSH keys baked into it. Therefore, we add the SSH key to the
	// environment unconditionally. Additionally, some tools like FFX often
	// require the SSH key path to be absolute (https://fxbug.dev/42051867).
	if t.SSHKey() != "" {
		absKeyPath, err := filepath.Abs(t.SSHKey())
		if err != nil {
			return err
		}
		testrunnerEnv[constants.SSHKeyEnvKey] = absKeyPath
	}

	// TODO(https://fxbug.dev/42063235): testrunner does heavy use of env
	// variables. Setting these env variables is temporary until we refactor
	// testrunner to take these variables as arguments or flags.
	for k, v := range testrunnerEnv {
		err := os.Setenv(k, v)
		if err != nil {
			return fmt.Errorf("error setting env variable %s=%s. %w", k, v, err)
		}
	}
	setEnviron(t.FFXEnv())
	r.testrunnerOptions.FFX = t.GetFFX().FFXInstance
	r.testrunnerOptions.Experiments = botanist.GetExperiments(r.experiments)

	if err := testrunner.SetupAndExecute(ctx, r.testrunnerOptions, testsPath); err != nil {
		return fmt.Errorf("testrunner with flags: %v, with timeout: %s, failed: %w", r.testrunnerOptions, r.timeout, err)
	}
	return nil
}

// setEnviron sets |environ| into the os.Env.
// The string in the environ slice must be in the format "key=value".
func setEnviron(environ []string) {
	for _, env := range environ {
		keyval := strings.Split(env, "=")
		os.Setenv(keyval[0], keyval[1])
	}
}

func (r *RunCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	args := f.Args()
	if len(args) == 0 {
		return subcommands.ExitUsageError
	}

	// If the TestOutDirEnvKey was set, that means botanist is being run in an infra
	// setting and thus needs an isolated environment.
	testOutDir, needsIsolatedEnv := os.LookupEnv(testrunnerconstants.TestOutDirEnvKey)
	cleanUp, err := environment.Ensure(needsIsolatedEnv)
	if err != nil {
		logger.Errorf(ctx, "failed to setup environment: %s", err)
		return subcommands.ExitFailure
	}
	defer cleanUp()

	if needsIsolatedEnv {
		// Use a temp directory for the output directory which we will move to the
		// actual testOutDir once the command completes. Otherwise, when run in a
		// swarming task, a subprocess that doesn't properly finish could still be
		// writing to the out dir as we try to upload the contents with the swarming
		// task outputs which will result in the swarming bot failing with BOT_DIED.
		tmpOutDir, err := os.MkdirTemp("", "")
		if err != nil {
			return subcommands.ExitFailure
		}
		if err := os.Setenv(testrunnerconstants.TestOutDirEnvKey, tmpOutDir); err != nil {
			return subcommands.ExitFailure
		}
		defer func() {
			if skippedFiles, err := osmisc.CopyDir(tmpOutDir, testOutDir, osmisc.SkipUnknownFiles); err != nil {
				logger.Errorf(ctx, "failed to copy outputs to %s: %s", testOutDir, err)
				// TODO(https://fxbug.dev/42079078): If we fail to copy outputs, at least copy
				// the ffx logs over so we can debug. Remove when attached bug is
				// fixed.
				if r.ffxPath != "" {
					ffxLogsDir := filepath.Join("ffx_outputs", "ffx_logs")
					if _, err := os.Stat(filepath.Join(testOutDir, ffxLogsDir)); os.IsNotExist(err) {
						if _, err := osmisc.CopyDir(filepath.Join(tmpOutDir, ffxLogsDir), filepath.Join(testOutDir, ffxLogsDir), osmisc.RaiseError); err != nil {
							logger.Errorf(ctx, "failed to copy ffx logs to %s: %s", filepath.Join(testOutDir, ffxLogsDir), err)
						}
					}
				}
			} else if len(skippedFiles) > 0 {
				skippedFilesTxt := filepath.Join(testOutDir, "skipped_files.txt")
				if err := os.WriteFile(skippedFilesTxt, []byte(strings.Join(skippedFiles, "\n")), os.ModePerm); err != nil {
					logger.Errorf(ctx, "failed to write %s: %s\nskipped files: %s", skippedFilesTxt, err, skippedFiles)
				}
			}

			if err := os.Setenv(testrunnerconstants.TestOutDirEnvKey, testOutDir); err != nil {
				logger.Errorf(ctx, "failed to reset %s to %s: %s", testrunnerconstants.TestOutDirEnvKey, testOutDir, err)
			}
			if err := os.RemoveAll(tmpOutDir); err != nil {
				logger.Errorf(ctx, "failed to remove temp outputs dir %s: %s", tmpOutDir, err)
			}
		}()
	}

	if err := r.execute(ctx, args); err != nil {
		logger.Errorf(ctx, "%s", err)
		return subcommands.ExitFailure
	}
	return subcommands.ExitSuccess
}

func (r *RunCommand) deriveTargetsFromFile(ctx context.Context, targetOpts targets.Options) ([]targets.Base, []targets.FuchsiaTarget, error) {
	data, err := os.ReadFile(r.configFile)
	if err != nil {
		return nil, nil, fmt.Errorf("%s: %w", constants.ReadConfigFileErrorMsg, err)
	}
	var configs []json.RawMessage
	if err := json.Unmarshal(data, &configs); err != nil {
		return nil, nil, fmt.Errorf("could not unmarshal config file as a JSON list: %w", err)
	}

	var baseTargets []targets.Base
	var fuchsiaTargets []targets.FuchsiaTarget

	for _, config := range configs {
		t, err := targets.FromJSON(ctx, config, targetOpts)
		if err != nil {
			return nil, nil, err
		}
		baseTargets = append(baseTargets, t)
		if f, ok := t.(targets.FuchsiaTarget); ok {
			fuchsiaTargets = append(fuchsiaTargets, f)
		}
	}

	if len(fuchsiaTargets) == 0 {
		return nil, nil, fmt.Errorf("no Fuchsia targets found")
	}

	return baseTargets, fuchsiaTargets, nil
}
