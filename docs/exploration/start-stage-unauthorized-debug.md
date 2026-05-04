# Start-next-stage: "unauthorized" error investigation

## Problem

When a multi-stage recipe reaches a manual-transition point, the pico/CLI and server
both attempted to advance to the next stage by sending `CMD_APO_START_STAGE` over
the WebSocket. This always resulted in an "unauthorized" error from the oven
backend, even though the command payload appeared identical to what the iOS phone
app sends from the same oven state.

## What we tried

- Sending `CMD_APO_START_STAGE` with the stage id as-is from Firestore.
- Normalizing the stage id for the wire format (e.g. stripping prefixes).
- Delaying `CMD_APO_UPDATE_COOK_STAGES` until after stage-transition confirmation
  (matching iOS timing — the phone sends the update command only *after* the oven
  confirms the transition, not alongside `CMD_APO_START_STAGE`).

None of these resolved the unauthorized error. The oven appears to require some
additional auth context or session state that the phone app holds but that our
server does not replicate.

## Decision

Remove all code that sends `CMD_APO_START_STAGE` — both the user-triggered
start-next-stage path (pico firmware, server HTTP API, CLI) and the
automatic-advance path (server sending `CMD_APO_START_STAGE` for stages where
`user_action_required == false`). Both paths produce the same "unauthorized"
rejection, so neither is kept.

## Consequences of removing the automatic-advance path

Multi-stage cooks where subsequent stages have `user_action_required == false`
(i.e. stages that would normally auto-advance without any phone-app interaction)
**will not advance automatically**. The oven will pause at the end of each stage
and the user must tap "Start next stage" in the phone app for every stage
transition, not just the ones explicitly marked as manual.

This is a functional regression from the original design intent. The root cause
is that `CMD_APO_START_STAGE` is rejected by the oven backend with "unauthorized"
regardless of `user_action_required`. Until a workaround is found (different auth
context, a different command sequence, or insight from traffic analysis), all
stage transitions require the phone app.

## Current behaviour

When the cook tracker detects that a stage is complete, it sets
`next_stage_ready = true` in the `cook_progress` status and broadcasts it to
clients. The pico displays "Next stage ready" and waits. No stage advance of any
kind is attempted by our software; the user must use the phone app to proceed for
every stage transition.
