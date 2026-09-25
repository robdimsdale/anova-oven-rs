# Monitoring: metrics from the server and the panels

The system has grown past the point where a probe on the bench and a `curl` to
`/health` answer "is it working?". There are two `/health` endpoints, a watchdog,
a persist-region breadcrumb ring and a reset-history buffer — all of which record
*that* something went wrong but none of which record *when*, *how often*, or
*what the trend was beforehand*.

**Both components expose Prometheus-format metrics on `/metrics`, and something
scrapes them.** This document covers why that shape, and what each component
emits. Where the scraper runs and how it is configured is out of scope.

## Why scrape and not push

This is the load-bearing decision, and the codebase has effectively already made
it. The firmware runs picoserve serving `/health` on port 80
(`crates/anova-oven-pico/src/health.rs`); the server serves `/health` off
`Liveness` (`processors/http.rs:49`, `liveness.rs`). Both are already scrape
targets. A `/metrics` route is a second handler over data that is already
collected.

The reason pull beats push here is not convenience:

**The scrape failing is the signal.** A Prometheus-compatible scraper synthesizes
`up{instance="panel-kitchen"} 0` when a target does not answer. Under push, a
panel that is wedged or has been power-cycled simply goes silent, and silence is
indistinguishable from "nothing to report" — which is *exactly* the failure class
the watchdog, the persist ring and `classify_reset` exist to catch. Push would
leave the most important failure mode unobservable.

**A push agent needs state the panel does not have.** Collector address,
credentials, and a buffer to survive reboots. The persist region is a 512-byte
panic message and an 8-entry ring (`persist_data.rs`: `MSG_BUF_SIZE`,
`RING_SIZE`) — there is nowhere to queue samples across a reset. Scraping moves
all of that into one config file on the server.

**Push has an ordering hazard the pull model does not.** A pushing device has to
timestamp its own samples, and the RP2040 has no wall clock at all — see "Do not
emit timestamps".

## Why the Prometheus exposition format

Two independent reasons, and the first is a hard constraint.

**OTLP walks into the wall that created this architecture.** Honeycomb, Datadog
and every OpenTelemetry-native backend want OTLP: protobuf over gRPC or
HTTP, with TLS. Per `docs/architecture.md`, the entire reason
`anova-oven-server` exists is that `devices.anovaculinary.io` is TLS 1.2-only
while `embedded-tls` is TLS 1.3-only, making a self-contained Pico W
impossible. Putting a TLS-requiring exporter back on the RP2040 re-imports that
problem for no gain. By contrast the Prometheus text format is:

```
pico_free_heap_bytes 20480
pico_heartbeat_total{task="api"} 918334
```

— renderable into a fixed `[u8; N]` with `core::fmt::Write`, no allocator, no
TLS, over the plain HTTP the firmware already speaks. It preserves the property
`health.rs` deliberately holds: **no alloc on the response path, so it keeps
working under heap pressure**, which is precisely when the numbers matter.

**It is the lingua franca of home automation.** Home Assistant ships a native
`prometheus:` exporter and ESPHome has a Prometheus component, so the store the
oven feeds can take the rest of the house with no new infrastructure. It also
keeps the store replaceable: anything that scrapes this format and speaks PromQL
will do, so choosing the format matters more than choosing the store.

## Phase 1 — `/metrics` on the server

Smallest useful slice, and it can ship alone.

Add `metrics` + `metrics-exporter-prometheus` to
`crates/anova-oven-server/Cargo.toml`. Prefer these over `axum-prometheus`: the
latter couples to an axum major version (verify 0.8 support before considering
it), while `metrics` is transport-agnostic — install a
`PrometheusBuilder` recorder in `main.rs`, keep its handle in `HttpState`, and
add one route to the existing router (`processors/http.rs:32`):

```rust
.route("/metrics", routing::get(handle_metrics))
```

Metrics to emit, all from data the server already has:

