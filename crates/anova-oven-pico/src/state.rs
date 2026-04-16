use alloc::string::String;

use defmt::warn;
use embassy_futures::select::{select, select3, Either, Either3};
use embassy_time::{Duration, Instant, Timer};

use crate::api_client::{ApiClient, ApiSnapshot, StateReceiver};
use crate::backlight::BacklightController;
use crate::display::{Display, ViewSpec};
use crate::input::{Input, InputEvent};

const MENU_INACTIVITY_TIMEOUT_SECS: u64 = 15;
const STOP_CONFIRM_TIMEOUT_SECS: u64 = 5;
const START_STOP_CONFIRM_TIMEOUT_SECS: u64 = 10;
const IDLE: &str = "idle";

#[derive(Clone)]
pub enum AppState {
    Offline,
    Idle,
    Cooking {
        optimistic_recipe_title: Option<String>,
    },
    BrowseRecipes {
        index: usize,
    },
    StartPending {
        recipe_title: String,
        recipe_id: String,
        since: Instant,
    },
    ConfirmStop,
    StopPending {
        since: Instant,
    },
}

impl Default for AppState {
    fn default() -> Self {
        Self::Idle
    }
}

pub enum BacklightPolicy {
    Full,
    Dim,
    FullThenDimAfter(Duration),
}

pub struct Ctx<'a> {
    pub input: &'a Input<'static>,
    pub api: &'a ApiClient<'static>,
    pub api_rx: StateReceiver<'static>,
    pub display: &'a Display<'static>,
    pub backlight: BacklightController,
}

impl<'a> Ctx<'a> {
    pub async fn api_changed(&mut self) {
        let _ = self.api_rx.changed().await;
    }
}

impl AppState {
    pub async fn execute(self, ctx: &mut Ctx<'_>) -> AppState {
        ctx.backlight.apply(self.backlight_policy());

        match self {
            AppState::Offline => execute_offline(ctx).await,
            AppState::Idle => execute_idle(ctx).await,
            AppState::Cooking {
                optimistic_recipe_title,
            } => execute_cooking(optimistic_recipe_title, ctx).await,
            AppState::BrowseRecipes { index } => execute_browse(index, ctx).await,
            AppState::StartPending {
                recipe_title,
                recipe_id,
                since,
            } => execute_start_pending(recipe_title, recipe_id, since, ctx).await,
            AppState::ConfirmStop => execute_confirm_stop(ctx).await,
            AppState::StopPending { since } => execute_stop_pending(since, ctx).await,
        }
    }

    fn backlight_policy(&self) -> BacklightPolicy {
        match self {
            AppState::Idle | AppState::Cooking { .. } => {
                BacklightPolicy::FullThenDimAfter(Duration::from_secs(5))
            }
            AppState::Offline
            | AppState::BrowseRecipes { .. }
            | AppState::StartPending { .. }
            | AppState::ConfirmStop
            | AppState::StopPending { .. } => BacklightPolicy::Full,
        }
    }

    fn idle_dim_delay(&self) -> Duration {
        match self.backlight_policy() {
            BacklightPolicy::FullThenDimAfter(delay) => delay,
            BacklightPolicy::Full | BacklightPolicy::Dim => Duration::from_secs(5),
        }
    }
}

fn baseline_state_for(snap: &ApiSnapshot) -> AppState {
    if snap.is_cooking() {
        AppState::Cooking {
            optimistic_recipe_title: None,
        }
    } else {
        AppState::Idle
    }
}

fn idle_view(snap: &ApiSnapshot) -> ViewSpec {
    if !snap.has_first_data() {
        ViewSpec::Connecting
    } else {
        ViewSpec::Status {
            status: snap.status.clone(),
            cook: snap.current_cook.clone(),
        }
    }
}

fn cooking_view(snap: &ApiSnapshot, optimistic_recipe_title: Option<&str>) -> ViewSpec {
    let cook = if snap.current_cook.is_some() {
        snap.current_cook.clone()
    } else if snap
        .status
        .as_ref()
        .is_some_and(|status| status.is_cooking())
    {
        optimistic_recipe_title.map(|title| anova_oven_api::CurrentCook {
            recipe_title: title.into(),
            recipe_id: None,
            started_at: String::from("pending"),
            stages: alloc::vec::Vec::new(),
            cook_stage_count: 0,
            total_stage_count: 0,
        })
    } else {
        None
    };

    ViewSpec::Status {
        status: snap.status.clone(),
        cook,
    }
}

