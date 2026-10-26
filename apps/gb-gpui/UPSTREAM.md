# Upstream GitButler pin

Pinned tag: `release/0.22.3` (`0ba35dc`, 2026-08-29).

That tag is the same build as the installed `/Applications/GitButler.app` 0.22.3
(`CFBundleShortVersionString` 0.22.3, bundle id `com.gitbutler.app`). gb-gpui
reads that app's data. It does not run a newer migration than the installed app.

`CHANNEL=release` in `.cargo/config.toml` makes `but-path` use `com.gitbutler.app`.

## Update procedure

1. Update the installed GitButler.app first.
2. Read its version: `plutil -extract CFBundleShortVersionString raw /Applications/GitButler.app/Contents/Info.plist`.
3. Bump the git tag on every GitButler dependency in `Cargo.toml` to `release/<version>`.
4. Re-seed `Cargo.lock` from upstream's lock at that tag:
   `git show release/X:Cargo.lock` in the GitButler clone
5. Copy `[patch.crates-io]` (gix and git2) from that tag's `Cargo.toml`.
6. `cargo check` in `apps/gb-gpui`.
7. `cargo test -p gb-bridge`.
8. Fix only `gb-bridge` (and `gb-gpui` if a type moved). Do not fork GitButler.

## Local override

To compile against a local tree, check out the **same tag** in a separate
directory. Do not point the patch at the fork clone (that
clone stays on master and is shared). Uncomment the example in
`.cargo/config.toml` and set the paths to that separate checkout.