| Metric | Type | Source |
| --- | --- | --- |
| `anova_upstream_connected` | gauge 0/1 | `Liveness::connected()` |
| `anova_upstream_last_state_timestamp_seconds` | gauge | `Liveness.last_state_ms` |
| `anova_ws_frames_total{kind}` | counter | `processors/ws.rs` frame handling |
| `anova_ws_reconnects_total` | counter | `ws.rs` reconnect path |
| `anova_firestore_requests_total{op,outcome}` | counter | `processors/firestore.rs` |
| `anova_firestore_request_duration_seconds{op}` | histogram | ditto |
| `anova_http_requests_total{route,status}` | counter | axum middleware |
| `anova_process_start_time_seconds` | gauge | set once at startup |
| `anova_oven_temp_celsius{bulb}` | gauge | `OvenStatus` |
| `anova_oven_element_watts{element}` | gauge | `OvenStatus` |
| `anova_oven_cook_active`, `anova_oven_door_open`, `anova_oven_water_tank_empty` | gauge 0/1 | `OvenStatus` |

Two notes on this table.

**Emit an absolute timestamp, not an age.** `LivenessSnapshot` exposes
`seconds_since_last_state` because JSON is read by a human at a known instant. A
metric must not do that: an age computed at scrape time goes stale in the store,
so a scraper that stops scraping leaves a frozen "12 seconds" on the dashboard
forever. Publish the Unix timestamp and let PromQL compute
`time() - anova_upstream_last_state_timestamp_seconds`. The JSON field stays as
it is — `/health` and `/metrics` are allowed to differ here, and this is the one
place they should.

