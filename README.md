# `fuser` source dependency for Fork

> [!IMPORTANT]
> This repository is a narrowly maintained source fork used by
> [Fork](https://github.com/fork-cloud/fork). It is not an independently
> supported distribution of `fuser`. For the general-purpose crate, releases,
> documentation, and community support, use
> [upstream `fuser`](https://github.com/cberner/fuser).

## Why this fork exists

Fork mounts workspaces through macFUSE's FSKit provider. That provider uses
libfuse2's message-oriented channel API instead of exposing a pollable FUSE file
descriptor. Upstream `fuser` does not currently support that transport.

This fork adds the Darwin channel transport and the associated mount, session,
teardown, and regression-test coverage required by Fork. Changes unrelated to
Fork's requirements belong upstream.

## Provenance

The fork is based on [`fuser` 0.18.0](https://github.com/cberner/fuser/releases/tag/v0.18.0)
at upstream commit
[`9c957f74efe715112049298cdf1d601781829c8d`](https://github.com/cberner/fuser/commit/9c957f74efe715112049298cdf1d601781829c8d).
The upstream history is preserved, and Fork-specific changes are kept as a
small patch series on top.

The package intentionally retains the name `fuser` so that Fork can consume it
as a source dependency. It is not published to crates.io.

## Consumption and maintenance

- Fork pins a full commit revision rather than depending on a branch tip.
- The fork is updated only when Fork requires a change or an upstream update.
- It makes no release, API-compatibility, or support commitment to other users.
- General `fuser` bugs and feature requests should go to
  [upstream](https://github.com/cberner/fuser/issues). Fork-specific integration
  defects should be reported to [Fork](https://github.com/fork-cloud/fork/issues).

The repository remains available as source so Fork builds can be reproducible
and the dependency delta can be audited without credentials.

## License and attribution

The upstream project and Fork-specific changes are licensed under the
[MIT License](LICENSE.md), except for files in `examples/` that explicitly state
a different license. Upstream copyright and attribution are preserved in the
source history and license.
