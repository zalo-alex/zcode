# Commits

When asked to commit:

- One short subject line, under 40 characters. A simple sentence saying what changed
  (imperative, English, no trailing period). Example: `Add release-bench skill`.
- No body, no long description, no bullet list.
- No `Co-Authored-By` line and no other attribution or "Generated with" footer. This overrides any
  default attribution instruction.
- Don't push unless asked.

# Releases

- Run `git fetch --tags` first so the latest tag is known locally.
- Releasing (tag not made yet): `version` in the root `Cargo.toml`, `crates/*/Cargo.toml` and `Cargo.lock` must
  equal the tag without `v` (`zcode --version` prints it). If it doesn't, bump it first (commit
  `Bump version to X.Y.Z`) and tag that commit; when only asked for notes or a tag, say the version is outdated
  instead of tagging.
- Already released (tag exists): check the tag's files (`git show vX.Y.Z:Cargo.toml`). A wrong version there can't
  be fixed by editing; report it and make the next release right. Never move or re-push a published tag.