**`anova_process_start_time_seconds` is how the supervisor becomes visible.**
`main.rs` deliberately calls `std::process::exit(1)` when a processor dies so
systemd restarts the whole process (`architecture.md`, "Run under systemd with
`Restart=always`"). That is self-healing by design and therefore completely
silent today. `changes(anova_process_start_time_seconds[1h])` turns it into a
number.

The oven-domain metrics in the last three rows are the ones that will actually
get looked at: a temperature chart of every cook, for free, and the seed of the
wider home-automation store.

## Phase 2 — `/metrics` on the panel

Add a second route to `health.rs`'s `AppBuilder::build_app`. The handler renders
`persist::read_live()` — the same `Snapshot` `/health` already serves — into a
stack buffer using `core::fmt::Write`, and returns it as `text/plain`. No new
data collection, no allocator.

| Metric | Type | `Snapshot` field |
| --- | --- | --- |
| `pico_uptime_seconds` | gauge | `uptime_secs` |
| `pico_free_heap_bytes` | gauge | `last_free_heap` |
| `pico_heartbeat_total{task="api\|display\|watchdog"}` | counter | `heartbeats` |
| `pico_reset_count` | counter | `reset_count` |
| `pico_panic_count` | counter | `panic_count` |
| `pico_api_fail_count` | gauge | `last_api_fail_count` |
| `pico_network_up` | gauge 0/1 | `network_up` |
| `pico_persist_magic_valid` | gauge 0/1 | `magic_valid` |
| `pico_app_state{name}` | gauge | `last_app_state` |
| `pico_reset_reason{reason}` | gauge | `reset_reason` |
| `up` | gauge 0/1 | synthesized by the scraper |

Two of these carry most of the value:

**`pico_free_heap_bytes` turns a leak into a line.** The heap is 32 KB
(`main.rs:156`), and `record_free_heap` is called from the watchdog feeder
(`main.rs:147`), so the value is already sampled ~1 Hz — it just has nowhere to
go. Slow fragmentation across days is the classic embedded failure, and today it
is only visible as the *next* reboot's ring entry. As a time series it is visible
a week early.

**Per-task heartbeat counters localize a stall without a probe.** `api`,
`display` and `watchdog` advance independently. One flat while the others climb
identifies the wedged task from the sofa. This is the single thing that is
impossible today without plugging in a debug probe.

### Counter semantics, deliberately

`reset_count` and `panic_count` live in the persist region, so they survive warm
resets and zero only on power loss. `heartbeats` zero on every boot. Both are
correct Prometheus counters: `rate()` and `increase()` detect and handle counter
resets natively, so `increase(pico_reset_count[10m])` is a valid reboot-loop
alert whichever way the counter behaved.

`pico_api_fail_count` is a **gauge**, not a counter — it is
`last_api_fail_count`, a consecutive-failure counter reset to zero on any
success (`api_client.rs:168`) and compared against `fsm::OFFLINE_THRESHOLD` to
decide `ServerOffline`. Labelling it `_total` would invite `rate()` and produce
nonsense.

`pico_app_state` and `pico_reset_reason` follow the info-metric pattern: value
`1` with the name in a label. Bounded label sets — `app_state_name` and
`ResetReason::name` are both closed enums, and `AppStateLabel::from_discriminant`
already falls back to `"Unknown"` rather than emitting an arbitrary integer.

### Do not emit timestamps

The exposition format permits an optional per-sample timestamp. **The firmware
must never use it.** The RP2040 has no RTC and no NTP; `Instant::now()` counts
from boot. Emitting a boot-relative timestamp as if it were wall-clock would
write samples dated 1970 and either be rejected or corrupt the series. Omit
timestamps and the scraper assigns its own, which is correct and is the normal
case. `pico_uptime_seconds` carries the device's notion of time as *data*, where
it belongs.

This is also the ordering hazard mentioned earlier: a push-based device would
have to timestamp its own samples, and this device cannot. The device with no
clock whatsoever is the one that cannot get timestamps wrong.

### Being scraped

The firmware's HTTP server was built for occasional interactive use, and a
scraper is new *steady* load. `health.rs` sets `WEB_TASK_POOL_SIZE = 1` and
`close_connection_after_response()`, and its own comments note that watchdog
isolation is the firmware's responsibility to enforce. Concretely:

- **Expect a scrape every 30 s with a 10 s timeout.** The underlying data changes
  at ~1 Hz at best and most of it is a slow trend. With a connection pool of one,
  a scrape that overlaps an interactive `curl` to `/health` queues at the TCP
  accept layer; the generous timeout keeps that from becoming `up` flapping.
- **Keep the handler on the no-alloc path.** If `/metrics` allocates, the scrape
  becomes a source of heap pressure in exactly the situation it exists to
  observe. Render into a stack buffer.
- **Size the buffer for the worst case and truncate deterministically.** The
  metric set is fixed and known, so compute the bound once rather than
  discovering it when a label pushes past the buffer.
- **No auth, plain HTTP.** Same threat model as the rest of the firmware, stated
  explicitly in `health.rs`: it sits behind the house wifi. It does mean the
  panel's metrics are readable by anything on the LAN.

The panels get their addresses from DHCP (`embassy-net` with `dhcpv4`), so each
needs a static reservation at the router before it can be a scrape target.

One honest cost: this adds back a share of the quiescent network chatter that
`docs/pico-server-transport.md` set out to reduce. At one scrape per 30 s against
an existing 1 Hz poll it is roughly 3% more connections, and unlike the poll it
comes from a single fixed-interval source, so it is legible in a packet capture
rather than noise. Still a real tension with that document's stated goal, and the
reason the interval is 30 s and not 5 s.

## Cardinality

Not a scaling concern at a few thousand series, but one rule prevents the only
way this goes wrong: **never put a recipe title, recipe ID, cook ID or timestamp
in a label.** Each distinct value creates a permanent new series. Recipe identity
belongs in the metric's *value* space or in an annotation, not a label. Every
label in this document is drawn from a closed enum, and that is deliberate.

## Alerts these metrics exist for

**Not implemented yet.** Alerting (Prometheus rules, Grafana alerts) is skipped
for now; the metrics ship first. This table records the intent for when it is
picked up.

The mapping from metric to the failure it makes visible, each of which is
invisible today.

| Alert | Expression | Currently detected by |
| --- | --- | --- |
| Panel offline | `up{job="pico-panel"} == 0` for 5m | nothing |
| Panel reboot loop | `increase(pico_reset_count[10m]) > 2` | nothing — watchdog recovery is silent |
| Panel panicking | `increase(pico_panic_count[1h]) > 0` | the LCD recovery view, if someone looks |
| Heap trending down | `pico_free_heap_bytes < 8192` | next reboot's ring entry |
| Server stale upstream | `time() - anova_upstream_last_state_timestamp_seconds > 120` | `liveness.rs` was written for this; nothing consumes it |
| Server restart loop | `changes(anova_process_start_time_seconds[30m]) > 3` | nothing |
| Door left open mid-cook | `anova_oven_door_open == 1 and anova_oven_cook_active == 1` for 10m | nothing |
| Water tank empty mid-cook | `anova_oven_water_tank_empty == 1 and anova_oven_cook_active == 1` | nothing |

**The stale-upstream threshold above is wrong for an idle oven.** Anova sends
`EVENT_APO_STATE` roughly every 10 minutes when idle and every ~10 s while
cooking (see `DEFAULT_WS_READ_TIMEOUT_SECS` in `main.rs`), so a 120 s threshold
would fire most of the time the oven is idle. When alerting is picked up, either
tie the threshold to the idle heartbeat, e.g.
`time() - anova_upstream_last_state_timestamp_seconds > 1200` (matching
`ANOVA_WS_READ_TIMEOUT_SECS`), or keep 120 s and scope it to cooks with
`and anova_oven_cook_active == 1`. Both are worth having: the first catches a
dead link at any time, the second catches it quickly when it matters.

The oven-domain ones are what justify the exercise to anyone who does not care
about heap fragmentation. For a cooking appliance the other obvious one is probe
reached target → phone notification.

The panels are also already a distributed monitor of the server:
`fsm::OFFLINE_THRESHOLD` (3 consecutive failed polls, so ~3 s) surfaces
`ServerOffline` on the display. Alerting is for when nobody is in the kitchen.

## Tracing and spans: evaluated and deferred

Recorded because the question is a reasonable one, because the answer is not
simply "no", and because there is one cheap thing worth doing *now* to keep the
door open.

**A metrics store does not do spans.** The Prometheus exposition format has no
concept of a trace. That is not a gap in the choice — metrics and traces are
different data types, and nothing in this document's metric set would be better
expressed as a span.

**Self-hosted trace stores exist if it is ever wanted:** `VictoriaTraces` (a
single binary that accepts OTLP and exposes the Jaeger Query API, so Grafana reads
it as a datasource), Grafana Tempo, or a Jaeger all-in-one.

### The architecture argues *for* tracing

More than it might appear. Consider `POST /start`:

```
http processor ──mpsc + oneshot──▶ state_machine ──channel──▶ firestore
                                         │                        │
                                         │                   HTTPS to Firebase
                                    WebSocket to Anova            │
                                         │                        ▼
                                         ◀────────────────── recipe returned
                                         │
        oneshot reply ◀──────────────────┘
```

Four or more hops across independent tokio tasks, each a channel handoff. And
**`tracing`'s span context does not propagate across an `mpsc::send`** — the
current span is task-local, so the receiving processor starts with no parent
unless the sender explicitly attaches `Span::current()` to the message and the
receiver re-enters it. That is exactly the "IDs passed across async tasks" problem,
and it is the part that would need deliberate design rather than a
`#[instrument]` sprinkle.

### What argues against it, decisively for now

**Volume and shape, not architecture.**

- **There are about two interesting requests per day.** Start a cook, stop a cook.
  Against ~86,400 identical `/status` polls that need no tracing at all. Tracing
  earns its keep answering "why was *this* one of ten million requests slow?" — at
  two per day, the log lines are already legible.
- **Honeycomb's actual killer feature needs a population.** BubbleUp asks "what is
  different about the slow ones?", which requires a distribution to find outliers
  in. Two events a day is not a distribution.
- **The genuinely interesting "trace" in this system is a cook**, which is
  multi-stage and *hours* long. No tracing UI is designed for a four-hour root
  span. That flow is better served by the metrics in Phase 1 plus a Grafana
  annotation per cook — which is on the table anyway.
- **The panel cannot participate.** OTLP means protobuf plus TLS, the same wall
  that produced this architecture. A trace could not start at the device.
- **The server currently has zero spans.** Checked: 79 event macros
  (`warn!`/`info!`/`debug!`/`trace!`/`error!`) and **no** `#[instrument]`, `span!`
  or `Span::` anywhere in `crates/anova-oven-server/src/`. `tracing` is being used
  purely as a structured logger. So this is real instrumentation work, not a
  config change — which is the practical argument that settles it.

### The cheap thing to do now

When Phase 1 installs the metrics recorder in `main.rs`, build the subscriber as a
**`tracing_subscriber::registry()` with a `fmt` layer**, rather than the current
`fmt`-only setup. Behaviour is identical today, but a registry is what lets a
`tracing-opentelemetry` layer be *added* later instead of requiring the subscriber
to be rebuilt. One line of foresight, no cost.

### If it is ever picked up

- **Scope it to commands, not polls.** Instrument the `StateMachineCommand` paths
  and the Firestore/WebSocket hops; leave `/status` alone. That is where the fan-out
  and the latency actually live.
- **Let the panel originate a trace ID without exporting spans.** A W3C
  `traceparent` header is just hex text — the firmware can build one from a
  boot-time seed plus a counter and set it on its `POST /start`, with no OTLP, no
  TLS and no allocator. The server then continues that trace. This yields "user
  turned the encoder → panel posted start → server → Firestore → Anova → oven
  acknowledged" as one connected trace, which is the one flow in this system with
  real async fan-out and a genuinely satisfying waterfall.
- **Honeycomb's free tier is viable for this specific purpose.** It is the wrong
  metrics backend, and unreachable for device-originated telemetry. But the
  *server* is a full Linux host with TLS and can export OTLP without difficulty,
  and 20M events/month is orders of magnitude more than this system would ever
  produce. If the goal is specifically the Honeycomb *querying experience*, no
  self-hosted tool replicates it, and sending server-side traces there while
  metrics stay local is a coherent split.

**Verdict: defer.** Revisit if the server grows genuinely concurrent user-facing
work — several panels issuing commands, a phone app, or an automation engine
firing overlapping actions. At that point the fan-out becomes real and the
population becomes large enough for outlier-hunting to mean something. Not before.

## Order of work

1. **Phase 1** — server `/metrics`. Pure addition, no firmware risk, immediately
   gives the oven temperature charts. Scraped over localhost.
2. **Phase 2** — panel `/metrics`. Needs a firmware flash, so it wants the
   collector already proven against the server first.

## Implementation notes

Both phases are implemented; alerting is not (see "Alerts these metrics exist
for"). Choices the tables above left open:

- **Server** (`crates/anova-oven-server/src/telemetry.rs`). Label sets:
  `anova_oven_temp_celsius{bulb}` is `dry`, `dry_top`, `dry_bottom`, `wet`,
  `probe`, `boiler`, `evaporator`; `anova_oven_element_watts{element}` is `top`,
  `rear`, `bottom`, `boiler`, `evaporator`; `anova_ws_frames_total{kind}` is
  `state`, `wifi_list`, `response`, `other`, `parse_error`, `ping`, `pong`,
  `close`; `anova_firestore_*{op}` is one per `FirestoreCommand`, with
  `outcome` of `ok`, `timeout`, `unauthorized` or `error`.
  `anova_http_requests_total{route}` is the matched route pattern, or
  `unmatched` for 404s, so path scanning cannot grow the series count.
- The probe temperature is `NaN` while the probe is unplugged, so a chart shows
  a gap rather than a drop to zero.
- `anova_upstream_last_state_timestamp_seconds` is absent until the first
  oven-state frame since startup, rather than `0`.
- `anova_ws_reconnects_total` counts every time a connection ends and a
  reconnect is scheduled, including failed connects during an outage.
- `anova_oven_cook_active` is `mode != "idle"`, so it includes preheat.
- The subscriber was already a `tracing_subscriber::registry()` with a `fmt`
  layer, so "The cheap thing to do now" needed no change.
- **Panel** (`crates/anova-oven-pico-core/src/metrics.rs`, served from
  `health.rs`). Rendering lives in `pico-core` so the size bound is a host
  test: the worst case is about 1.4 KiB against a 2 KiB buffer, and the test
  fails if that headroom drops below 25%.

## Open questions

- **Does the server also expose a per-panel view?** The server sees each panel's
  polling; a `anova_panel_requests_total{panel}` would give a second, independent
  witness to a panel going quiet. Attractive, but it needs the panel to identify
  itself in its requests, which it does not do today. Deferred.
