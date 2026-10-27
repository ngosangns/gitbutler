---
name: gb-gpui-dev
description: Drive the gb-gpui Dev window over its control socket. Use when checking gb-gpui settings, shortcuts, branches, history, or fake data, or when taking an offscreen screenshot of that window.
---

# gb-gpui-dev

The window is `apps/gb-gpui` in `~/Github/gitbutlerapp/gitbutler` (fork `ngosangns/gitbutler`). Cargo runs from that directory. The control socket exists only in a build with `--features devtools`.

`ns-dev gb-gpui` talks to the socket. `ns-dev gb-gpui launch` opens `~/Applications/gb-gpui Dev.app`. Do not use that launch for a temporary bundle.

## Safety

Never set `GB_GPUI_LIVE` against a real checkout or the installed GitButler data directory. Fake mode is `GB_GPUI_FAKE=1`.

Do not start forge sign-in or any OAuth flow. This build has no forge connection. The settings screen says sign-in is not available. Do not type tokens into the socket.

Do not read or write:

- `~/Library/Application Support/com.gitbutler.app*`
- `~/Library/Application Support/gitbutler`
- a real repository's `.git/gitbutler`
- the keychain

A live settings test sets `E2E_TEST_APP_DATA_DIR` to a fresh temp directory before any `but_path` call, and aborts if the resolved app data or config directory is outside that temp directory.

`ns-dev` does not raise the window or send OS input. `shot` is an offscreen render. `open -g -j` and `NS_DEV_QUIET=1` keep the window from taking focus.

If `gb-gpui.sock` answers `ping`, it belongs to another process. Do not delete it and do not quit that process. A socket file that refuses the connection, with no `gb-gpui` process holding it, is leftover. The next devtools launch unlinks that file and binds.

## Fake and live

| Mode | How | What it touches |
| --- | --- | --- |
| Fake | `GB_GPUI_FAKE=1` | Memory only. Demo project, canned reviews, in-memory settings. |
| Live | no `GB_GPUI_FAKE` | The app data directory selected by `CHANNEL` and `E2E_TEST_APP_DATA_DIR`. |

`require_preview` refuses a write command unless the process was started with `GB_GPUI_FAKE`. Those commands return `This command writes changes and only runs in the preview.`

Release packaging (`apps/gb-gpui/scripts/package-app.sh`) builds without the `devtools` feature. That `.app` has no socket. A screenshot build is a separate `.app` with `--features devtools` and bundle id `com.ngosangns.gb-gpui.dev`.

## Socket

Path: `${TMPDIR:-/tmp}/ns-dev-$(id -u)/gb-gpui.sock`. The app id is the window title `gb-gpui`.

One JSON object per line. `ns-dev gb-gpui <command> key=value` sends `{"cmd":"<command>", ...}`. `ns-dev gb-gpui raw '<json>'` sends the object as given.

`ns-dev gb-gpui help` lists the shared commands only: `ping`, `help`, `screenshot`, `resize`, `scroll`. `GbWindow` does not publish a command spec, so the app commands below are accepted and are not in that list. `cmd` `help` is the shared list. It does not open the Keyboard card. Use `keyboard`.

## Commands

Shared, from ns-ui:

| Command | Arguments | Effect |
| --- | --- | --- |
| `ping` | | Visibility, size, protocol, pid. |
| `help` | | Shared command list. Does not toggle the Keyboard card. |
| `screenshot` | `path`, optional `crop` `[x,y,w,h]`, optional `settle` | Offscreen PNG. `shot` is the ns-dev name. |
| `resize` | `width`, `height` | Window content size in points. This is not the column drag. |
| `scroll` | `to=top\|end` or `y` | Editor column. |

App commands, from `GbWindow::dev` in `crates/gb-gpui/src/main.rs`. Preview means `GB_GPUI_FAKE`.

