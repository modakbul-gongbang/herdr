# Safe rollout plan for the ambient reader change

Status: change is implemented, tested, and proven against isolated dev instances
(local and on `mini` over SSH). Not yet applied to any installed/running server.

## What changes for an installed server

The new `experimental.ambient_reader` config flag defaults to `false`, so an
installed server that upgrades to this binary with an unmodified `config.toml`
has zero behavior change: `ambient_for_pane` short-circuits on
`self.state.ambient_reader_enabled` before touching any process or file.

## Restart requirement

`ambient_reader_enabled` is read once in `App::new` from `config.experimental.ambient_reader`
and is deliberately excluded from the live config-reload path
(`src/app/mod.rs`, ~line 1524). Toggling the flag in `config.toml` while a
server is running has no effect until that server process restarts. This
matches the flag's own doc comment in `src/config/model.rs`.

## Rollout steps (for a human to execute, not to be run unattended)

1. Build the release binary from this branch: `ZIG=/opt/homebrew/opt/zig@0.15/bin/zig cargo build --release`.
2. Stop the target's installed server with its own normal path
   (`herdr server stop`, or the platform service manager if one wraps it),
   confirming no unrelated in-flight work is lost — check
   `~/.config/herdr/session.json` timestamp and any attached panes first.
3. Replace the installed binary (`~/.local/bin/herdr` locally, or the
   equivalent path on `mini`) with the new build.
4. Restart the server via its normal path. `ambient_reader` stays `false`
   (unset) at this point — this step alone is a no-op behavior change,
   verified by the full test suite (3350/3351 passing, one pre-existing
   unrelated flaky test) and by running the previous binary's exact
   `session.snapshot` shape unchanged for any pane without the flag on.
5. Only after step 4 is confirmed stable, opt in per-host by adding
   `[experimental]\nambient_reader = true` to that host's real
   `~/.config/herdr/config.toml`, then restart the server once more (per the
   restart requirement above).
6. Verify with `session.snapshot` that panes running Claude/Codex sessions
   whose pane cwd matches their session's original creation cwd now carry a
   populated `ambient` field, and that everything else is unaffected.

## Rollback

Reverting is symmetric: remove/`false` the config flag and restart, or
redeploy the previous binary. No persisted state, migration, or schema
change is involved — the reader is stateless and reads nothing but existing
session files on disk.

## Explicit non-actions

This plan does not authorize actually stopping or replacing
`/Users/hoyeonlee/.local/bin/herdr` (locally) or the installed server on
`mini` (`/Users/grab/.local/bin/herdr`, PID 12671 as of the proof run). Both
were left running and unmodified throughout implementation and proof. Steps
2-5 above require explicit human execution and confirmation, not automated
follow-through.
