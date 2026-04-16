use alloc::string::String;
use alloc::vec::Vec;

use defmt::{error, info, warn};
use embassy_executor::{SpawnError, Spawner};
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::watch::{Receiver, Sender, Watch};
use embassy_time::{with_timeout, Duration, Instant, Timer};
use heapless::Vec as HeaplessVec;

use crate::api::{fetch_current_cook, fetch_recipes, fetch_status, send_start, send_stop};

const API_CALL_TIMEOUT_SECS: u64 = 5;
const POST_ACTION_COOK_REFRESH_DELAY_SECS: u64 = 1;
const POST_START_CURRENT_COOK_REFRESH_DELAY_SECS: u64 = 3;
const COOK_POLL_INTERVAL_SECS: u64 = 10;
const RECIPE_POLL_INTERVAL_SECS: u64 = 3600;
const NORMAL_POLL_INTERVAL_SECS: u64 = 1;
const POLL_BACKOFF_TIER1_FAILS: u64 = 5;
const POLL_BACKOFF_TIER2_FAILS: u64 = 10;
const POLL_BACKOFF_TIER3_FAILS: u64 = 15;
const POLL_BACKOFF_TIER1_SECS: u64 = 5;
const POLL_BACKOFF_TIER2_SECS: u64 = 15;
const POLL_BACKOFF_TIER3_SECS: u64 = 30;
const EVENT_QUEUE_CAPACITY: usize = 16;

pub const OFFLINE_THRESHOLD: u64 = 3;

pub type CommandChannel = Channel<CriticalSectionRawMutex, ApiCommand, 4>;
pub type StateWatch = Watch<CriticalSectionRawMutex, ApiSnapshot, 1>;
pub type StateReceiver<'a> = Receiver<'a, CriticalSectionRawMutex, ApiSnapshot, 1>;

pub struct ApiClient<'a> {
    commands: &'a CommandChannel,
    state: &'a StateWatch,
}

#[derive(Clone)]
pub enum ApiCommand {
    Start { recipe_id: String },
    Stop,
}

#[derive(Clone, Default)]
pub struct ApiSnapshot {
    pub status: Option<anova_oven_api::OvenStatus>,
    pub current_cook: Option<anova_oven_api::CurrentCook>,
    pub recipes: Vec<anova_oven_api::Recipe>,
    pub fail_count: u64,
    pub last_success_at: Option<Instant>,
}

impl ApiSnapshot {
    pub fn is_offline(&self) -> bool {
        self.fail_count >= OFFLINE_THRESHOLD
    }

    pub fn has_first_data(&self) -> bool {
        self.last_success_at.is_some()
    }

    pub fn is_cooking(&self) -> bool {
        self.current_cook.is_some()
            || self
                .status
                .as_ref()
                .is_some_and(|status| status.mode.as_str() != "idle")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EventKind {
    PollStatus,
    PollCurrentCook,
    PollRecipes,
    ApiStart,
    ApiStop,
}

#[derive(Clone, Copy)]
struct ScheduledEvent {
    kind: EventKind,
    execution_time: Instant,
    priority: u8,
}

impl EventKind {
    fn priority(self) -> u8 {
        match self {
            EventKind::ApiStart | EventKind::ApiStop => 0,
            _ => 1,
        }
    }
}

#[derive(Clone, Copy)]
enum EnqueueMode {
    PreferEarlier,
    Replace,
}

struct EventQueue {
    events: HeaplessVec<ScheduledEvent, EVENT_QUEUE_CAPACITY>,
}

impl EventQueue {
    fn new() -> Self {
        Self {
            events: HeaplessVec::new(),
        }
    }

    fn enqueue(&mut self, kind: EventKind, execution_time: Instant, mode: EnqueueMode) {
        if let Some(existing) = self.events.iter_mut().find(|event| event.kind == kind) {
            existing.execution_time = match mode {
                EnqueueMode::PreferEarlier => existing.execution_time.min(execution_time),
                EnqueueMode::Replace => execution_time,
            };
            return;
        }

        if self
            .events
            .push(ScheduledEvent {
                kind,
                execution_time,
                priority: kind.priority(),
            })
            .is_err()
        {
            error!(
                "Api event queue overflow (capacity {}); dropping event",
                EVENT_QUEUE_CAPACITY
            );
        }
    }

    fn soonest_index(&self) -> Option<usize> {
        self.events
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                a.execution_time
                    .cmp(&b.execution_time)
                    .then(a.priority.cmp(&b.priority))
            })
            .map(|(idx, _)| idx)
    }

    fn next_due_at(&self) -> Option<Instant> {
        self.soonest_index()
            .map(|idx| self.events[idx].execution_time)
    }

    fn has_pending(&self, kind: EventKind) -> bool {
        self.events.iter().any(|event| event.kind == kind)
    }

    fn pop_due(&mut self, now: Instant) -> Option<ScheduledEvent> {
        let idx = self.soonest_index()?;
        if self.events[idx].execution_time > now {
            return None;
        }

        Some(self.events.swap_remove(idx))
    }
}

