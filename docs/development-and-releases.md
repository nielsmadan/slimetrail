# Development and releases

## Prepare the checkout

Use Rustup, Python 3.11+, Just, curl, tar and `timeout` (install coreutils if absent).
`rust-toolchain.toml` pins the development compiler and rustfmt/Clippy/LLVM tools;
`Cargo.toml` separately declares the supported MSRV. CI checks both.

Run `timeout 900 just setup`. It downloads checksum-verified tools into `.cache/tools`,
fetches locked Cargo dependencies, installs Lefthook hooks and runs `just doctor`.
`just setup-tools` installs only the analysis tools and is used by CI. Tool versions and
the four platform-specific checksums live in `scripts/tooling.sh`; update them together.
Rustup must be able to install the declared toolchain/components before running checks.

| Command | Coverage |
| --- | --- |
| `just check` | Rust/Python formatting and lint, Rust all-target/all-feature tests, Python tooling tests, worktree secret scan |
| `just check-all` | All basic checks plus dependency policy, unused dependencies, workflow lint and rustdoc with warnings denied |
| `just dependency-policy` | Cargo deny advisories, licenses, dependency sources and version policy |
| `just check-secret-history` | Redacted Gitleaks scan of reachable Git history |
| `just coverage` | LLVM coverage summary, HTML and LCOV under `.cache/coverage` |
| `just bench` | Release build, synthetic search measurements and incremental/cleanup validation |
| `just build` | Host release binary under `target/release` |
| `just install` / `just uninstall` | Cargo installation; respects `CARGO_INSTALL_ROOT` |

Checks use command deadlines. The benchmark has no latency threshold, and coverage is
reported without an arbitrary minimum. Tests and lint run with `--all-features`, which
enables the `test-hooks` feature: a build-limit override (`SLIMETRAIL_TEST_BUILD_LIMIT_MS`)
and a discovery counter that some integration tests need. Release builds leave it off, so
plain `cargo test` skips those tests. See the [performance procedure](tests/search-performance/README.md).
The pre-commit hook checks changed Rust/Python file types and scans staged changes for
secrets; pre-push runs `just check-all`. Secret scans never follow symlinks, and the worktree
scan includes tracked/unignored files only, excluding private `.local` indexes.

Clippy enables `pedantic` and `nursery`, with warnings denied. Rust forbids unsafe code
and warns on unreachable public items. Internal APIs allow `missing_errors_doc`,
`must_use_candidate` and `redundant_pub_crate`, following Hooklinesinker. Narrow exceptions
carry reasons at their use sites. Review new nursery suggestions before applying them.

## CI and dependency maintenance

[CI](../.github/workflows/ci.yml) runs quality checks on Linux and macOS, checks the
manifest's MSRV, builds all four release targets and produces Linux coverage artifacts.
The final `check` job succeeds only when every required job succeeds. Workflows use
explicit permissions, pinned action commits, bounded jobs and Rust caches.

[Dependabot](../.github/dependabot.yml) checks Cargo and Actions weekly on Monday at
08:00 Europe/Berlin, groups minor/patch updates and uses a seven-day cooldown.
The auto-merge workflow enables squash auto-merge only for Dependabot minor/patch PRs.
It runs on `pull_request_target` with the built-in token, reads metadata only and never
checks out or executes PR content. It refuses to arm auto-merge unless an active branch
rule requires the GitHub Actions `check` status. The repository must also enable auto-merge.

An administrator can apply those settings with:

```sh
gh repo edit nielsmadan/slimetrail --enable-auto-merge --enable-squash-merge
gh api --method POST repos/nielsmadan/slimetrail/rulesets --input .github/required-ci.json
```

The ruleset gives repository administrators the same bypass as Hooklinesinker. Create it
once; inspect `gh ruleset list` before reapplying. An existing rule should be updated
by its ID rather than duplicated. The new required status becomes available when this
CI workflow reaches GitHub.

[Scheduled checks](../.github/workflows/scheduled.yml) repeat analysis weekly and retain
aggregate synthetic benchmark output. They never opt into real session stores.

## Build release artifacts

`just build-release` builds `aarch64-apple-darwin`, `x86_64-apple-darwin`,
`aarch64-unknown-linux-gnu` and `x86_64-unknown-linux-gnu`. Both Apple targets are combined
with `lipo`. The output is `dist/slimetrail-macos-universal`,
`dist/slimetrail-linux-aarch64`, `dist/slimetrail-linux-x86_64` and `dist/SHA256SUMS`.
Pass a subset of targets when cross toolchains are unavailable, for example
`just build-release aarch64-apple-darwin x86_64-apple-darwin` on macOS.

Install the requested targets with Rustup first. Linux ARM cross-builds require a C
compiler for bundled SQLite as well as the Rust linker; CI sets both
`CC_aarch64_unknown_linux_gnu` and `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER` to
`aarch64-linux-gnu-gcc`. Linux artifacts are built on Ubuntu 24.04 with GNU libc;
compatibility with older distributions is not established. Failed builds preserve an
existing `dist`. Verify downloaded artifacts against `SHA256SUMS` before execution.

## Prepare and publish

Install [Curtains](https://github.com/nielsmadan/curtaincall), then run `just release`
from clean `main`, with complete history and signing/push credentials. Policy lives in
[`scripts/release.json`](../scripts/release.json). `just release --dry-run` previews the
proposal; `just release patch`, `minor` or an exact version selects the version.
The initial release proposal is `0.1.0`.

Curtains runs `just check-all`, asks for confirmation, updates Cargo manifest/lock
versions through `scripts/prepare_release.py`, commits and creates a signed annotated
tag, then atomically pushes the branch and tag. This is a publication command, distinct
from building or installing locally. Agents do not push or invoke a publishing release.

The [tag workflow](../.github/workflows/release.yml) validates tag ancestry, runs quality,
coverage and MSRV gates, builds all targets, verifies the compiled version against the
tag, emits checksums and build provenance, and publishes through Curtains. Release notes
come from commit subjects since the preceding release. The publish job revalidates the
tag and uses narrowly scoped write permissions. A successful local command does not
establish that the remote workflow finished; verify its result and assets separately.

Publication uses a draft until all assets are uploaded. If publication fails, rerun
only that job to reuse the same artifacts; never replace a public tag. Native-store
mutations, consumer migrations, crates.io publication and Homebrew distribution remain
separate work. The Rust library is internal and the package has `publish = false`.