fn optimistic_idle_view(snap: &ApiSnapshot) -> ViewSpec {
    let status = snap.status.as_ref().map(|status| {
        let mut optimistic = status.clone();
        optimistic.mode = String::from(IDLE);
        optimistic.timer_mode = String::from(IDLE);
        optimistic.timer_current_secs = 0;
        optimistic.target_temperature_c = None;
        optimistic.steam_target_pct = None;
        optimistic
    });

    ViewSpec::Status { status, cook: None }
}

async fn execute_offline(ctx: &mut Ctx<'_>) -> AppState {
    ctx.display.render(ViewSpec::ServerOffline);

    loop {
        ctx.api_changed().await;
        let snap = ctx.api.snapshot();
        if !snap.is_offline() {
            return baseline_state_for(&snap);
        }
    }
}

async fn execute_idle(ctx: &mut Ctx<'_>) -> AppState {
    let idle_dim_delay = AppState::Idle.idle_dim_delay();
    let mut dim_at = Instant::now() + idle_dim_delay;
    let mut dimmed = false;

    loop {
        let snap = ctx.api.snapshot();

        if snap.is_offline() {
            return AppState::Offline;
        }
        if snap.is_cooking() {
            return AppState::Cooking {
                optimistic_recipe_title: None,
            };
        }

        ctx.display.render(idle_view(&snap));

        if dimmed {
            match select(ctx.input.recv(), ctx.api_changed()).await {
                Either::First(InputEvent::EncoderCW) if !snap.recipes.is_empty() => {
                    ctx.backlight.set_full();
                    return AppState::BrowseRecipes { index: 0 };
                }
                Either::First(_) => {
                    ctx.backlight.set_full();
                    dim_at = Instant::now() + idle_dim_delay;
                    dimmed = false;
                }
                Either::Second(()) => {}
            }
        } else {
            match select3(ctx.input.recv(), ctx.api_changed(), Timer::at(dim_at)).await {
                Either3::First(InputEvent::EncoderCW) if !snap.recipes.is_empty() => {
                    ctx.backlight.set_full();
                    return AppState::BrowseRecipes { index: 0 };
                }
                Either3::First(_) => {
                    ctx.backlight.set_full();
                    dim_at = Instant::now() + idle_dim_delay;
                }
                Either3::Second(()) => {}
                Either3::Third(()) => {
                    ctx.backlight.set_dim();
                    dimmed = true;
                }
            }
        }
    }
}

async fn execute_cooking(
    mut optimistic_recipe_title: Option<String>,
    ctx: &mut Ctx<'_>,
) -> AppState {
    loop {
        let snap = ctx.api.snapshot();

        if snap.is_offline() {
            return AppState::Offline;
        }
        if !snap.is_cooking() {
            return AppState::Idle;
        }

        if snap.current_cook.is_some() {
            optimistic_recipe_title = None;
        }

        ctx.display
            .render(cooking_view(&snap, optimistic_recipe_title.as_deref()));

        match select(ctx.input.recv(), ctx.api_changed()).await {
            Either::First(InputEvent::EncoderCCW) => return AppState::ConfirmStop,
            Either::First(_) => {}
            Either::Second(()) => {}
        }
    }
}

