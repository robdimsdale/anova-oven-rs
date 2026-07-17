use alloc::string::String;

use defmt::{error, info, warn};
use embassy_executor::{SpawnError, Spawner};
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::watch::{Receiver, Sender, Watch};
use embassy_time::{with_timeout, Duration, Instant, Timer};
use portable_atomic_util::Arc;
use static_cell::StaticCell;

use anova_oven_pico_core::api::normalize_server_url;
pub use anova_oven_pico_core::fsm::ApiSnapshot;
use anova_oven_pico_core::scheduler::{
    EnqueueMode, EventKind, EventQueue, ScheduledEvent, EVENT_QUEUE_CAPACITY,
};

use crate::api::{
    fetch_current_cook, fetch_recipes, fetch_status, send_start, send_stop, Aligned,
    HTTP_RX_BUF_LEN,
};

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

struct ApiRuntime<'a> {
    stack: embassy_net::Stack<'static>,
    state_tx: Sender<'a, CriticalSectionRawMutex, ApiSnapshot, 1>,
    // 16 KB HTTP RX buffer, owned exclusively by this runtime. Taken once from a
    // StaticCell in `api_client_task`, so the "one buffer, one user" invariant is
    // a compile-time fact rather than an unenforced `static mut` convention.
    rx_buf: &'a mut [u8],
    // Normalized server base URL, computed once at construction. SERVER_URL is a
    // compile-time `env!` constant, so re-normalizing it per request just churned
    // the heap ~1/s forever (review §2.1).
    server_url: String,
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
        rx_buf: &'a mut [u8],
    ) -> Self {
        let now = Instant::now();
        let mut event_queue = EventQueue::new();
        // Stagger the three initial polls so the first drain isn't ~15 s of
        // back-to-back network I/O during which a user Stop sits unserviced
        // (review §1.2). A fresh queue with three pushes can't overflow.
        let _ = event_queue.enqueue(EventKind::PollStatus, now, EnqueueMode::PreferEarlier);
        let _ = event_queue.enqueue(
            EventKind::PollCurrentCook,
            now + Duration::from_millis(250),
            EnqueueMode::PreferEarlier,
        );
        let _ = event_queue.enqueue(
            EventKind::PollRecipes,
            now + Duration::from_millis(500),
            EnqueueMode::PreferEarlier,
        );

        Self {
            stack,
            state_tx,
            rx_buf,
            server_url: normalize_server_url(crate::SERVER_URL),
            snapshot: ApiSnapshot::default(),
            event_queue,
            pending_start_recipe_id: None,
        }
    }

    /// Enqueue + log on overflow. The lib's `EventQueue::enqueue` returns
    /// `Result<(), QueueOverflow>` so it stays effect-free; the bin owns the
    /// `error!` and the policy of dropping on overflow.
    fn enqueue(&mut self, kind: EventKind, execution_time: Instant, mode: EnqueueMode) {
        if self
            .event_queue
            .enqueue(kind, execution_time, mode)
            .is_err()
        {
            error!(
                "Api event queue overflow (capacity {}); dropping event",
                EVENT_QUEUE_CAPACITY
            );
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
        crate::persist::record_api_fail_count(0);
        crate::ota::notify_api_success();
    }

    fn record_fast_poll_failure(&mut self, message: &'static str) {
        self.snapshot.fail_count = self.snapshot.fail_count.saturating_add(1);
        crate::persist::record_api_fail_count(self.snapshot.fail_count as u32);
        warn!(
            "{} ({} consecutive fast-poll failures)",
            message, self.snapshot.fail_count
        );
    }

    fn queue_post_action_refresh(&mut self, now: Instant) {
        let refresh_at = now + Duration::from_secs(POST_ACTION_COOK_REFRESH_DELAY_SECS);
        self.enqueue(
            EventKind::PollStatus,
            refresh_at,
            EnqueueMode::PreferEarlier,
        );
        self.enqueue(
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
            send_start(
                self.stack,
                &mut *self.rx_buf,
                &self.server_url,
                recipe_id.as_str(),
            ),
        )
        .await
        .is_err()
        {
            warn!("POST /start: timed out");
        }

        let now = Instant::now();
        self.enqueue(
            EventKind::PollStatus,
            now + Duration::from_secs(POST_ACTION_COOK_REFRESH_DELAY_SECS),
            EnqueueMode::PreferEarlier,
        );
        self.enqueue(
            EventKind::PollCurrentCook,
            now + Duration::from_secs(POST_START_CURRENT_COOK_REFRESH_DELAY_SECS),
            EnqueueMode::PreferEarlier,
        );
    }

    async fn handle_api_stop(&mut self) {
        if with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            send_stop(self.stack, &mut *self.rx_buf, &self.server_url),
        )
        .await
        .is_err()
        {
            warn!("POST /stop: timed out");
        }
    }

    async fn handle_poll_status(&mut self) {
        if self.poll_action_in_flight() {
            self.enqueue(
                EventKind::PollStatus,
                Instant::now() + Duration::from_secs(POST_ACTION_COOK_REFRESH_DELAY_SECS),
                EnqueueMode::PreferEarlier,
            );
            return;
        }

        match with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            fetch_status(self.stack, &mut *self.rx_buf, &self.server_url),
        )
        .await
        {
            Ok(Ok(status)) => {
                self.snapshot.status = Some(status);
                self.record_fast_poll_success();
            }
            Ok(Err(_)) => {
                self.record_fast_poll_failure("GET /status failed");
            }
            Err(_) => {
                self.record_fast_poll_failure("GET /status timed out");
            }
        }

        let interval = self
            .next_poll_interval_secs()
            .max(NORMAL_POLL_INTERVAL_SECS);
        self.enqueue(
            EventKind::PollStatus,
            Instant::now() + Duration::from_secs(interval),
            EnqueueMode::PreferEarlier,
        );

        self.publish_snapshot();
    }

    async fn handle_poll_current_cook(&mut self) {
        if self.poll_action_in_flight() {
            self.enqueue(
                EventKind::PollCurrentCook,
                Instant::now() + Duration::from_secs(POST_ACTION_COOK_REFRESH_DELAY_SECS),
                EnqueueMode::PreferEarlier,
            );
            return;
        }

        match with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            fetch_current_cook(self.stack, &mut *self.rx_buf, &self.server_url),
        )
        .await
        {
            Ok(Ok(Some(cook))) => {
                self.snapshot.current_cook = Some(cook);
                self.record_fast_poll_success();
                self.reconcile_current_cook_recipe_title();
            }
            Ok(Ok(None)) => {
                self.snapshot.current_cook = None;
                self.record_fast_poll_success();
            }
            Ok(Err(_)) => {
                self.record_fast_poll_failure("GET /current-cook failed");
            }
            Err(_) => {
                self.record_fast_poll_failure("GET /current-cook timed out");
            }
        }

        let interval = self.next_poll_interval_secs().max(COOK_POLL_INTERVAL_SECS);
        self.enqueue(
            EventKind::PollCurrentCook,
            Instant::now() + Duration::from_secs(interval),
            EnqueueMode::PreferEarlier,
        );

        self.publish_snapshot();
    }

    async fn handle_poll_recipes(&mut self) {
        match with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            fetch_recipes(self.stack, &mut *self.rx_buf, &self.server_url),
        )
        .await
        {
            Ok(Ok(recipes)) => {
                self.snapshot.recipes = Arc::new(recipes);
            }
            Ok(Err(_)) => {
                warn!("GET /recipes: fetch failed");
            }
            Err(_) => {
                warn!("GET /recipes: timed out");
            }
        };

        self.reconcile_current_cook_recipe_title();

        let interval = self
            .next_poll_interval_secs()
            .max(RECIPE_POLL_INTERVAL_SECS);
        self.enqueue(
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
                // If two Start commands for different recipes arrive before the
                // drain loop services them, this overwrites the first recipe id
                // and the two ApiStart events coalesce (enqueue dedups by
                // kind), so recipe A is silently dropped in favour of B. This
                // is acceptable: two back-to-back starts for different recipes
                // would be rejected by the upstream Anova oven server (not our
                // intermediary server) anyway, so only the latter could ever
                // have taken effect.
                self.pending_start_recipe_id = Some(recipe_id);
                self.enqueue(EventKind::ApiStart, now, EnqueueMode::PreferEarlier);
            }
            ApiCommand::Stop => {
                self.enqueue(EventKind::ApiStop, now, EnqueueMode::PreferEarlier);
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
    static RX_BUF: StaticCell<Aligned<HTTP_RX_BUF_LEN>> = StaticCell::new();
    let rx_buf = &mut RX_BUF.init(Aligned([0u8; HTTP_RX_BUF_LEN])).0[..];
    let mut runtime = ApiRuntime::new(stack, state.sender(), rx_buf);

    loop {
        crate::persist::bump_api_heartbeat();
        if let Some(next_due) = runtime.event_queue.next_due_at() {
            match select(Timer::at(next_due), commands.receive()).await {
                Either::First(()) => loop {
                    // Service pending commands before each event so a user
                    // Stop/Start isn't stuck behind a multi-second poll drain
                    // (review §1.2). handle_command enqueues ApiStop/ApiStart
                    // at `now` with priority 0, so the following pop_due
                    // tie-breaks it ahead of any other poll due at the same
                    // instant. Exit once nothing is due and park in the outer
                    // select.
                    while let Ok(command) = commands.try_receive() {
                        runtime.handle_command(command);
                    }
                    match runtime.event_queue.pop_due(Instant::now()) {
                        Some(event) => runtime.handle_event(event).await,
                        None => break,
                    }
                },
                Either::Second(command) => runtime.handle_command(command),
            }
        } else {
            let command = commands.receive().await;
            runtime.handle_command(command);
        }
    }
}
