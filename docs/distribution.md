# Distribution + update system

This doc describes how `libertai-cli` is distributed, what's already in the
repo, what still needs manual GitHub/DNS/crates.io setup, and how to cut a
release once everything is wired up. The overall design mirrors the Aleph
Rust CLI (`github.com/aleph-im/aleph-rs`) per CTO direction — hand-rolled, no
third-party release tooling (cargo-dist, axoupdater, etc.).

## Status (2026-04-22)

### In the repo and ready

- `Cargo.toml` metadata (`repository`, `homepage`, `readme`, `keywords`,
  `categories`, `rust-version`, `exclude`, `[package.metadata.deb]`).
- `rust-toolchain.toml` pinned to `stable` channel.
- `packaging/install.sh` — universal one-liner for Linux / macOS / WSL.
- `packaging/apt/install.sh` — APT-repo bootstrap for Debian/Ubuntu.
- `packaging/brew/formula.rb.tmpl` — Homebrew formula template.
- `.github/workflows/release.yml` — 8-job pipeline (verify →
  check-version → check-release → build-binaries → [publish-apt,
  publish-brew] → request-publish-approval → publish-crates).
- `src/update_check.rs` + wiring — passive 24h update-check banner.
- `README.md` rewrite of the `## Install` section.

### Blocked on manual setup (see below)