| Command | Arguments | Preview |
| --- | --- | --- |
| `state` | | No. Projects, selection, stacks, panes, composer, remote, pages, forge, settings, activity. |
| `latency` | `ms` | Yes. Fake delay in milliseconds before each gated bridge call. `0` is instant. The window stays synchronous at `0` and moves the call off the UI thread when `ms` is above `0`. |
| `fail` | `op` | Yes. The next fake call of that operation fails once, before the delay. `op` is a catalog id such as `push` or `fetch`. |
| `select` | `index` or `id` | No. |
| `refresh` | | No. |
| `project-menu` | `action=open\|close` | No. |
| `view` | `mode=list\|tree` | No. |
| `focus` | `branch` or `commit` | No. |
| `pane` | `name=uncommitted\|changes\|branches\|detail` or `projects` | No. |
| `open` | `commit` or `file` | No. |
| `key` | `key` (`up`, `escape`, `cmd-r`, …). `esc` is `escape`. | No. The store applies it. GPUI must accept the token. |
| `trigger-event` | `kind` (default `worktree`) | Preview backend only. |
| `assign` | `path`, optional `lane` | Yes. |
| `commit` | `title`, optional `body`, optional `lane` | Yes. |
| `reword` | `commit`, `message` | Yes. |
| `squash` | optional `subject` and `target` | Yes. |
| `uncommit` | optional `commit` | Yes. |
| `undo` | | Yes. |
| `composer` | `action=open\|close`, optional `title`, `body`, `lane` (`new` names a lane) | No. |
| `columns` | `handle=0\|1` and `delta` (points), optional `width`, or `reset` | No. Drags a column edge. `reset` restores the default split. Fake mode does not write `ui-state.json`. |
| `page` | `name=workspace\|branches\|history` | No. Opens that page and keeps the other page's filter, selection, and scroll. Escape from Branches or History returns to the workspace. |
| `menu` | `file` (context menu; add `hunk` for a hunk), `commit`, `lane`, `branch`, optional `x` and `y`, or `action=close` | No. Right-click and the ⋯ button open the same menu. `m` still opens the lane picker directly. |
| `add` | `path` | Yes. |
| `setup` | `target`, optional `remote` or `push` | Yes. |
| `fetch` | | Yes. |
| `integrate` | `action=open\|apply` | Yes. |
| `push` | `lane`, optional `menu`, optional `force=lease\|force` with `confirm` | Yes. |
| `askpass` | `action=trigger\|answer\|cancel`, `text` on answer | Yes. |
| `branches` | `action=open\|filter\|apply\|unapply\|delete`. `tab=local\|remote\|pr`. `text` or `filter`. `name` on apply. | Yes. |
| `history` | `action=open\|select\|restore\|snapshot`. `id` on select. `message` on snapshot. | Yes. |
| `edit` | `action=commit\|save\|abort`. `commit` on commit. | Yes. |
| `resolve` | `file` and `side=ours\|theirs`, optional `mark` | Yes. |
| `forge` | `action=publish`, `branch` | Yes. Inserts a canned draft. No network. |
| `suggest` | optional `action=replace\|keep` | Yes. Fills the composer from file names. |
| `settings` | `action=open\|close\|app\|project\|save`. Save fields: `metrics`, `errors`, `fetch`, `title`, `shadow`. | Yes. |
| `keyboard` | `action=open\|close` | No. Opens or closes the shortcut card. |
| `perf` | `label=before\|after` (default `after`), `scenario` (`all` or one of `idle`, `scroll-long-diff`, `long-file-list`, `long-history`, `watcher-burst`, `column-drag`), optional `full`, or `action=status` | Yes. Fake data only. Starts a frame loop and returns immediately. `label=before` paints every file, branch, and history row and scans every diff line. The loop writes `~/.agent-scratch/gb-gpui/perf-<label>.json`. Poll `perf action=status` until `wrote` is true. `GB_GPUI_PERF=1` starts the same run after the project opens. `GB_GPUI_PERF_LABEL=before` selects the full paint. |

`state` settings fields: `settings_open`, `settings_section` (`app` or `project`), `metrics`, `error_reporting`, `fetch_interval`, `remove_allowed`, `signing`, `settings_loading`, `settings_error`, `help` (Keyboard card open).

`state` also includes `columns` (always 3), `column_widths`, `column_fractions`, `page` (`workspace`, `branches`, or `history`), `branch_scroll`, `history_scroll`, and `menu_open`.

Activity fields: `activity_count` (spinners that have passed the slow start), `activity_line` (the status-strip sentence, or null), `latency_ms`, `loading_ops` (ids still in the loading phase, including the quiet moment before a spinner), `toasts` (messages on screen), and `strip` (the full status-strip text). `GB_GPUI_FAKE_LATENCY_MS` sets the same delay at process start. Fetch and push cannot be cancelled. Askpass still can.

