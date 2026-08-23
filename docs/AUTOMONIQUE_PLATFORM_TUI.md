# Automonique platform TUI

The maintained fork ships a provider-free operator cockpit for
`automonique.platform/v1`. It never initializes a model provider in managed
mode. State and mutations travel only through the authenticated platform
client, and every mutation supported by platform v1 returns or reconciles a
durable receipt.

Launch it directly:

```console
jcode platform
```

The Automonique distribution exposes the same client as:

```console
automonique tui
```

Use `--socket PATH` for an explicit local admin socket and `--json` for a
bounded, non-interactive snapshot. Remote operation is SSH to the host; the
admin socket is not exposed as a TUI-specific TCP service.

## Operator surface

The cockpit provides authority-qualified overview, run, session, approval,
model, failure and receipt views. Session observation is independent from the
short exclusive controller lease. Attaching, detaching, claiming control and
releasing control never start, stop or implicitly mutate a provider session.

Attached sessions can be displayed as a grid, rows, columns, tabs or one
focused pane. Layout and observer session IDs are stored locally with
owner-only permissions. Restoring a layout skips missing sessions and never
restores controller authority.

The action palette is generated from the server's exact authority-qualified
action-registry resources; the presence of the generic `execute` method never
enables an inferred mutation. Publishing the registry through ordinary v1
resources preserves compatibility with strict existing clients. New requests,
follow-ups and lease-authorized active-turn steering use an explicit, bounded
composer and enter Automonique's durable intake against the exact target
revision. Steering is offered only from the action palette when the focused
pane holds a current exclusive lease; its target is the lease identity and
revision, not a session inferred from pane focus. Start, cancel, request,
follow-up, steer and approval decisions always show the exact authority/kind/ID,
target revision and consequence before submission. A
disconnect or event gap makes the client visibly read-only until an
authoritative snapshot and any ambiguous receipt have been reconciled.

## Keys

- `1`–`7`: switch views
- arrows: select a durable resource
- `a` / `d`: attach or detach an observer pane
- `c` / `x`: claim or release control
- `i`: compose an explicit node request or selected-session follow-up
- `[` / `]`: focus another pane
- `Shift+Left` / `Shift+Right`: reorder the focused pane
- `Shift+P`: pin or unpin the focused pane in the saved workspace
- `l`: cycle pane layout
- `p`: open capability-driven actions, including active-turn steering when the focused lease permits it
- `r`: force authoritative resynchronization
- `h`: toggle high contrast
- `?`: help
- `q`: detach, release this client's leases and exit

## Verification

The managed client is covered by pure reducer tests for duplicates, gaps,
reordering, snapshot replacement, bounded per-pane buffers and reconnect lease
invalidation; fixed-size wide, narrow, monochrome-readable and high-contrast
render tests; fake-backend lifecycle and lease-loss steering tests; and a real PTY test that launches
the binary, exercises input, and verifies alternate-screen restoration.
