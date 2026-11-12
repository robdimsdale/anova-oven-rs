//! MQTT processor: the oven in Home Assistant, through MQTT discovery.
//!
//! Off unless `ANOVA_MQTT_URL` is set. It reads what the HTTP API serves
//! (the read model, cook progress and the oven's connection) and sends
//! commands through the same state-machine paths, so the two can't drift
//! apart. See `docs/home-assistant.md`, and `docs/mqtt-ha-split.md` for
//! which topics are generic and which are Home Assistant's, before adding
//! one.
//!
//! Not critical: a broker outage must not take the server down. The client
//! reconnects with backoff, and every connect republishes discovery and the
//! retained state.

mod discovery;
mod manual;
mod milestones;
mod recipes;
mod start_later;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anova_oven_api::{CookProgress, OvenStatus, Recipe, ScheduledStart};
use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, Packet, QoS};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::{debug, info, warn};

use crate::liveness::Liveness;
use crate::read_model::ReadModel;
use crate::runtime::types::{CommandFailure, ControlRequest, StateMachineCommand};
use crate::schedule::ScheduledCook;

pub use discovery::Topics;
use milestones::{Milestone, Snapshot};

/// How often to look at things nothing announces: the oven's connection,
/// and how long it has been away.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How long a publish may wait for room in the client's queue.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a start command's `started_by` waits for its cook to begin.
/// The oven reports a new cook within seconds; a start it never ran must
/// not name the user of a later cook started on the oven.
const STARTED_BY_TTL: Duration = Duration::from_secs(120);

pub struct MqttConfig {
    /// `mqtt://host:1883`; the port defaults to 1883.
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub topics: Topics,
    /// Local mode: announce the controls only it can drive.
    pub local: bool,
    /// The broker drops one of two clients with the same ID, so a second
    /// server on the same broker (a test run beside the real one) needs
    /// its own.
    pub client_id: String,
    /// Where the oven is, which "Start at 16:30" is read in.
    pub time_zone: Option<jiff::tz::TimeZone>,
}

/// `ANOVA_MQTT_CLIENT_ID`, or `anova-oven-server` when it's unset or blank.
fn client_id(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or("anova-oven-server")
        .to_string()
}

impl MqttConfig {
    /// From `ANOVA_MQTT_*`; `None` when `ANOVA_MQTT_URL` isn't set.
    pub fn from_env(local: bool, time_zone: Option<jiff::tz::TimeZone>) -> Option<Self> {
        let url = std::env::var("ANOVA_MQTT_URL").ok()?;
        Some(Self {
            url,
            username: std::env::var("ANOVA_MQTT_USERNAME").ok(),
            password: std::env::var("ANOVA_MQTT_PASSWORD").ok(),
            topics: Topics {
                prefix: std::env::var("ANOVA_MQTT_TOPIC_PREFIX")
                    .unwrap_or_else(|_| "anova/oven".into()),
                discovery: std::env::var("ANOVA_MQTT_DISCOVERY_PREFIX")
                    .unwrap_or_else(|_| "homeassistant".into()),
            },
            local,
            client_id: client_id(std::env::var("ANOVA_MQTT_CLIENT_ID").ok().as_deref()),
            time_zone,
        })
    }

    fn options(&self) -> Result<MqttOptions, String> {
        let (host, port) = broker_address(&self.url)?;
        let mut options = MqttOptions::new(self.client_id.clone(), host, port);
        if let (Some(user), Some(pass)) = (&self.username, &self.password) {
            options.set_credentials(user, pass);
        }
        options.set_keep_alive(Duration::from_secs(30));
        // Discovery for the recipe select and the selected recipe's steps
        // can outgrow rumqttc's 10 KiB default.
        options.set_max_packet_size(256 * 1024, 256 * 1024);
        options.set_last_will(LastWill::new(
            self.topics.availability(),
            "offline",
            QoS::AtLeastOnce,
            true,
        ));
        Ok(options)
    }
}

/// What the HTTP API serves, for the processor to read.
pub struct Sources {
    pub read_model: watch::Receiver<ReadModel>,
    pub cook_progress: watch::Receiver<Option<CookProgress>>,
    pub liveness: Arc<Liveness>,
    pub failures: broadcast::Receiver<CommandFailure>,
    pub sm_cmd_tx: mpsc::Sender<StateMachineCommand>,
}

/// From the client's event loop to the processor.
enum Incoming {
    Connected,
    Command { topic: String, payload: Vec<u8> },
}

