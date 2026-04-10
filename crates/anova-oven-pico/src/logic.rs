pub fn is_active_cook(current_cook_present: bool, status_mode: Option<&str>) -> bool {
    current_cook_present || status_mode.is_some_and(|mode| mode != "idle")
}

pub fn should_dim_backlight(
    is_show_status: bool,
    baseline_elapsed_secs: Option<u64>,
    active_cook: bool,
    led_dim_timer_secs: u64,
) -> bool {
    is_show_status
        && baseline_elapsed_secs.is_some_and(|elapsed| elapsed >= led_dim_timer_secs)
        && !active_cook
}

#[cfg(test)]
mod tests {
    use super::{is_active_cook, should_dim_backlight};

    #[test]
    fn active_cook_when_current_cook_exists() {
        assert!(is_active_cook(true, Some("idle")));
    }

    #[test]
    fn active_cook_when_status_mode_not_idle() {
        assert!(is_active_cook(false, Some("cooking")));
    }

    #[test]
    fn inactive_cook_when_idle_and_no_current_cook() {
        assert!(!is_active_cook(false, Some("idle")));
    }

    #[test]
    fn inactive_cook_when_status_unknown_and_no_current_cook() {
        assert!(!is_active_cook(false, None));
    }

    #[test]
    fn dims_only_after_baseline_timeout_when_not_active() {
        assert!(should_dim_backlight(true, Some(5), false, 5));
        assert!(!should_dim_backlight(true, Some(4), false, 5));
        assert!(!should_dim_backlight(false, Some(10), false, 5));
        assert!(!should_dim_backlight(true, Some(10), true, 5));
    }
}
