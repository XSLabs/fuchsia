#!/usr/bin/env fuchsia-vendored-python
# Copyright 2017 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Build script for a Go app.

import argparse
import errno
import filecmp
import json
import os
import shutil
import subprocess
import sys

from gen_library_metadata import FUCHSIA_MODULE, get_sources


# rmtree manually removes all subdirectories and files instead of using
# shutil.rmtree, to avoid registering spurious reads on stale
# subdirectories. See https://fxbug.dev/42153728.
def rmtree(dir):
    if not os.path.exists(dir):
        return
    for root, dirs, files in os.walk(dir, topdown=False):
        for file in files:
            os.unlink(os.path.join(root, file))
        for dir in dirs:
            full_path = os.path.join(root, dir)
            if os.path.islink(full_path):
                os.unlink(full_path)
            else:
                os.rmdir(full_path)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--root-out-dir", help="Path to root of build output", required=True
    )
    parser.add_argument(
        "--cc", help="The C compiler to use", required=False, default="cc"
    )
    parser.add_argument(
        "--cxx", help="The C++ compiler to use", required=False, default="c++"
    )
    parser.add_argument(
        "--clang-fuchsia-api-level",
        help='The target Fuchsia API level. Required iff "--current-os" is "fuchsia"',
        type=int,
    )
    parser.add_argument(
        "--ar", help="The archive linker to use", required=False, default="ar"
    )
    parser.add_argument(
        "--objcopy",
        help="The objcopy tool to use",
        required=False,
        default="objcopy",
    )
    parser.add_argument("--sysroot", help="The sysroot to use", required=False)
    parser.add_argument(
        "--target", help="The compiler target to use", required=False
    )
    parser.add_argument(
        "--current-cpu",
        help="Target architecture.",
        choices=["x64", "arm64"],
        required=True,
    )
    parser.add_argument(
        "--current-os",
        help="Target operating system.",
        choices=["fuchsia", "linux", "mac", "win"],
        required=True,
    )
    parser.add_argument("--buildidtool", help="The path to the buildidtool.")
    parser.add_argument(
        "--build-id-dir", help="The path to the .build-id directory."
    )
    parser.add_argument(
        "--go-root", help="The go root to use for builds.", required=True
    )
    parser.add_argument(
        "--go-cache", help="Cache directory to use for builds.", required=False
    )
    parser.add_argument(
        "--golibs-dir",
        help="The directory containing third party libraries.",
        required=True,
    )
    parser.add_argument(
        "--is-test", help="True if the target is a go test", default=False
    )
    parser.add_argument(
        "--gcflag",
        help="Arguments to pass to Go compiler",
        action="append",
        default=[],
    )
    parser.add_argument(
        "--ldflag",
        help="Arguments to pass to Go linker",
        action="append",
        default=[],
    )
    parser.add_argument(
        "--go-dep-files",
        help="List of files describing library dependencies",
        nargs="*",
        default=[],
    )
    parser.add_argument(
        "--go-sources",
        help="List of Go source files to include during compilation",
        nargs="*",
        default=[],
    )
    parser.add_argument("--binname", help="Output file", required=True)
    parser.add_argument(
        "--output-path",
        help="Where to output the (unstripped) binary",
        required=True,
    )
    parser.add_argument(
        "--stripped-output-path",
        help="Where to output a stripped binary, if supplied",
    )
    parser.add_argument(
        "--verbose",
        help="Tell the go tool to be verbose about what it is doing",
        action="store_true",
    )

    pkg_group = parser.add_mutually_exclusive_group(required=True)
    pkg_group.add_argument("--package", help="The package name")
    pkg_group.add_argument(
        "--library-metadata",
        help=(
            "go_deps file containing metadata about the package to build, "
            "as generated by gen_library_metadata.py"
        ),
    )

    parser.add_argument(
        "--include-dir",
        help="-isystem path to add",
        action="append",
        default=[],
    )
    parser.add_argument(
        "--lib-dir", help="-L path to add", action="append", default=[]
    )
    parser.add_argument("--vet", help="Run go vet", action="store_true")
    parser.add_argument(
        "--tag", help="Add a go build tag", default=[], action="append"
    )
    parser.add_argument(
        "--cgo", help="Whether to enable CGo", action="store_true"
    )
    args = parser.parse_args()

    try:
        os.makedirs(args.go_cache)
    except OSError as e:
        if e.errno == errno.EEXIST and os.path.isdir(args.go_cache):
            pass
        else:
            raise

    goarch = {
        "x64": "amd64",
        "arm64": "arm64",
    }[args.current_cpu]
    goos = {
        "fuchsia": "fuchsia",
        "linux": "linux",
        "mac": "darwin",
        "win": "windows",
    }[args.current_os]

    dist = args.stripped_output_path or args.output_path

    # Project path is a package specific gopath, also known as a "project" in go parlance.
    project_path = os.path.join(
        args.root_out_dir, "gen", "gopaths", args.binname
    )

    # Clean up any old project path to avoid leaking old dependencies.
    gopath_src = os.path.join(project_path, "src")
    rmtree(gopath_src)

    dst_vendor = os.path.join(gopath_src, "vendor")
    os.makedirs(dst_vendor)
    # Symlink interprets path against the current working directory, so use
    # absolute path for consistency.
    abs_golibs_dir = os.path.abspath(args.golibs_dir)
    for src in ["go.mod", "go.sum"]:
        os.symlink(
            os.path.join(abs_golibs_dir, src), os.path.join(gopath_src, src)
        )
    os.symlink(
        os.path.join(os.path.join(abs_golibs_dir, "vendor"), "modules.txt"),
        os.path.join(dst_vendor, "modules.txt"),
    )

    if args.package:
        package = args.package
    elif args.library_metadata:
        with open(args.library_metadata) as f:
            package = json.load(f)["package"]

    files_to_link = [
        (os.path.join(package, os.path.basename(f)), f) for f in args.go_sources
    ] + (
        list(get_sources(args.go_dep_files).items())
        if args.go_dep_files
        else []
    )

    linked = set()
    # Create a GOPATH for the packages dependency tree.
    for dst, src in sorted(files_to_link):
        # This path is later used in go commands that run in cwd=gopath_src.
        src = os.path.abspath(src)
        if not args.is_test and src.endswith("_test.go"):
            continue

        # If the destination is part of the "main module", strip off the
        # module path. Otherwise, put it in the vendor directory.
        if dst.startswith(FUCHSIA_MODULE):
            dst = os.path.relpath(dst, FUCHSIA_MODULE)
        else:
            dst = os.path.join("vendor", dst)

        if dst.endswith("/..."):
            # When a directory and all its subdirectories must be made available, map
            # the directory directly.
            dst = dst[:-4]
        elif os.path.isfile(src):
            # When sources are explicitly listed in the BUILD.gn file, each `src` will
            # be a path to a file that must be mapped directly.
            #
            # Paths with /.../ in the middle designate go packages that include
            # subpackages, but also explicitly list all their source files.
            #
            # The construction of these paths is done in the go list invocation, so we
            # remove these sentinel values here.
            dst = dst.replace("/.../", "/")
        else:
            raise ValueError(f"Invalid go_dep entry: {dst=}, {src=}")

        dstdir = os.path.join(gopath_src, dst)

        # Skip previously linked files. This could happen when a binary has
        # dependencies with overlapping sources.
        #
        # This is currently only possible when depending on golibs that use
        # `...`, which are globbing-like targets that provision multiple Go
        # packages. For example: `golang.org/x/net` includes
        # `golang.org/x/net/bpf`, and it's possible to have both in the
        # dependency graph.
        #
        # TODO(https://fxbug.dev/377788797): Re-evaluate support for `...`.
        to_link = (src, dstdir)
        if to_link in linked:
            continue
        linked.add(to_link)

        # Make a symlink to the src directory or file.
        parent = os.path.dirname(dstdir)
        if not os.path.exists(parent):
            os.makedirs(parent)
        # hardlink regular files instead of symlinking to handle non-Go
        # files that we want to embed using //go:embed, which doesn't
        # support symlinks.
        # TODO(https://fxbug.dev/42162237): Add a separate mechanism for
        # declaring embedded files, and only hardlink those files
        # instead of hardlinking all sources.
        if os.path.isdir(src):
            os.symlink(src, dstdir)
        else:
            try:
                os.link(src, dstdir)
            except OSError:
                # Hardlinking may fail, for example if `src` is in a
                # separate filesystem on a mounted device.
                shutil.copyfile(src, dstdir)

    cflags = []
    if args.sysroot:
        cflags.extend(["--sysroot", os.path.abspath(args.sysroot)])
    if args.target:
        cflags.extend(["-target", args.target])

    if (args.clang_fuchsia_api_level is None) != (args.current_os != "fuchsia"):
        parser.error(
            '--clang-fuchsia-api-level must be specified if and only if the target OS is "fuchsia".'
        )
    if args.current_os == "fuchsia":
        if args.clang_fuchsia_api_level <= 0:
            parser.error(
                "--clang-fuchsia-api-level must specify a positive integer. Value specified: "
                + str(args.clang_fuchsia_api_level)
            )
        cflags.extend(
            ["-ffuchsia-api-level=" + str(args.clang_fuchsia_api_level)]
        )

    ldflags = cflags[:]
    if args.current_os == "linux":
        ldflags.extend(
            [
                "-stdlib=libc++",
                # TODO(https://fxbug.dev/42142932): the following flags are not recognized by CGo.
                # '-rtlib=compiler-rt',
                # '-unwindlib=libunwind',
            ]
        )

    for dir in args.include_dir:
        cflags.extend(["-isystem", os.path.abspath(dir)])
    ldflags.extend(["-L" + os.path.abspath(dir) for dir in args.lib_dir])

    cflags_joined = " ".join(cflags)
    ldflags_joined = " ".join(ldflags)

    build_goroot = os.path.abspath(args.go_root)

    env = {
        # /usr/bin:/bin are required for basic things like bash(1) and env(1). Note
        # that on Mac, ld is also found from /usr/bin.
        "PATH": os.path.join(build_goroot, "bin") + ":/usr/bin:/bin",
        "GOARCH": goarch,
        "GOOS": goos,
        # GOPATH won't be used, but Go still insists that we set it. Without it,
        # Go emits the succinct error: `missing $GOPATH`. Go further insists
        # that $GOPATH/go.mod not exist; if we pass `gopath_src` here (which
        # is where we symlinked our go.mod), we get another succinct error:
        # `$GOPATH/go.mod exists but should not`. Finally, GOPATH must be
        # absolute, otherwise:
        #
        # go: GOPATH entry is relative; must be absolute path: ...
        # For more details see: 'go help gopath'
        #
        # and here we are.
        "GOPATH": os.path.abspath(project_path),
        # Disallow downloading modules from any source.
        #
        # See https://golang.org/ref/mod#environment-variables under `GOPROXY`.
        "GOPROXY": "off",
        # Some users have GOROOT set in their parent environment, which can break
        # things, so it is always set explicitly here.
        "GOROOT": build_goroot,
        # GOCACHE, CC and CXX below may be used in different working directories
        # so they have to be absolute.
        "GOCACHE": os.path.abspath(args.go_cache),
        "AR": os.path.abspath(args.ar),
        "CC": os.path.abspath(args.cc),
        "CXX": os.path.abspath(args.cxx),
        "CGO_CFLAGS": cflags_joined,
        "CGO_CPPFLAGS": cflags_joined,
        "CGO_CXXFLAGS": cflags_joined,
        "CGO_LDFLAGS": ldflags_joined,
    }

    # This variable is used by LLVM profile runtime.
    if llvm_profile_file := os.getenv("LLVM_PROFILE_FILE"):
        env["LLVM_PROFILE_FILE"] = llvm_profile_file

    # Infra sets $TMPDIR which is cleaned between builds.
    if tmpdir := os.getenv("TMPDIR"):
        env["TMPDIR"] = tmpdir

    if args.cgo:
        env["CGO_ENABLED"] = "1"
    if args.target:
        env["CC_FOR_TARGET"] = env["CC"]
        env["CXX_FOR_TARGET"] = env["CXX"]

    go_tool = os.path.join(build_goroot, "bin", "go")

    if args.vet:
        subprocess.run(
            [go_tool, "vet", package], env=env, cwd=gopath_src
        ).check_returncode()

    cmd = [go_tool]
    if args.is_test:
        cmd += ["test", "-c"]
    else:
        cmd += ["build", "-trimpath"]
    if args.verbose:
        cmd += ["-x"]
    if args.tag:
        cmd += ["-tags=%s" % ",".join(args.tag)]
    if args.gcflag:
        cmd += ["-gcflags=%s" % " ".join(args.gcflag)]
    # Clear the buildid to make the build reproducible
    ldflag = ["-buildid="]
    if args.ldflag:
        ldflag += args.ldflag
    cmd += ["-ldflags=%s" % " ".join(ldflag)]

    # If an output file already exists, compile to a temporary file, and then
    # only if the new file and the existing file are different, move the new
    # file over the existing file.
    if os.path.exists(args.output_path):
        compilation_output_path = args.output_path + ".new"

        # Since the go compiler is written in go, the action tracer can't see
        # any writes that it does, so we need to first write to the temp file to
        # make it visible to the action tracer
        with open(compilation_output_path, "w") as touch:
            touch.write("")
    else:
        compilation_output_path = args.output_path

    cmd += [
        # Omit version control information so that binaries are deterministic
        # based on their source code and don't change on each commit.
        "-buildvcs=false",
        "-pkgdir",
        os.path.join(project_path, "pkg"),
        "-o",
        os.path.relpath(compilation_output_path, gopath_src),
        package,
    ]
    proc = subprocess.run(
        cmd,
        env=env,
        cwd=gopath_src,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    # We want to both capture stdout/stderr and print it but there isn't an easy
    # way to do that automatically, so we must separately print the captured
    # output.
    print(proc.stdout, end="")

    if proc.returncode:
        if not proc.stdout:
            raise Exception(
                "go build had an exit code of %d but did not print any output"
                % proc.returncode
            )
        # Don't raise an exception because that would add a noisy Python
        # traceback that clutters up the relevant output from `go build`.
        return proc.returncode

    try:
        # Check to see if the compiled file is different from the existing file
        if compilation_output_path != args.output_path:
            if filecmp.cmp(
                compilation_output_path, args.output_path, shallow=False
            ):
                # The newly compiled file matches, so exit early after cleaning up.
                os.remove(compilation_output_path)
                return 0
            else:
                # Move the newly compiled file over the existing and continue with
                # any processing.
                os.rename(compilation_output_path, args.output_path)

        # If the package contains no *_test.go files `go test -c` will exit with a
        # retcode of zero, but not produce the expected output file. Instead it will
        # print a warning like "no test files".
        #
        # Not producing the expected output file leads to confusing no-op failures
        # and breakages in targets that depend on this one, so we should turn it
        # into an immediate failure. We can't check if the file exists to determine
        # whether the build succeeded because the file might have been created by a
        # previous build, so instead we assume that Go will only print anything if
        # it didn't produce the output file, or otherwise failed in some fatal way.
        if proc.stdout.strip():
            raise Exception("go build printed unexpected output")

        if args.stripped_output_path:
            if args.current_os == "mac":
                subprocess.run(
                    [
                        "xcrun",
                        "strip",
                        "-x",
                        args.output_path,
                        "-o",
                        args.stripped_output_path,
                    ],
                    env=env,
                ).check_returncode()
            else:
                subprocess.run(
                    [
                        args.objcopy,
                        "--strip-sections",
                        args.output_path,
                        args.stripped_output_path,
                    ],
                    env=env,
                ).check_returncode()

        if args.buildidtool:
            if not args.build_id_dir:
                raise ValueError("Using --buildidtool requires --build-id-dir")
            subprocess.run(
                [
                    args.buildidtool,
                    "-build-id-dir",
                    args.build_id_dir,
                    "-stamp",
                    dist + ".build-id.stamp",
                    "-entry",
                    ".debug=" + args.output_path,
                    "-entry",
                    "=" + dist,
                ]
            ).check_returncode()

    finally:
        # Clean up the tree of go files assembled in gopath_src to indicate to the
        # action tracer that they were intermediates and not final outputs.
        rmtree(gopath_src)

    return 0


if __name__ == "__main__":
    sys.exit(main())