pub async fn run(config: MqttConfig, mut sources: Sources) {
    let options = match config.options() {
        Ok(options) => options,
        Err(e) => {
            warn!(error = %e, "[mqtt] bad ANOVA_MQTT_URL; MQTT is off");
            return;
        }
    };
    let (client, eventloop) = AsyncClient::new(options, 64);
    let (incoming_tx, mut incoming_rx) = mpsc::channel(32);
    tokio::spawn(drive(eventloop, incoming_tx));

    let mut processor = Processor::new(client, config.topics, config.local, config.time_zone);
    let mut poll = tokio::time::interval(POLL_INTERVAL);

    loop {
        tokio::select! {
            incoming = incoming_rx.recv() => match incoming {
                Some(Incoming::Connected) => processor.on_connected(&sources).await,
                Some(Incoming::Command { topic, payload }) => {
                    processor.on_command(&topic, &payload, &sources).await;
                }
                None => return,
            },
            changed = sources.read_model.changed() => {
                if changed.is_err() {
                    return;
                }
                processor.on_change(&sources, false).await;
            }
            changed = sources.cook_progress.changed() => {
                if changed.is_err() {
                    return;
                }
                processor.on_change(&sources, false).await;
            }
            failure = sources.failures.recv() => match failure {
                Ok(failure) => processor.command_failed(&failure.command, &failure.reason).await,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(missed = n, "[mqtt] missed command failures");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = poll.tick() => processor.on_change(&sources, false).await,
        }
    }
}

/// Run the client's event loop, reporting connects and commands. On an
/// error rumqttc reconnects at the next poll, so this only paces it.
async fn drive(mut eventloop: rumqttc::EventLoop, incoming_tx: mpsc::Sender<Incoming>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                info!("[mqtt] connected to the broker");
                backoff = Duration::from_secs(1);
                if incoming_tx.send(Incoming::Connected).await.is_err() {
                    return;
                }
            }
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                let command = Incoming::Command {
                    topic: publish.topic,
                    payload: publish.payload.to_vec(),
                };
                if incoming_tx.send(command).await.is_err() {
                    return;
                }
            }
            Ok(_) => {}
            Err(e) => {
                warn!(error = %e, retry_secs = backoff.as_secs(), "[mqtt] broker connection failed");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

struct Processor {
    client: AsyncClient,
    topics: Topics,
    local: bool,
    time_zone: Option<jiff::tz::TimeZone>,
    recipes: Vec<Recipe>,
    /// The select's options, each with its recipe ID.
    options: Vec<(String, String)>,
    /// The selected recipe's ID. Lives here, not in Home Assistant, so it
    /// survives an HA restart and a stale select can't start the wrong one.
    selected: Option<String>,
    /// The manual cook being set up. Here for the same reasons.
    manual: manual::Draft,
    last_manual: Option<String>,
    /// The values the "Start in" and "Start at" buttons use. Here for the
    /// same reasons.
    start_later: start_later::Draft,
    last_start_later: Option<String>,
    /// `None` until the first snapshot, so a schedule from before a restart
    /// isn't announced again.
    last_scheduled: Option<Option<ScheduledStart>>,
    last_state: Option<String>,
    /// The oven's display unit, from the last status that had one: the
    /// temperature numbers' unit, and the summaries'. Kept while the oven
    /// is away, so their discovery doesn't flip to °C and back.
    unit: Option<String>,
    /// The unit the discovery configs went out in.
    discovered_unit: Option<String>,
    /// `None` until the first snapshot, which is the baseline: a restart
    /// mid-cook announces nothing.
    last_snapshot: Option<Snapshot>,
    next_stage_ready: Option<bool>,
    /// The `started_by` of the last start command, and when it came, until
    /// the cook it started begins (or [`STARTED_BY_TTL`] passes).
    pending_started_by: Option<(String, Instant)>,
    /// Who started the running cook: the `started_by` of its start command,
    /// or `None` (started on the oven, by a schedule, or without one). Every
    /// event of the cook carries it.
    started_by: Option<String>,
}

impl Processor {
    fn new(
        client: AsyncClient,
        topics: Topics,
        local: bool,
        time_zone: Option<jiff::tz::TimeZone>,
    ) -> Self {
        Processor {
            client,
            topics,
            local,
            time_zone,
            recipes: Vec::new(),
            options: Vec::new(),
            selected: None,
            manual: manual::Draft::default(),
            last_manual: None,
            start_later: start_later::Draft::default(),
            last_start_later: None,
            last_scheduled: None,
            last_state: None,
            unit: None,
            discovered_unit: None,
            last_snapshot: None,
            next_stage_ready: None,
            pending_started_by: None,
            started_by: None,
        }
    }

    async fn publish(&self, topic: String, retain: bool, payload: impl Into<Vec<u8>>) {
        publish_within(&self.client, PUBLISH_TIMEOUT, topic, retain, payload).await;
    }

    /// Discovery, availability and every retained topic, then the
    /// subscription. On every connect, since the broker may have lost them.
    async fn on_connected(&mut self, sources: &Sources) {
        let model = sources.read_model.borrow().clone();
        self.recipes = model.recipes;
        self.options = recipes::options(&self.recipes);
        if let Some(status) = &model.status {
            self.unit = Some(status.temperature_unit.clone());
        }
        self.publish_discovery().await;
        self.publish(self.topics.availability(), true, "online")
            .await;
        if let Err(e) = self
            .client
            .subscribe(self.topics.commands(), QoS::AtLeastOnce)
            .await
        {
            warn!(error = %e, "[mqtt] subscribe failed");
        }
        self.publish_recipes().await;
        self.last_state = None;
        self.last_manual = None;
        self.last_start_later = None;
        self.next_stage_ready = None;
        self.on_change(sources, true).await;
    }

    async fn publish_discovery(&mut self) {
        let unit = self.unit();
        for (topic, payload) in discovery::configs(&self.topics, self.local, &self.titles(), &unit)
        {
            self.publish(topic, true, payload).await;
        }
        self.discovered_unit = Some(unit);
    }

    fn titles(&self) -> Vec<String> {
        self.options
            .iter()
            .map(|(title, _)| title.clone())
            .collect()
    }

    /// The recipe list, the select's options and the selected recipe.
    async fn publish_recipes(&mut self) {
        if !self
            .selected
            .as_ref()
            .is_some_and(|id| self.options.iter().any(|(_, o)| o == id))
        {
            self.selected = self.options.first().map(|(_, id)| id.clone());
        }
        let titles = self.titles();
        self.publish(
            self.topics.recipes(),
            true,
            serde_json::to_vec(&titles).unwrap_or_default(),
        )
        .await;
        self.publish_selected().await;
    }

    async fn publish_selected(&self) {
        let unit = self.unit();
        let selected = self.selected.as_ref().and_then(|id| {
            let title = &self.options.iter().find(|(_, o)| o == id)?.0;
            let recipe = self.recipes.iter().find(|r| &r.id == id)?;
            Some((title.clone(), recipes::attributes(recipe, &unit)))
        });
        let (title, attributes) =
            selected.unwrap_or_else(|| ("No recipes".into(), serde_json::json!({})));
        self.publish(self.topics.selected(), true, title).await;
        self.publish(
            self.topics.selected_attributes(),
            true,
            attributes.to_string(),
        )
        .await;
    }

    /// The manual cook's state, if it changed. Its summary is in the
    /// oven's display unit, so a change of unit counts.
    async fn publish_manual(&mut self) {
        let state = self.manual.state(&self.unit()).to_string();
        if self.last_manual.as_deref() != Some(state.as_str()) {
            self.publish(self.topics.manual(), true, state.clone())
                .await;
            self.last_manual = Some(state);
        }
    }

    /// The "Start in" and "Start at" values, if they changed.
    async fn publish_start_later(&mut self) {
        let state = self.start_later.state().to_string();
        if self.last_start_later.as_deref() != Some(state.as_str()) {
            self.publish(self.topics.start_later(), true, state.clone())
                .await;
            self.last_start_later = Some(state);
        }
    }

    fn unit(&self) -> String {
        self.unit.clone().unwrap_or_else(|| "C".into())
    }

    /// Something may have changed: republish what did, and announce any
    /// milestones.
    async fn on_change(&mut self, sources: &Sources, force: bool) {
        let model = sources.read_model.borrow().clone();

        let new_options = recipes::options(&model.recipes);
        if new_options != self.options {
            self.recipes = model.recipes.clone();
            self.options = new_options;
            let (topic, payload) = discovery::select_config(&self.topics, &self.titles());
            self.publish(topic, true, payload).await;
            self.publish_recipes().await;
        } else if model.recipes.len() == self.recipes.len() {
            self.recipes = model.recipes.clone();
        }

        let connected = sources.liveness.connected();
        let status = model.status.clone().map(|mut status| {
            status.cook_progress = sources.cook_progress.borrow().clone();
            status.upstream = Some(anova_oven_api::UpstreamHealth {
                connected,
                disconnected_secs: sources.liveness.disconnected_secs(),
                oven_offline_secs: sources.liveness.oven_offline_secs(),
            });
            status
        });

        if let Some(status) = &status {
            self.unit = Some(status.temperature_unit.clone());
            if self.discovered_unit.is_some() && self.discovered_unit != self.unit {
                self.publish_discovery().await;
            }
        }

        let state = state_json(
            status.as_ref(),
            model.current_cook.as_ref(),
            connected,
            model.scheduled_start.as_ref(),
        )
        .to_string();
        if force || self.last_state.as_deref() != Some(state.as_str()) {
            self.publish(self.topics.state(), true, state.clone()).await;
            self.last_state = Some(state);
        }

        let snapshot = Snapshot::new(
            status.as_ref(),
            model.current_cook.as_ref(),
            connected,
            self.last_snapshot.as_ref(),
        );
        let ready = snapshot.next_stage_ready && snapshot.connected;
        if force || self.next_stage_ready != Some(ready) {
            let availability = if ready { "online" } else { "offline" };
            self.publish(self.topics.next_stage_availability(), true, availability)
                .await;
            self.next_stage_ready = Some(ready);
        }

        // A scheduled cook that fired (or was cancelled) is no one's: a
        // `started_by` still pending must not name its user.
        let scheduled = model.scheduled_start;
        if matches!(&self.last_scheduled, Some(Some(_))) && scheduled.is_none() {
            self.pending_started_by = None;
        }

        let had_baseline = self.last_snapshot.is_some();
        if let Some(prev) = self.last_snapshot.take() {
            for milestone in milestones::milestones(&prev, &snapshot) {
                if milestone == Milestone::CookStarted {
                    self.started_by = self
                        .pending_started_by
                        .take()
                        .filter(|(_, at)| at.elapsed() < STARTED_BY_TTL)
                        .map(|(user, _)| user);
                }
                let source = if milestone.describes_previous() {
                    &prev
                } else {
                    &snapshot
                };
                self.event(milestone, source.payload(milestone.as_str()))
                    .await;
            }
        }
        // The cook's events are out: a later fault on an idle oven is no
        // one's.
        if snapshot.connected && !snapshot.cooking {
            self.started_by = None;
        }
        // The baseline is the oven's first report. Before it, the status
        // says idle, so a restart mid-cook would announce the cook again.
        if had_baseline || (connected && status.is_some()) {
            self.last_snapshot = Some(snapshot);
        }

        // A new or moved scheduled cook. Not its start or cancellation: the
        // cook's own events, or a `command_failed`, say what happened.
        if let Some(prev) = &self.last_scheduled {
            let moved = |s: &&ScheduledStart| {
                prev.as_ref()
                    .is_none_or(|p| (&p.at, &p.recipe_id) != (&s.at, &s.recipe_id))
            };
            if let Some(s) = scheduled.as_ref().filter(moved) {
                info!(event = "cook_scheduled", at = %s.at, "[mqtt] milestone");
                let payload = serde_json::json!({
                    "event_type": "cook_scheduled",
                    "recipe_title": s.title,
                    "recipe_id": s.recipe_id,
                    "at": s.at,
                });
                self.publish(self.topics.event(), false, payload.to_string())
                    .await;
            }
        }
        self.last_scheduled = Some(scheduled);

        self.publish_manual().await;
        self.publish_start_later().await;
    }

    async fn event(&self, milestone: Milestone, mut payload: serde_json::Value) {
        info!(event = milestone.as_str(), "[mqtt] milestone");
        payload["started_by"] = self.started_by.clone().into();
        self.publish(self.topics.event(), false, payload.to_string())
            .await;
    }

    async fn command_failed(&self, command: &str, reason: &str) {
        self.start_failed(command, reason, None).await;
    }

    /// A `command_failed` that names who sent the refused command: the
    /// `started_by` of a start command, never the running cook's.
    async fn start_failed(&self, command: &str, reason: &str, requested_by: Option<&str>) {
        warn!(command, reason, "[mqtt] command failed");
        let payload = failure_payload(self.last_snapshot.as_ref(), command, reason, requested_by);
        self.publish(self.topics.event(), false, payload.to_string())
            .await;
    }

    async fn on_command(&mut self, topic: &str, payload: &[u8], sources: &Sources) {
        let Some(name) = topic
            .strip_prefix(&self.topics.command(""))
            .map(str::to_string)
        else {
            return;
        };
        let text = String::from_utf8_lossy(payload).trim().to_string();
        debug!(command = %name, payload = %text, "[mqtt] command");

        // Kept for the cook only once the state machine takes the start (see
        // below), so a refused start never names its user on a later cook.
        let is_start = name == "start" || name == "start_manual";
        let requested_by = if is_start { started_by(&text) } else { None };

        let (reply_tx, reply_rx) = oneshot::channel();
        let cmd = match name.as_str() {
            "select_recipe" => {
                match self.options.iter().find(|(title, _)| *title == text) {
                    Some((_, id)) => {
                        self.selected = Some(id.clone());
                        self.publish_selected().await;
                    }
                    None => warn!(title = %text, "[mqtt] no recipe with that title"),
                }
                return;
            }
            "start" => {
                // `{"recipe_id": "…"}`, or anything else (a button's
                // `PRESS`) for the selected recipe.
                let recipe_id = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| v.get("recipe_id")?.as_str().map(String::from))
                    .or_else(|| self.selected.clone());
                let Some(recipe_id) = recipe_id else {
                    self.start_failed("start", "no recipe selected", requested_by.as_deref())
                        .await;
                    return;
                };
                StateMachineCommand::StartCook {
                    recipe_id,
                    reply: reply_tx,
                }
            }
            "start_manual" => {
                // `{"title": "…", "stages": [...]}` for any manual cook, or
                // anything else (a button's `PRESS`) for the one set up in
                // the manual entities.
                let recipe = match serde_json::from_str::<ManualStart>(&text) {
                    Ok(start) => manual::check(start.title, start.stages),
                    Err(_) => self.manual.recipe(),
                };
                match recipe {
                    Ok(recipe) => StateMachineCommand::StartManualCook {
                        title: recipe.title,
                        stages: recipe.stages,
                        reply: reply_tx,
                    },
                    Err(e) => {
                        self.start_failed("start_manual", &e, requested_by.as_deref())
                            .await;
                        return;
                    }
                }
            }
            field if field.starts_with("manual/") => {
                if let Err(e) = self.manual.set(&field["manual/".len()..], &text) {
                    self.command_failed(field, &e).await;
                }
                self.publish_manual().await;
                return;
            }
            "start_later/in_minutes" | "start_later/at_time" => {
                let result = if name.ends_with("in_minutes") {
                    self.start_later.set_in_minutes(&text)
                } else {
                    self.start_later.set_at_time(&text)
                };
                if let Err(e) = result {
                    self.command_failed(&name, &e).await;
                }
                // Republished either way, so a refused value snaps back.
                self.last_start_later = None;
                self.publish_start_later().await;
                return;
            }
            "start_in" | "start_at" => {
                self.schedule_start(&name, &text, sources).await;
                return;
            }
            "cancel_scheduled_start" => {
                StateMachineCommand::CancelScheduledStart { reply: reply_tx }
            }
            "start_scheduled_now" => StateMachineCommand::StartScheduledNow { reply: reply_tx },
            "stop" => StateMachineCommand::StopCook { reply: reply_tx },
            other => {
                let request = match control_request(other, &text) {
                    Ok(request) => request,
                    Err(e) => {
                        self.command_failed(other, &e).await;
                        return;
                    }
                };
                StateMachineCommand::Control {
                    request,
                    reply: reply_tx,
                }
            }
        };
        if sources.sm_cmd_tx.send(cmd).await.is_err() {
            return;
        }
        // This task waits for the reply, so `on_change` can't see the new
        // cook before `pending_started_by` is set.
        match reply_rx.await {
            Ok(Ok(())) if is_start => {
                // A start without `started_by` clears it, so an earlier
                // user isn't named.
                self.pending_started_by = requested_by.map(|user| (user, Instant::now()));
            }
            // A scheduled cook started now is no one's.
            Ok(Ok(())) if name == "start_scheduled_now" => self.pending_started_by = None,
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                self.start_failed(&name, &e.to_string(), requested_by.as_deref())
                    .await
            }
            Err(_) => {
                self.start_failed(
                    &name,
                    "state machine dropped the reply",
                    requested_by.as_deref(),
                )
                .await
            }
        }
    }
}