async fn execute_browse(mut index: usize, ctx: &mut Ctx<'_>) -> AppState {
    let mut deadline = Instant::now() + Duration::from_secs(MENU_INACTIVITY_TIMEOUT_SECS);

    loop {
        let snap = ctx.api.snapshot();

        if snap.is_offline() {
            return AppState::Offline;
        }
        if snap.is_cooking() {
            return AppState::Cooking {
                optimistic_recipe_title: None,
            };
        }
        if snap.recipes.is_empty() {
            return AppState::Idle;
        }

        index = index.min(snap.recipes.len() - 1);
        ctx.display.render(ViewSpec::RecipeBrowser {
            recipes: snap.recipes.clone(),
            index,
        });

        match select3(ctx.input.recv(), ctx.api_changed(), Timer::at(deadline)).await {
            Either3::First(InputEvent::EncoderCW) => {
                index = (index + 1).min(snap.recipes.len() - 1);
                deadline = Instant::now() + Duration::from_secs(MENU_INACTIVITY_TIMEOUT_SECS);
            }
            Either3::First(InputEvent::EncoderCCW) => {
                if index == 0 {
                    return AppState::Idle;
                }
                index -= 1;
                deadline = Instant::now() + Duration::from_secs(MENU_INACTIVITY_TIMEOUT_SECS);
            }
            Either3::First(InputEvent::EncoderButton) => {
                if let Some(recipe) = snap.recipes.get(index) {
                    return AppState::StartPending {
                        recipe_title: recipe.title.clone(),
                        recipe_id: recipe.id.clone(),
                        since: Instant::now(),
                    };
                }
            }
            Either3::Second(()) => {}
            Either3::Third(()) => return AppState::Idle,
        }
    }
}

async fn execute_start_pending(
    recipe_title: String,
    recipe_id: String,
    since: Instant,
    ctx: &mut Ctx<'_>,
) -> AppState {
    ctx.api.start(recipe_id);
    ctx.display.render(ViewSpec::StartingCook {
        recipe_title: recipe_title.clone(),
    });
    let deadline = since + Duration::from_secs(START_STOP_CONFIRM_TIMEOUT_SECS);

    loop {
        match select(ctx.api_changed(), Timer::at(deadline)).await {
            Either::First(()) => {
                let snap = ctx.api.snapshot();
                if snap.is_offline() {
                    return AppState::Offline;
                }
                if snap.is_cooking() {
                    return AppState::Cooking {
                        optimistic_recipe_title: Some(recipe_title.clone()),
                    };
                }
            }
            Either::Second(()) => {
                warn!("StartPending timed out without cook confirmation");
                return AppState::Idle;
            }
        }
    }
}

async fn execute_confirm_stop(ctx: &mut Ctx<'_>) -> AppState {
    let mut deadline = Instant::now() + Duration::from_secs(STOP_CONFIRM_TIMEOUT_SECS);

    loop {
        let snap = ctx.api.snapshot();
        if snap.is_offline() {
            return AppState::Offline;
        }
        if !snap.is_cooking() {
            return AppState::Idle;
        }

        ctx.display.render(ViewSpec::StopConfirmation {
            status: snap.status.clone(),
            cook: snap.current_cook.clone(),
        });

        match select3(ctx.input.recv(), ctx.api_changed(), Timer::at(deadline)).await {
            Either3::First(InputEvent::EncoderButton) => {
                return AppState::StopPending {
                    since: Instant::now(),
                };
            }
            Either3::First(InputEvent::EncoderCW) => {
                return AppState::Cooking {
                    optimistic_recipe_title: None,
                };
            }
            Either3::First(InputEvent::EncoderCCW) => {
                deadline = Instant::now() + Duration::from_secs(STOP_CONFIRM_TIMEOUT_SECS);
            }
            Either3::Second(()) => {}
            Either3::Third(()) => {
                return AppState::Cooking {
                    optimistic_recipe_title: None,
                };
            }
        }
    }
}

async fn execute_stop_pending(since: Instant, ctx: &mut Ctx<'_>) -> AppState {
    ctx.api.stop();
    let snap = ctx.api.snapshot();
    ctx.display.render(optimistic_idle_view(&snap));
    let deadline = since + Duration::from_secs(START_STOP_CONFIRM_TIMEOUT_SECS);

    loop {
        match select(ctx.api_changed(), Timer::at(deadline)).await {
            Either::First(()) => {
                let snap = ctx.api.snapshot();
                if snap.is_offline() {
                    return AppState::Offline;
                }
                if !snap.is_cooking() {
                    return AppState::Idle;
                }
            }
            Either::Second(()) => {
                warn!("StopPending timed out without idle confirmation");
                return AppState::Cooking {
                    optimistic_recipe_title: None,
                };
            }
        }
    }
}