No release can cut until every item in [Prerequisites](#prerequisites) is
done. Once they are, push a `v*.*.*` tag and the workflow does the rest.

---

## Prerequisites

All human-only, one-time. Do them in any order except where noted.

### 1. Reserve the crate name on crates.io

```sh
cargo publish --dry-run
cargo publish
```

Needs `~/.cargo/credentials.toml` from
[crates.io/me](https://crates.io/settings/tokens). This publishes `0.1.0` so
no one else squats `libertai-cli`. The release workflow will publish future
versions automatically.

### 2. Create `github.com/Libertai/homebrew-tap`

Public repo, name **must** start with `homebrew-` for `brew tap Libertai/tap`
to resolve. Seed with a README; the release workflow creates
`Formula/libertai.rb` on each stable tag.

### 3. Create `github.com/Libertai/apt` + GitHub Pages + DNS

1. Create `github.com/Libertai/apt` (public, empty).
2. Repo settings → Pages → source: `main` branch, `/` root. Save.
3. DNS: `apt.libertai.io` CNAME → `libertai.github.io`. TTL whatever your
   registrar defaults to.
4. First workflow run writes `CNAME`, `gpg.key`, and `install.sh` into the
   repo root; GitHub Pages picks them up automatically.

### 4. Generate the packaging GPG key

Ed25519, no passphrase, dedicated identity (do NOT reuse a personal key):

```sh
gpg --batch --gen-key <<EOF
Key-Type: EdDSA
Key-Curve: ed25519
Name-Real: LibertAI Packaging
Name-Email: packaging@libertai.io
%no-protection
%commit
EOF

# export — this armored blob becomes APT_GPG_PRIVATE_KEY
gpg --export-secret-keys --armor packaging@libertai.io
```

**Key rotation after compromise**: generate a new key, update
`APT_GPG_PRIVATE_KEY`, and publish a security advisory telling users to
re-run `apt.libertai.io/install.sh` (which will refetch the new public
key). There is no transparent rotation — this is a generic limitation of
APT, not ours.

### 5. Create three GitHub Environments

In `github.com/Libertai/libertai-cli` → Settings → Environments:

| Name | Protection |
| --- | --- |
| `apt` | no approval required (but optional, matches Aleph) |
| `brew` | no approval required (optional) |
| `crates-io-publish` | **Required reviewers** — check the box, add yourself |

The approval gate on `crates-io-publish` is the safety valve: even after
everything else succeeds, a human must click Approve before the crate is
shipped.

### 6. Add repository secrets

Settings → Secrets and variables → Actions:

| Secret | Value | Scope |
| --- | --- | --- |
| `CARGO_REGISTRY_TOKEN` | API token from `crates.io/settings/tokens` | crates-io-publish env |
| `APT_GPG_PRIVATE_KEY` | armored private key from step 4 | apt env |
| `APT_REPO_TOKEN` | classic PAT, `repo` scope, write on `Libertai/apt` | apt env |
| `BREW_REPO_TOKEN` | classic PAT, `repo` scope, write on `Libertai/homebrew-tap` | brew env |

Classic PAT > fine-grained PAT for cross-repo writes from Actions (as of
early 2026 fine-grained still has friction).

Put each secret in the matching Environment, not the repo-wide bucket — that
way the token is only exposed to jobs that actually need it.

---

## First-release playbook (v0.2.0)

Once all prerequisites are done:

1. Bump `version = "0.2.0"` in `Cargo.toml`. Commit, merge to `main`.
2. On `github.com/Libertai/libertai-cli` → Releases → **Draft a new release**:
   - Tag: `v0.2.0` (do not yet push; Draft reserves the tag).
   - Title, release notes. Publish.
   - Alternatively: create the tag locally, push it; then go draft the
     release against the existing tag. Either order works — what matters is
     that a GH Release exists before `build-binaries` runs.
3. Push the tag if you haven't: `git tag v0.2.0 && git push origin v0.2.0`.
4. The workflow fires. Watch Actions tab. Expected timeline:
   - `verify` — ~3 min
   - `check-version`, `check-release` — seconds
   - `build-binaries` (matrix, 4 targets) — ~4 min
   - `publish-apt`, `publish-brew` — ~2 min each, in parallel
   - `request-publish-approval` — **waits for your click**
   - After you approve: `publish-crates` — ~1 min
5. Confirm:
   - GH Release has 4 raw binaries + 4 `.sha256` sidecars + one `.deb`
   - `curl -fsSL https://raw.githubusercontent.com/Libertai/libertai-cli/main/packaging/install.sh | sh` in a clean Docker container
   - `curl -fsSL https://apt.libertai.io/install.sh | sudo bash` in a clean Debian container
   - `brew install Libertai/tap/libertai` on a Mac
   - `cargo install libertai-cli` anywhere with a Rust toolchain

### Dry run before the real thing

Cut `v0.2.0-rc1` first. The workflow treats any version with a `-` as a
prerelease: `publish-apt` and `publish-brew` are **skipped**, but
`build-binaries` and (after approval) `publish-crates` still run. Lets you
exercise the full pipeline without polluting the apt repo or the brew tap.

---

## Recurring release workflow

After v0.2.0 is out, every subsequent release follows the same pattern:

```sh
# bump Cargo.toml version
$EDITOR Cargo.toml          # version = "0.2.1"
git commit -am "release: v0.2.1"
git push

# draft the release on GitHub web UI with notes, then:
git tag v0.2.1 && git push origin v0.2.1

# wait, approve crates-io-publish when prompted
```

`cargo publish` respects `Cargo.lock` via `--locked`, so the CI build is
reproducible against whatever dep versions were in main at tag time.

### If something goes wrong mid-release

- **`check-version` fails**: tag version doesn't match Cargo.toml. Fix
  Cargo.toml, re-commit, delete the tag, re-tag.
- **`check-release` fails**: you didn't draft the GH Release first. Draft
  it (tag is already there), then re-run the failed workflow run.
- **`build-binaries` fails on one target**: rare; usually a transient Rust
  toolchain hiccup on Windows. Just re-run the job.
- **`publish-apt` fails mid-way**: the APT repo may be in a half-updated
  state. Check `Libertai/apt` — if the `.deb` is present but the index
  missing, run `reprepro includedeb stable <.deb>` locally against a
  checkout, push. Next release will self-heal.
- **`publish-crates` fails**: you probably forgot `--locked`, or the
  registry rejected the version (already published, name conflict, etc.).
  Once a version is on crates.io it cannot be deleted — only yanked. Bump
  patch and re-cut if that happens.

---

## Update-check behavior (in-binary)

Every `libertai <command>` (except `login`/`logout`/`config`) performs a
microsecond-scale cache read and, separately, may spawn a background thread
to refresh the cache. The background thread never blocks `main`; the banner
appears on the *next* startup after the cache has been refreshed with a
newer version.

### Skip conditions

The check is bypassed entirely when *any* of these are true:

- `check_for_updates = false` in `config.toml`
- `NO_UPDATE_CHECK` env var is set (any value)
- `CI` env var is set
- stdout is not a terminal (piped, redirected)
- subcommand is `login`, `logout`, or `config`

### Cache file

`~/.config/libertai/update-check.json` (mode 0600, same treatment as
`config.toml`). Schema:

```json
{
  "last_check_unix": 1745000000,
  "latest_version_seen": "0.3.0"
}
```

Delete the file to force a re-check on next startup. It auto-recreates.

### Upgrade-hint routing

The banner points to a different command depending on where the running
binary lives:

| `current_exe()` contains | Hint |
| --- | --- |
| `/.cargo/bin/` | `cargo install libertai-cli --force` |
| `/Cellar/` or `/opt/homebrew/` | `brew upgrade libertai` |
| starts with `/usr/bin/` | `sudo apt upgrade libertai-cli` |
| anything else | release-page URL |

### Version comparison

Hand-rolled semver in `is_strictly_newer`: parses `major.minor.patch[-pre]`,
ignores build metadata. Stable > prerelease for the same base version;
prerelease strings compare lexicographically (`rc2 > rc1`; don't ship `rc10`
or the ordering breaks — bump to the next stable instead).

---

## Deferred / skipped (decide later)

- **Windows code signing** — ~$200/yr EV cert. Without it Windows users get
  a SmartScreen warning on first run. Accept the friction until someone
  complains.
- **Linux aarch64 binary** — `packaging/install.sh` explicitly errors out on
  linux-aarch64 today. Add `aarch64-unknown-linux-gnu` to the
  `build-binaries` matrix when we have real demand.
- **Scoop / WinGet / AUR / nix** — not planned.
- **`libertai update` self-replacing subcommand** — explicit CTO direction:
  no third-party auto-updaters, no in-binary binary-swapping. Use the
  system package manager.
- **SLSA attestations / SBOM / cosign** — Aleph doesn't ship these; match.
- **npm wrapper** (`npx libertai`) — maybe v0.3. Our audience (Claude Code /
  OpenCode / Aider users) lives in JS ecosystems, so `npx` is a natural fit.
- **Dedicated `get.libertai.io`** → GH Pages copy of `install.sh`, for a
  cleaner URL than the raw.githubusercontent one. v0.3 nice-to-have.
- **`release-plz`** for automated version bumping / changelog generation —
  worth adopting once we've shipped 2-3 releases manually and hand-editing
  the changelog feels painful.

---

## Reference: what we borrowed

All of the workflow structure, `packaging/apt/install.sh`, and
`packaging/brew/formula.rb.tmpl` were modeled on
`aleph-im/aleph-rs`'s equivalents. When in doubt about a release-pipeline
pattern, grep their repo first — they've been running this stack longer.