/// The `command_failed` payload: the last snapshot's view of the oven, the
/// command, the reason, and `started_by`. That is the refused start's own
/// `started_by` (`null` for any other command), never the running cook's,
/// so a notification reaches the person whose command failed.
fn failure_payload(
    snapshot: Option<&Snapshot>,
    command: &str,
    reason: &str,
    requested_by: Option<&str>,
) -> serde_json::Value {
    let mut payload = snapshot
        .cloned()
        .unwrap_or_default()
        .payload("command_failed");
    payload["command"] = command.into();
    payload["reason"] = reason.into();
    payload["started_by"] = requested_by.into();
    payload
}

impl Processor {
    /// `cmd/start_in` or `cmd/start_at`: set a recipe to start later.
    async fn schedule_start(&mut self, name: &str, text: &str, sources: &Sources) {
        let request = start_later::Request::parse(text);
        let Some(recipe_id) = request.recipe_id.clone().or_else(|| self.selected.clone()) else {
            self.command_failed(name, "no recipe selected").await;
            return;
        };
        let when = self.start_later.when(name, &request);
        let at = match crate::schedule::resolve(
            &when,
            self.time_zone.as_ref(),
            jiff::Timestamp::now(),
        ) {
            Ok(at) => at,
            Err(e) => {
                self.command_failed(name, &e).await;
                return;
            }
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let cmd = StateMachineCommand::ScheduleStart {
            at,
            cook: ScheduledCook::Recipe { recipe_id },
            replace: request.replace,
            reply: reply_tx,
        };
        if sources.sm_cmd_tx.send(cmd).await.is_err() {
            return;
        }
        match reply_rx.await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => self.command_failed(name, &e.to_string()).await,
            Err(_) => {
                self.command_failed(name, "state machine dropped the reply")
                    .await
            }
        }
    }
}