Column widths persist in gb-gpui's own `ui-state.json` (`~/Library/Application Support/gb-gpui` on macOS, or `GB_GPUI_UI_DIR`). Fake mode and tests stay in memory unless that directory is set, and the file is never written under GitButler's app data or a repo `.git/gitbutler`. Double-click a column edge to reset. `resize` still changes the window, not the columns.

## Performance

`perf` measures the fake window. It does not read a real repository. Run it twice on the same build profile and say which profile (`dev` when `debug_assertions` is set, otherwise `release`).

```
ns-dev gb-gpui perf label=before
ns-dev gb-gpui perf action=status
ns-dev gb-gpui perf label=after
```

`render_us` is the time spent inside `render()`. `fps` is completed scenario frames over wall time. A covered window stops the display link, so the run schedules the next draw itself (one millisecond later, 220ms on the first watcher frame so the refresh gate can fire). Compare `render_us` and `painted_rows`. The JSON `headless` object is the same selector benchmark as `cargo test -p gb-gpui --features devtools measure_covers_every_scenario`. `full` (or `label=before`) builds every file, branch-page, and history row and scans every diff line. The normal path virtualizes those lists. A lane with 48 commits or fewer stays a card. A longer lane is a uniform list. Watcher bursts share one refresh gate: 200 events start at most two workspace refreshes. The headless `before_refreshes` / `after_refreshes` fields are that proof.

## Tests

From `apps/gb-gpui` in `~/Github/gitbutlerapp/gitbutler` (fork `ngosangns/gitbutler`), at most `-j 4`:

```
cargo fmt -p gb-bridge -p gb-gpui
cargo clippy -p gb-bridge -p gb-gpui --all-targets --features devtools -- -D warnings
cargo test -p gb-bridge -p gb-gpui -j 4
```

Do not pass `GB_GPUI_LIVE`. Do not run `cargo fmt` from the ns-gpui root.

## Screenshots

1. Stop if `gb-gpui.sock` answers `ping`. A refused connection with no listener is leftover.
2. `cargo build -p gb-gpui --features devtools -j 4` from `apps/gb-gpui` in `~/Github/gitbutlerapp/gitbutler` (fork `ngosangns/gitbutler`).
3. Wrap that binary in a temporary `.app` under `apps/gb-gpui/target`, with bundle id `com.ngosangns.gb-gpui.dev`. Do not copy it to `~/Applications`. Do not run `scripts/install-dev.sh` or `scripts/dev.sh` launch.
4. The socket starts only when the executable path contains ` Dev.app/` or `NS_DEVTOOLS` is set. This temporary bundle is `gb-gpui.app`, so the launch needs the variable: `env -u GB_GPUI_LIVE open -g -j --env GB_GPUI_FAKE=1 --env NS_DEV_QUIET=1 --env NS_DEVTOOLS=1 "<that.app>"`.
5. Wait until the socket answers `ping`. `ns-dev` reports a refused connection as "no socket".
6. `ns-dev gb-gpui resize 1280 860`, then the command, then `ns-dev gb-gpui shot <file.png>`.
7. Quit only that bundle. `osascript` with id `com.ngosangns.gb-gpui.dev`, then kill the process whose path is that bundle's `Contents/MacOS/gb-gpui` if it is still running. Do not kill a different `gb-gpui`.

The picture is the offscreen PNG. It does not include other windows. Quit the dev app when the shots are saved.

## Packaging

`apps/gb-gpui/scripts/package-app.sh` writes `apps/gb-gpui/target/bundle/gb-gpui.app`.

- Bundle id `com.ngosangns.gb-gpui`, name `gb-gpui`, version `0.22.3`.
- Release build without `devtools`. `--debug` uses the debug profile, still without `devtools`. `--check` verifies `Info.plist`, the executable bit, `AppIcon.icns`, `ASKPASS.txt`, and the absence of `ns-dev-`, `NS_DEVTOOLS`, and `devtools: cannot listen`.
- The bridge does not ship an askpass binary. `Resources/ASKPASS.txt` says the runtime stub is `$TMPDIR/gb-bridge-askpass-stub` and that `GITBUTLER_ASKPASS_BIN` is set only when it was unset.
- The script does not install, launch, or sign.