struct ApiRuntime<'a> {
    stack: embassy_net::Stack<'static>,
    state_tx: Sender<'a, CriticalSectionRawMutex, ApiSnapshot, 1>,
    snapshot: ApiSnapshot,
    event_queue: EventQueue,
    pending_start_recipe_id: Option<String>,
}

impl<'a> ApiClient<'a> {
    pub fn new(
        stack: embassy_net::Stack<'static>,
        commands: &'static CommandChannel,
        state: &'static StateWatch,
        spawner: Spawner,
    ) -> Result<Self, SpawnError> {
        spawner.spawn(api_client_task(stack, commands, state)?);
        Ok(Self { commands, state })
    }

    pub fn snapshot(&self) -> ApiSnapshot {
        self.state.try_get().unwrap_or_default()
    }

    pub fn receiver(&self) -> Option<StateReceiver<'a>> {
        self.state.receiver()
    }

    pub fn start(&self, recipe_id: String) {
        if self
            .commands
            .try_send(ApiCommand::Start { recipe_id })
            .is_err()
        {
            warn!("API command channel full; dropping start command");
        }
    }

    pub fn stop(&self) {
        if self.commands.try_send(ApiCommand::Stop).is_err() {
            warn!("API command channel full; dropping stop command");
        }
    }
}

impl<'a> ApiRuntime<'a> {
    fn new(
        stack: embassy_net::Stack<'static>,
        state_tx: Sender<'a, CriticalSectionRawMutex, ApiSnapshot, 1>,
    ) -> Self {
        let now = Instant::now();
        let mut event_queue = EventQueue::new();
        event_queue.enqueue(EventKind::PollStatus, now, EnqueueMode::PreferEarlier);
        event_queue.enqueue(EventKind::PollCurrentCook, now, EnqueueMode::PreferEarlier);
        event_queue.enqueue(EventKind::PollRecipes, now, EnqueueMode::PreferEarlier);

        Self {
            stack,
            state_tx,
            snapshot: ApiSnapshot::default(),
            event_queue,
            pending_start_recipe_id: None,
        }
    }

    fn next_poll_interval_secs(&self) -> u64 {
        match self.snapshot.fail_count {
            n if n >= POLL_BACKOFF_TIER3_FAILS => POLL_BACKOFF_TIER3_SECS,
            n if n >= POLL_BACKOFF_TIER2_FAILS => POLL_BACKOFF_TIER2_SECS,
            n if n >= POLL_BACKOFF_TIER1_FAILS => POLL_BACKOFF_TIER1_SECS,
            _ => NORMAL_POLL_INTERVAL_SECS,
        }
    }

    fn publish_snapshot(&self) {
        self.state_tx.send(self.snapshot.clone());
    }

    fn record_fast_poll_success(&mut self) {
        self.snapshot.fail_count = 0;
        self.snapshot.last_success_at = Some(Instant::now());
    }

    fn record_fast_poll_failure(&mut self, message: &'static str) {
        self.snapshot.fail_count = self.snapshot.fail_count.saturating_add(1);
        warn!(
            "{} ({} consecutive fast-poll failures)",
            message, self.snapshot.fail_count
        );
    }

    fn queue_post_action_refresh(&mut self, now: Instant) {
        let refresh_at = now + Duration::from_secs(POST_ACTION_COOK_REFRESH_DELAY_SECS);
        self.event_queue.enqueue(
            EventKind::PollStatus,
            refresh_at,
            EnqueueMode::PreferEarlier,
        );
        self.event_queue.enqueue(
            EventKind::PollCurrentCook,
            refresh_at,
            EnqueueMode::PreferEarlier,
        );
    }

    fn poll_action_in_flight(&self) -> bool {
        self.pending_start_recipe_id.is_some()
            || self.event_queue.has_pending(EventKind::ApiStart)
            || self.event_queue.has_pending(EventKind::ApiStop)
    }

    fn reconcile_current_cook_recipe_title(&mut self) {
        let Some(cook) = self.snapshot.current_cook.as_mut() else {
            return;
        };

        let Some(recipe_id) = cook.recipe_id.as_deref() else {
            return;
        };

        if let Some(recipe) = self
            .snapshot
            .recipes
            .iter()
            .find(|recipe| recipe.id == recipe_id)
        {
            cook.recipe_title = recipe.title.clone();
        }
    }

    async fn handle_event(&mut self, event: ScheduledEvent) {
        match event.kind {
            EventKind::PollStatus => self.handle_poll_status().await,
            EventKind::PollCurrentCook => self.handle_poll_current_cook().await,
            EventKind::PollRecipes => self.handle_poll_recipes().await,
            EventKind::ApiStart => self.handle_api_start().await,
            EventKind::ApiStop => self.handle_api_stop().await,
        }
    }