/// The `started_by` of a start command's JSON: who asked for the cook, such
/// as a Home Assistant user ID. The server only passes it on in the cook's
/// events, so a notification can go to that person. It is not checked
/// against anything and grants nothing. Only a short plain ID is taken
/// (letters, digits, `-`, `_`, `.`, at most 64), since every event repeats
/// it.
fn started_by(text: &str) -> Option<String> {
    let json = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let user = json.get("started_by")?.as_str()?;
    let plain = user
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    (plain && !user.is_empty() && user.len() <= 64).then(|| user.to_string())
}

/// A manual cook given in full, as `POST /start` takes one.
#[derive(serde::Deserialize)]
struct ManualStart {
    #[serde(default)]
    title: Option<String>,
    stages: Vec<anova_oven_api::Stage>,
}

/// Host and port from `mqtt://host[:port]`. TLS (`mqtts://`) isn't supported
/// yet, so the client doesn't bring a second TLS stack.
fn broker_address(url: &str) -> Result<(String, u16), String> {
    let rest = url
        .strip_prefix("mqtt://")
        .or_else(|| url.strip_prefix("tcp://"))
        .ok_or_else(|| format!("`{url}` must start with mqtt:// (mqtts:// isn't supported yet)"))?;
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return Err(format!(
            "`{url}` needs a host, and credentials go in ANOVA_MQTT_USERNAME/PASSWORD"
        ));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => {
            let port = port.parse().map_err(|_| format!("bad port in `{url}`"))?;
            Ok((host.to_string(), port))
        }
        None => Ok((authority.to_string(), 1883)),
    }
}

