---
name: release
description: Make a zcode release - bump the version in the code if needed, create the tag, and write the GitHub release text from the commits since the previous tag. Use when the user asks to make/cut a release, for release notes / a changelog / the release description, or to tag a version.
---

# release

Makes a release of a given version: bumps the version in the code when it is outdated, creates the git tag, and writes
short, user-facing release text in markdown, ready to paste into the GitHub release description.

## Steps

1. **Get the tag.** The user must give a version/tag (e.g. `v0.4.0`). If none was input, ask the user which tag to
   use (suggest the next patch after the latest tag) and wait for the answer. Never guess it.
2. **Pull the tags** first so the previous/latest tag is known locally: `git fetch --tags`.
3. **Collect the commits** with one command (no shell variables or pipes needed):
   ```bash
   python /home/alex/dev/zcode/.claude/skills/release/collect_commits.py [TAG]
   ```
   - Tag exists: pass it; commits between the previous `v*` tag and that tag.
   - Tag doesn't exist yet: run without `TAG`; commits since the latest tag up to `HEAD`. Use the given tag for the
     title.
   - Output: range, compare URL, then per commit: hash, subject, body, changed files with +/- counts.
4. **Check the version.** `version` in the root `Cargo.toml`, `crates/*/Cargo.toml` and `Cargo.lock` (the workspace
   packages' entries) must equal the release version (tag without `v`), because `zcode --version` prints it.
   - **Releasing** (the tag doesn't exist yet): check the working tree. If it differs, **bump it**: edit those files
     (refresh `Cargo.lock` with `cargo update --workspace --offline` or `cargo check --offline`), then commit with
     `git add` of only those files (the working tree may hold unrelated changes) and message
     `Bump version to X.Y.Z`. Per the project commit rules: no body, no attribution, don't push.
   - **Already released** (tag exists): check the tag's files, e.g. `git show vX.Y.Z:Cargo.toml`. If it differs, the
     published binaries report the wrong version and can't be fixed; say so and that the next release must carry the
     right one. Don't bump, don't re-tag.
5. **Understand vague subjects.** Commit subjects are short (under 40 chars) on purpose. When a subject doesn't say
   what a user would notice, look at the files list and, if still unclear, `git show <hash>` for that commit only.
6. **Write the notes** (rules below), save them to a fresh temp file `release-notes.md` (e.g. via `mktemp -d`);
   don't put it in the repo. Leave the version bump commit out of the notes.
7. **Create the tag** only if it doesn't exist: `git tag vX.Y.Z` on the commit that carries the right version
   (the bump commit, or `HEAD` if no bump was needed; lightweight, no message). Never move, delete or re-create an
   existing tag. Don't push it unless the user asks.
8. **Answer** with the markdown in a fenced ```` ```markdown ```` block, then say whether the version was bumped and
   the tag created (or why not), then the file as a clickable markdown link
   `[release-notes.md](/abs/path/release-notes.md)` (never in backticks). One plain line outside the block if you
   skipped or merged anything noteworthy.

## Writing rules
- Audience: people who *use* zcode, not people who read the diff. Say what changed for them, in plain words.
- Structure (omit empty sections):
  ```markdown
  ## v0.4.0

  One sentence: the main thing in this release.

  ### New
  - ...
  ### Fixed
  - ...
  ### Docs and internal
  - ...

  **Full changelog**: <compare URL>
  ```
- One line per item, under ~100 chars, starting with a verb or the thing affected. No commit hashes, no file names,
  no PR/issue numbers unless they appear in a commit.
- Merge commits about the same change into one line; drop pure noise (typo fixes, "Add commit rules", CI tweaks)
  unless users are affected. Put docs, bench and tooling changes in "Docs and internal", one line max each.
- Mark breaking changes first, bold, under a `### Breaking` section, with what to do about it.
- Don't invent features: every line must trace to a commit. If a commit is unclear after step 2, describe it
  conservatively or leave it out and say so.
- Keep the "Full changelog" compare link from the script output.
- Benchmarks: don't include any. If the user wants them in the same release text, run the `release-bench` skill and
  append its markdown after this text.
- Don't edit the release on GitHub (`gh release edit`, API) unless the user asks.
