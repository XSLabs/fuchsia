# Integrator Development Kit (IDK)

This folder contains information about developing the Fuchsia Integrator Development Kit (IDK).

> [Download the Fuchsia IDK](download.md)

## Support

Please note that at this time, Fuchsia does not support public usage of
the Fuchsia IDK. The APIs in the IDK are subject to change at any time without notice.

## Strategy {#strategy}

Fuchsia is taking a modular approach to exposing the Fuchsia platform to developers.

At the center of this strategy is the Integrator Development Kit (IDK), distilled out
of the Git repository mentioned in [Contributing changes](/docs/development/source_code/contribute_changes.md).
This IDK contains a small set of libraries and tools required to start building
and running programs that target Fuchsia.
The contents of that IDK represent the most basic contract that the Fuchsia
platform developers offer to prospective developers.

The Fuchsia IDK is not suitable for immediate consumption.
It does not contain any reference to toolchains or build systems, and in fact
does not require any specific instance of these.
While this might be viewed as a drawback, this is actually a feature, an
integral part of a layered approach to building a fully-functional SDK.
Even though it is not tied to a particular build system, the IDK contains
metadata that may be used to produce support for a large variety of build
systems, thereby producing various SDK distributions.
Having the IDK cleanly separated from these various distributions allows
for very flexible release schemes and iteration cycles.

The present documentation focuses on the details of the creation process of the
IDK.
The documentation included in the IDK, hosted under `//idk/docs`, contains
information regarding how to work with the IDK.

## What belongs in the IDK? {#what-belongs-in-an-idk}

By default, a piece of code in the Fuchsia tree cannot be added to any IDK:
participation is a strictly opt-in decision. Additionally, this decision is
encoded locally within the code's build file. This was done for multiple
reasons:

1. Developers modifying the code need to be aware of the potential impact on
   external customers as early as possible;
1. Publishing that code to the IDK may require extra input from the developers to
   inform the build system about how to properly include that code in an SDK;
1. Knowing whether the code may be included in an IDK or not allows the build
   system to perform extra checks on that code to ensure conformity with IDK
   standards.

In order to be made available in the IDK, a piece of code must follow a set of
[standards and guidelines](standards.md).


## Infrastructure {#infrastructure}

The SDK creation pipeline consists of two pieces:

1. The backend, which uses the build system to generate a tarball containing
   compiled artifacts, source files, and metadata;
1. The frontend, which applies transformations to that tarball and turn into
   e.g. an SDK distribution.

### Backend {#backend}

The backend really is just a specialized use of the build system. In other
words, running the SDK backend amounts to passing the right set of arguments to
the Fuchsia build system, which in turn produces an archive with a
[set layout](layout.md).
The inner workings of the backend are described [here][backend].

The backend does not just produce an IDK: it is also used as a control mechanism
for API evolution. The API surface exposed by the IDK is captured in a set of
reference files representing its elements: modifications to this surface need to
be explicitly acknowledged by developers by updating the relevant reference
files, whose latest version is also generated by the backend. The purpose of
this mechanism is to detect and prevent accidental changes to the IDK as early
as possible in the release cycle, as well as give us tools to observe and review
the evolution of the API surface.

### Frontend {#frontend}

The term frontend is used to describe any process that ingests the Fuchsia IDK
archive and applies transformations to it.

In the Fuchsia tree, frontends are used to generate SDK distributions, e.g. a Bazel-ready
workspace.

Frontends may also be used to adapt a Fuchsia IDK archive for consumption in a
particular development environment by for example generating build files for a
given build system. The presence of extensive metadata in the archive itself
allows for this kind of processing.


## IDK

The partner IDK is represented by the `//sdk:final_fuchsia_idk` target.


## Recipes {#recipes}

### Generating an IDK archive {#generating-an-idk-archive}

The various targets representing IDKs are always included in the build graph.
In order to build the contents of an IDK, [build][fx-build-target] one of the
targets above.

Note that this will generate and verify IDK contents, but won't actually build
an archive with these contents.

To build the archive, run the following commands:

    fx set minimal.x64
    fx build sdk:final_fuchsia_idk

The resulting archive will be located under
`<outdir>/sdk/archive/fuchsia_idk.tar.gz`.

The IDK includes host tools needed for Fuchsia development. By default, when
the IDK is built locally, it only includes host tools for the current host
architecture, either x64 or arm64. When building the IDK on x64 hosts, you can
also include arm64 host tools by setting:

   fx set minimal.x64 --args=sdk_cross_compile_host_tools=true

### Adding content to an IDK {#adding-content-to-an-idk}

The first step is to make that content available to SDKs. This is done by using
a set of templates listed in the [backend documentation][backend].
The next step is to add that content to an existing IDK definition. For a target
`//path/to/my:super_target`, this is accomplished by making the implicit
`//path/to/my:super_target_sdk` target a dependency of the `sdk` target.

Note that some content types require a `.api` source file describing the state
of the IDK element's API.
These files are produced by the build system.
In order to seed the first version of such a file, let the build system tell you
where it expects to find the file, then create this file and leave it empty,
and finally run the build again: it will again tell you where to get the initial
version from.

### Turning IDK-related errors into warnings {#turning-idk-related-errors}

There exist some build steps to verify that the contents of an IDK don't get
modified by accident. An unacknowledged modification results in a build failure
until the relevant reference files are updated in the source tree.
While locally iterating on some public API, having to repeatedly update
reference files can be tedious. In order to turn the build errors into warnings,
configure then build with this extra GN argument: `warn_no_sdk_changes=true`.



[backend]: /build/sdk/README.md
[fx-config]: /docs/development/build/fx.md#configure-a-build
[fx-build-target]: /docs/development/build/fx.md#building-a-specific-target
