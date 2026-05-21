use alloc::string::String;

use defmt::warn;
use embassy_futures::select::{select, select3, Either, Either3};
use embassy_time::{Duration, Instant, Timer};

pub use anova_oven_pico_core::fsm::{AppState, BacklightPolicy};
use anova_oven_pico_core::fsm::{
    active_recipe_title, baseline_state_for, cooking_view, idle_view, next_stage_prompt,
    optimistic_idle_view, ViewSpec,
};

use crate::api_client::{ApiClient, StateReceiver};
use crate::backlight::BacklightController;
use crate::display::Display;
use crate::input::{Input, InputEvent};

const MENU_INACTIVITY_TIMEOUT_SECS: u64 = 15;
const STOP_CONFIRM_TIMEOUT_SECS: u64 = 5;
const START_STOP_CONFIRM_TIMEOUT_SECS: u64 = 10;

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

/// Top-level FSM tick. Records the breadcrumb, applies the backlight
/// policy, and dispatches to the per-state handler. Returns the next
/// state. `AppState` and the per-state decision/view helpers are pure
/// (in `anova-oven-pico-core::fsm`); the handlers below are where
/// effects live.
pub async fn execute(state: AppState, ctx: &mut Ctx<'_>) -> AppState {
    crate::persist::record_app_state(state.discriminant());
    ctx.backlight.apply(state.backlight_policy());

    match state {
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
        AppState::AwaitNextStage { next_description } => {
            execute_await_next_stage(next_description, ctx).await
        }
    }
}

async fn execute_offline(ctx: &mut Ctx<'_>) -> AppState {
    ctx.display.render(ViewSpec::ServerOffline);

    loop {
        match select(ctx.input.recv(), ctx.api_changed()).await {
            Either::First(_) => {
                // Keep draining local input while offline so encoder bursts do not
                // fill the queue during network outages.
            }
            Either::Second(()) => {
                let snap = ctx.api.snapshot();
                if !snap.is_offline() {
                    return baseline_state_for(&snap);
                }
            }
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
        if let Some(description) = next_stage_prompt(&snap) {
            return AppState::AwaitNextStage {
                next_description: description,
            };
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

async fn execute_await_next_stage(_next_description: String, ctx: &mut Ctx<'_>) -> AppState {
    loop {
        let snap = ctx.api.snapshot();

        if snap.is_offline() {
            return AppState::Offline;
        }
        if !snap.is_cooking() {
            return AppState::Idle;
        }
        // Server cleared next_stage_ready (e.g. another client advanced the
        // cook, or the flag was transient) — fall back to the cooking view.
        if next_stage_prompt(&snap).is_none() {
            return AppState::Cooking {
                optimistic_recipe_title: None,
            };
        }

        ctx.display.render(ViewSpec::NextStagePrompt {
            recipe_title: active_recipe_title(&snap),
        });

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
            count: snap.recipes.len(),
            index,
            title: snap.recipes[index].title.clone(),
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
        match select3(ctx.input.recv(), ctx.api_changed(), Timer::at(deadline)).await {
            Either3::First(_) => {}
            Either3::Second(()) => {
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
            Either3::Third(()) => {
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
        match select3(ctx.input.recv(), ctx.api_changed(), Timer::at(deadline)).await {
            Either3::First(_) => {}
            Either3::Second(()) => {
                let snap = ctx.api.snapshot();
                if snap.is_offline() {
                    return AppState::Offline;
                }
                if !snap.is_cooking() {
                    return AppState::Idle;
                }
            }
            Either3::Third(()) => {
                warn!("StopPending timed out without idle confirmation");
                return AppState::Cooking {
                    optimistic_recipe_title: None,
                };
            }
        }
    }
}