    async fn handle_api_start(&mut self) {
        let Some(recipe_id) = self.pending_start_recipe_id.take() else {
            warn!("ApiStart fired without a staged recipe id");
            return;
        };

        info!("Sending POST /start with recipe id: {}", recipe_id.as_str());
        if with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            send_start(self.stack, recipe_id.as_str()),
        )
        .await
        .is_err()
        {
            warn!("POST /start: timed out");
        }

        let now = Instant::now();
        self.event_queue.enqueue(
            EventKind::PollStatus,
            now + Duration::from_secs(POST_ACTION_COOK_REFRESH_DELAY_SECS),
            EnqueueMode::PreferEarlier,
        );
        self.event_queue.enqueue(
            EventKind::PollCurrentCook,
            now + Duration::from_secs(POST_START_CURRENT_COOK_REFRESH_DELAY_SECS),
            EnqueueMode::PreferEarlier,
        );
    }

    async fn handle_api_stop(&mut self) {
        if with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            send_stop(self.stack),
        )
        .await
        .is_err()
        {
            warn!("POST /stop: timed out");
        }
    }

    async fn handle_poll_status(&mut self) {
        if self.poll_action_in_flight() {
            self.event_queue.enqueue(
                EventKind::PollStatus,
                Instant::now() + Duration::from_secs(POST_ACTION_COOK_REFRESH_DELAY_SECS),
                EnqueueMode::PreferEarlier,
            );
            return;
        }

        match with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            fetch_status(self.stack),
        )
        .await
        {
            Ok(Some(status)) => {
                self.snapshot.status = Some(status);
                self.record_fast_poll_success();
            }
            Ok(None) => {
                self.record_fast_poll_failure("GET /status failed");
            }
            Err(_) => {
                self.record_fast_poll_failure("GET /status timed out");
            }
        }

        let interval = self
            .next_poll_interval_secs()
            .max(NORMAL_POLL_INTERVAL_SECS);
        self.event_queue.enqueue(
            EventKind::PollStatus,
            Instant::now() + Duration::from_secs(interval),
            EnqueueMode::PreferEarlier,
        );

        self.publish_snapshot();
    }

    async fn handle_poll_current_cook(&mut self) {
        if self.poll_action_in_flight() {
            self.event_queue.enqueue(
                EventKind::PollCurrentCook,
                Instant::now() + Duration::from_secs(POST_ACTION_COOK_REFRESH_DELAY_SECS),
                EnqueueMode::PreferEarlier,
            );
            return;
        }

        match with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            fetch_current_cook(self.stack),
        )
        .await
        {
            Ok(current_cook) => {
                self.snapshot.current_cook = current_cook;
                self.record_fast_poll_success();
                self.reconcile_current_cook_recipe_title();
            }
            Err(_) => {
                self.record_fast_poll_failure("GET /current-cook timed out");
            }
        }

        let interval = self.next_poll_interval_secs().max(COOK_POLL_INTERVAL_SECS);
        self.event_queue.enqueue(
            EventKind::PollCurrentCook,
            Instant::now() + Duration::from_secs(interval),
            EnqueueMode::PreferEarlier,
        );

        self.publish_snapshot();
    }

    async fn handle_poll_recipes(&mut self) {
        self.snapshot.recipes = match with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            fetch_recipes(self.stack),
        )
        .await
        {
            Ok(recipes) => recipes,
            Err(_) => {
                warn!("GET /recipes: timed out");
                core::mem::take(&mut self.snapshot.recipes)
            }
        };

        self.reconcile_current_cook_recipe_title();

        let interval = self
            .next_poll_interval_secs()
            .max(RECIPE_POLL_INTERVAL_SECS);
        self.event_queue.enqueue(
            EventKind::PollRecipes,
            Instant::now() + Duration::from_secs(interval),
            EnqueueMode::PreferEarlier,
        );
        self.publish_snapshot();
    }

    fn handle_command(&mut self, command: ApiCommand) {
        let now = Instant::now();

        match command {
            ApiCommand::Start { recipe_id } => {
                self.pending_start_recipe_id = Some(recipe_id);
                self.event_queue
                    .enqueue(EventKind::ApiStart, now, EnqueueMode::PreferEarlier);
            }
            ApiCommand::Stop => {
                self.event_queue
                    .enqueue(EventKind::ApiStop, now, EnqueueMode::PreferEarlier);
                self.queue_post_action_refresh(now);
            }
        }
    }
}

#[embassy_executor::task]
async fn api_client_task(
    stack: embassy_net::Stack<'static>,
    commands: &'static CommandChannel,
    state: &'static StateWatch,
) -> ! {
    let mut runtime = ApiRuntime::new(stack, state.sender());

    loop {
        if let Some(next_due) = runtime.event_queue.next_due_at() {
            match select(Timer::at(next_due), commands.receive()).await {
                Either::First(()) => {
                    while let Some(event) = runtime.event_queue.pop_due(Instant::now()) {
                        runtime.handle_event(event).await;
                    }
                }
                Either::Second(command) => runtime.handle_command(command),
            }
        } else {
            let command = commands.receive().await;
            runtime.handle_command(command);
        }
    }
}