/// A control command from its topic name and payload.
fn control_request(name: &str, payload: &str) -> Result<ControlRequest, String> {
    match name {
        "next_stage" => Ok(ControlRequest::NextStage),
        "lamp" => match payload.to_ascii_uppercase().as_str() {
            "ON" => Ok(ControlRequest::Lamp { on: true }),
            "OFF" => Ok(ControlRequest::Lamp { on: false }),
            _ => Err(format!("lamp wants ON or OFF, not `{payload}`")),
        },
        "timer" => match payload.parse::<f64>() {
            Ok(secs) if secs >= 1.0 => Ok(ControlRequest::Timer { secs: secs as u64 }),
            _ => Err(format!("timer wants seconds, not `{payload}`")),
        },
        "temperature" => match payload.parse::<f32>() {
            Ok(celsius) if (25.0..=250.0).contains(&celsius) => {
                Ok(ControlRequest::Temperature { celsius })
            }
            _ => Err(format!("temperature wants 25–250 °C, not `{payload}`")),
        },
        other => Err(format!("unknown command `{other}`")),
    }
}

/// Publish, but wait at most `limit` for room in the client's queue.
///
/// While the broker is away, the event loop sends nothing, so the queue
/// fills, and a plain publish would then wait until the broker returns.
/// Every other task of the processor (the oven's state, events, commands)
/// would wait with it, and stale messages would pile up. A publish that
/// times out is dropped: a reconnect publishes every retained topic again
/// (`on_connected`), and an event from the outage is stale anyway.
async fn publish_within(
    client: &AsyncClient,
    limit: Duration,
    topic: String,
    retain: bool,
    payload: impl Into<Vec<u8>>,
) -> bool {
    let send = client.publish(topic.clone(), QoS::AtLeastOnce, retain, payload);
    match tokio::time::timeout(limit, send).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            warn!(topic, error = %e, "[mqtt] publish failed");
            false
        }
        Err(_) => {
            debug!(topic, "[mqtt] publish dropped: the broker is away");
            false
        }
    }
}

