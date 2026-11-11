---
name: release-notes
description: Write the GitHub release text for a zcode version from its commits (since the previous tag). Use when the user asks for release notes / a changelog / the release description, before or after tagging a release.
---

# release-notes

Turns the commits of a release into short, user-facing release text in markdown, ready to paste into the GitHub
release description.

## Steps

1. **Pull the tags** first so the previous/latest tag is known locally: `git fetch --tags`.
2. **Collect the commits** with one command (no shell variables or pipes needed):
   ```bash
   python /home/alex/dev/zcode/.claude/skills/release-notes/collect_commits.py [TAG]
   ```
   - `TAG` (e.g. `v0.4.0`): commits between the previous `v*` tag and that tag.
   - No `TAG`: commits since the latest tag up to `HEAD` (release not tagged yet). Use the version the user names for
     the title, else say "Unreleased".
   - Output: range, compare URL, then per commit: hash, subject, body, changed files with +/- counts.
3. **Check the version.** `version` in the root `Cargo.toml`, `crates/*/Cargo.toml` and `Cargo.lock` must equal the
   release version (tag without `v`), because `zcode --version` prints it. Which tree to check depends on the case:
   - **Releasing** (no `TAG`, or the tag doesn't exist yet): check the working tree. If it differs, tell the user in
     the answer that the version must be bumped before the tag is made.
   - **Already released** (tag exists): check the tag's files, e.g. `git show vX.Y.Z:Cargo.toml`. If it differs, the
     published binaries report the wrong version and can't be fixed; say so and that the next release must carry the
     right one. Don't suggest re-tagging.
   Don't edit the version unless asked.
4. **Understand vague subjects.** Commit subjects are short (under 40 chars) on purpose. When a subject doesn't say
   what a user would notice, look at the files list and, if still unclear, `git show <hash>` for that commit only.
5. **Write the notes** (rules below), save them to a fresh temp file `release-notes.md` (e.g. via `mktemp -d`), and
   don't touch the repo.
6. **Answer** with the markdown in a fenced ```` ```markdown ```` block, then the file as a clickable markdown link
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