/// The state topic: the `/status` JSON plus what Home Assistant can't
/// work out without templating every field: `connected`, `phase`,
/// `recipe_title`, `timer_remaining_secs`, `stage_ends_at` (so the phone
/// counts down without an update every second) and `stage_started_at` (so it
/// can count up through a stage with no timer). Both come from the cook
/// progress (see `cook_progress`), as ISO times, and are `null` without it.
/// `scheduled_start` is there (`null` when there's none) even before the
/// oven first reports.
fn state_json(
    status: Option<&OvenStatus>,
    cook: Option<&anova_oven_api::CurrentCook>,
    connected: bool,
    scheduled: Option<&ScheduledStart>,
) -> serde_json::Value {
    let scheduled = serde_json::to_value(scheduled).unwrap_or_default();
    let Some(status) = status else {
        return serde_json::json!({
            "connected": connected,
            "mode": "unknown",
            "scheduled_start": scheduled,
        });
    };
    let mut state = serde_json::to_value(status).unwrap_or_default();
    let remaining = status.timer_remaining_secs();
    state["connected"] = connected.into();
    state["phase"] = status.phase().into();
    state["recipe_title"] = cook.map(|c| c.display_name().to_string()).into();
    state["scheduled_start"] = scheduled;
    state["timer_remaining_secs"] = remaining.into();
    let iso = |t: Option<i64>| {
        t.and_then(|t| jiff::Timestamp::from_second(t).ok())
            .map(|t| t.to_string())
    };
    // Without the cook's progress (no current cook yet), both are `null`:
    // an end worked out from now would move at every publish.
    let progress = status.cook_progress.as_ref();
    state["stage_ends_at"] = iso(progress.and_then(|p| p.stage_ends_at)).into();
    state["stage_started_at"] = iso(progress.and_then(|p| p.stage_started_at)).into();
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::types::SmError;

    #[tokio::test]
    async fn a_publish_gives_up_when_the_queue_stays_full() {
        // An event loop that never runs, as while the broker is away: the
        // queue holds one message, and nothing takes it out.
        let (client, _eventloop) = AsyncClient::new(MqttOptions::new("test", "localhost", 1883), 1);
        let limit = Duration::from_millis(50);
        assert!(publish_within(&client, limit, "a".into(), false, "1").await);
        let started = std::time::Instant::now();
        assert!(!publish_within(&client, limit, "a".into(), false, "2").await);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it must not wait for the broker"
        );
    }

    #[test]
    fn the_state_carries_both_times_from_the_progress() {
        let mut status = crate::cook_progress::tests::base_status();
        status.cook_progress = Some(
            serde_json::from_value(serde_json::json!({
                "recipe_title": "Bread", "current_stage_index": 0, "total_stage_count": 1,
                "current_stage_description": "200 °C", "current_stage_kind": "cook",
                "next_stage_ready": false, "stage_started_at": 400, "stage_ends_at": 1600,
            }))
            .unwrap(),
        );
        let v = state_json(Some(&status), None, true, None);
        assert_eq!(v["stage_ends_at"], "1970-01-01T00:26:40Z");
        assert_eq!(v["stage_started_at"], "1970-01-01T00:06:40Z");
        if let Some(p) = status.cook_progress.as_mut() {
            p.stage_ends_at = None;
            p.stage_started_at = None;
        }
        let v = state_json(Some(&status), None, true, None);
        assert_eq!(v["stage_ends_at"], serde_json::Value::Null);
        assert_eq!(v["stage_started_at"], serde_json::Value::Null);
    }

    #[test]
    fn a_running_timer_without_the_progress_has_no_end() {
        let mut status = crate::cook_progress::tests::base_status();
        status.timer_mode = "running".into();
        status.timer_total_secs = 600;
        status.timer_current_secs = 600;
        assert!(status.timer_remaining_secs().is_some());
        let v = state_json(Some(&status), None, true, None);
        assert_eq!(v["stage_ends_at"], serde_json::Value::Null);
        assert_eq!(v["stage_started_at"], serde_json::Value::Null);
    }

    /// A processor, and the sources it reads, with a state machine that
    /// answers every start with `answer()` (`Ok` accepts it).
    fn processor_with(answer: fn() -> Result<(), SmError>) -> (Processor, Sources) {
        let (client, eventloop) = AsyncClient::new(MqttOptions::new("test", "localhost", 1883), 64);
        // Never polled; kept alive so publishes queue instead of failing.
        std::mem::forget(eventloop);
        let topics = Topics {
            prefix: "anova/oven".into(),
            discovery: "homeassistant".into(),
        };
        let (sm_cmd_tx, mut sm_cmd_rx) = mpsc::channel(4);
        tokio::spawn(async move {
            while let Some(cmd) = sm_cmd_rx.recv().await {
                match cmd {
                    StateMachineCommand::StartCook { reply, .. }
                    | StateMachineCommand::StartManualCook { reply, .. }
                    | StateMachineCommand::StartScheduledNow { reply } => {
                        let _ = reply.send(answer());
                    }
                    _ => {}
                }
            }
        });
        let sources = Sources {
            read_model: watch::channel(ReadModel::default()).1,
            cook_progress: watch::channel(None).1,
            liveness: Arc::new(Liveness::new(30)),
            failures: broadcast::channel(4).1,
            sm_cmd_tx,
        };
        (Processor::new(client, topics, true, None), sources)
    }

    fn pending(p: &Processor) -> Option<&str> {
        p.pending_started_by.as_ref().map(|(user, _)| user.as_str())
    }

    #[tokio::test]
    async fn only_an_accepted_start_keeps_its_started_by() {
        let start = br#"{"recipe_id": "r1", "started_by": "alice"}"#;
        let (mut p, sources) = processor_with(|| Ok(()));
        p.on_command("anova/oven/cmd/start", start, &sources).await;
        assert_eq!(pending(&p), Some("alice"));
        // A later start without one clears it.
        p.on_command("anova/oven/cmd/start", br#"{"recipe_id": "r1"}"#, &sources)
            .await;
        assert_eq!(pending(&p), None);

        // The state machine refuses it.
        let (mut p, sources) = processor_with(|| Err(SmError::Disconnected));
        p.on_command("anova/oven/cmd/start", start, &sources).await;
        assert_eq!(pending(&p), None);
    }

    #[tokio::test]
    async fn an_early_refusal_keeps_no_started_by() {
        let (mut p, sources) = processor_with(|| Ok(()));
        // No recipe given and none selected.
        p.on_command(
            "anova/oven/cmd/start",
            br#"{"started_by": "alice"}"#,
            &sources,
        )
        .await;
        assert_eq!(pending(&p), None);
        // A manual cook that `manual::check` refuses: no stages.
        let bad = br#"{"title": "x", "stages": [], "started_by": "alice"}"#;
        p.on_command("anova/oven/cmd/start_manual", bad, &sources)
            .await;
        assert_eq!(pending(&p), None);
        // A refused start leaves an earlier accepted one alone.
        p.on_command(
            "anova/oven/cmd/start",
            br#"{"recipe_id": "r1", "started_by": "bob"}"#,
            &sources,
        )
        .await;
        p.on_command(
            "anova/oven/cmd/start",
            br#"{"started_by": "alice"}"#,
            &sources,
        )
        .await;
        assert_eq!(pending(&p), Some("bob"));
    }

    #[tokio::test]
    async fn a_scheduled_cook_started_now_is_no_ones() {
        let (mut p, sources) = processor_with(|| Ok(()));
        p.on_command(
            "anova/oven/cmd/start",
            br#"{"recipe_id": "r1", "started_by": "alice"}"#,
            &sources,
        )
        .await;
        p.on_command("anova/oven/cmd/start_scheduled_now", b"PRESS", &sources)
            .await;
        assert_eq!(pending(&p), None);
    }

    #[test]
    fn command_failed_names_the_requester_not_the_running_cook() {
        // Alice's cook runs. Bob's start is refused: his name, not hers.
        let running = Snapshot {
            connected: true,
            cooking: true,
            cook_key: Some("cook-1".into()),
            ..Default::default()
        };
        let p = failure_payload(Some(&running), "start", "busy", Some("bob"));
        assert_eq!(p["event_type"], "command_failed");
        assert_eq!(p["command"], "start");
        assert_eq!(p["reason"], "busy");
        assert_eq!(p["started_by"], "bob");
        // Any other command names no one.
        let p = failure_payload(Some(&running), "lamp", "offline", None);
        assert_eq!(p["started_by"], serde_json::Value::Null);
    }

    #[test]
    fn started_by_takes_only_a_short_plain_id() {
        let id = "8f1b2c3d4e5f40718293a4b5c6d7e8f9";
        assert_eq!(
            started_by(&format!(r#"{{"recipe_id": "r1", "started_by": "{id}"}}"#)),
            Some(id.to_string())
        );
        assert_eq!(started_by(r#"{"recipe_id": "r1"}"#), None);
        assert_eq!(started_by("PRESS"), None);
        for bad in ["", " ", "a b", "<script>", "x\n", &"a".repeat(65)] {
            let json = serde_json::json!({ "started_by": bad }).to_string();
            assert_eq!(started_by(&json), None, "{bad}");
        }
        assert_eq!(started_by(r#"{"started_by": 7}"#), None);
    }

    #[test]
    fn control_payloads() {
        assert_eq!(
            control_request("lamp", "on"),
            Ok(ControlRequest::Lamp { on: true })
        );
        assert_eq!(
            control_request("timer", "90"),
            Ok(ControlRequest::Timer { secs: 90 })
        );
        assert_eq!(
            control_request("timer", "5400.0"),
            Ok(ControlRequest::Timer { secs: 5400 })
        );
        assert_eq!(
            control_request("temperature", "180"),
            Ok(ControlRequest::Temperature { celsius: 180.0 })
        );
        assert!(control_request("temperature", "500").is_err());
        assert!(control_request("lamp", "dim").is_err());
        assert!(control_request("timer", "0").is_err());
        assert!(control_request("bogus", "").is_err());
    }

    #[test]
    fn state_without_a_status_is_still_published() {
        let v = state_json(None, None, false, None);
        assert_eq!(v["connected"], false);
        assert_eq!(v["scheduled_start"], serde_json::Value::Null);
        let scheduled = ScheduledStart {
            at: "2026-11-07T16:30:00Z".into(),
            title: "Bread".into(),
            recipe_id: Some("r1".into()),
        };
        let v = state_json(None, None, false, Some(&scheduled));
        assert_eq!(v["scheduled_start"]["at"], "2026-11-07T16:30:00Z");
    }

    #[test]
    fn broker_addresses() {
        assert_eq!(
            broker_address("mqtt://broker.lan"),
            Ok(("broker.lan".into(), 1883))
        );
        assert_eq!(
            broker_address("mqtt://192.0.2.5:1884/"),
            Ok(("192.0.2.5".into(), 1884))
        );
        assert!(broker_address("mqtts://broker.lan").is_err());
        assert!(broker_address("mqtt://u:p@broker.lan").is_err());
        assert!(broker_address("broker.lan").is_err());
    }

    #[test]
    fn options_parse_the_url() {
        let config = MqttConfig {
            url: "mqtt://broker.lan:1883".into(),
            username: Some("u".into()),
            password: Some("p".into()),
            topics: Topics {
                prefix: "anova/oven".into(),
                discovery: "homeassistant".into(),
            },
            local: true,
            client_id: "anova-oven-test".into(),
            time_zone: None,
        };
        let options = config.options().unwrap();
        assert_eq!(options.broker_address(), ("broker.lan".to_string(), 1883));
        assert_eq!(options.client_id(), "anova-oven-test");
    }

    #[test]
    fn client_id_defaults_when_unset_or_blank() {
        assert_eq!(client_id(None), "anova-oven-server");
        assert_eq!(client_id(Some("  ")), "anova-oven-server");
        assert_eq!(client_id(Some(" oven-test ")), "oven-test");
    }
}
