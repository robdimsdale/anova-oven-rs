//! Recipe-stage normalization for the Anova v2 protocol.
//!
//! The oven appears to bind a preheat stage to its successor cook stage by
//! their shared base UUID — the iOS app sends preheat IDs of the form
//! `<cook_stage_id>-preheat`. Recipes loaded from Firestore have unrelated
//! UUIDs per stage; sending those verbatim causes the oven to reject
//! `CMD_APO_START_STAGE` for the cook stage with `unauthorized`. This helper
//! rewrites preheat stage IDs to match the iOS pattern before the cook is
//! sent on the wire.

use anova_oven_api::Stage;

/// In-place rewrite preheat stage IDs to `<next_stage_id>-preheat` for any
/// preheat stage immediately followed by a non-preheat stage with an `id`.
/// Stages already using the `-preheat` suffix are left unchanged.
pub fn rewrite_preheat_stage_ids(stages: &mut [Stage]) {
    for i in 0..stages.len().saturating_sub(1) {
        if stages[i].kind != "preheat" {
            continue;
        }
        if stages[i + 1].kind == "preheat" {
            continue;
        }
        let Some(next_id) = stages[i + 1].id.clone() else {
            continue;
        };
        let new_id = format!("{next_id}-preheat");
        if stages[i].id.as_deref() == Some(new_id.as_str()) {
            continue;
        }
        stages[i].id = Some(new_id);
    }
}

#[cfg(test)]
mod tests {
    use super::rewrite_preheat_stage_ids;
    use anova_oven_api::Stage;

    fn stage(id: Option<&str>, kind: &str) -> Stage {
        Stage {
            id: id.map(Into::into),
            kind: kind.into(),
            temperature_c: 25.0,
            temperature_bulbs_mode: Some("wet".into()),
            duration_secs: None,
            timer_added: None,
            probe_added: None,
            probe_target_c: None,
            steam_pct: 0.0,
            fan_speed: 0,
            user_action_required: None,
            rack_position: None,
            heating_element_top: None,
            heating_element_rear: None,
            heating_element_bottom: None,
            vent_open: None,
            title: None,
        }
    }

    #[test]
    fn rewrites_preheat_followed_by_cook() {
        let mut s = vec![
            stage(Some("preheat-uuid"), "preheat"),
            stage(Some("cook-uuid"), "cook"),
        ];
        rewrite_preheat_stage_ids(&mut s);
        assert_eq!(s[0].id.as_deref(), Some("cook-uuid-preheat"));
        assert_eq!(s[1].id.as_deref(), Some("cook-uuid"));
    }

    #[test]
    fn leaves_solo_preheat_untouched() {
        let mut s = vec![stage(Some("uuid"), "preheat")];
        rewrite_preheat_stage_ids(&mut s);
        assert_eq!(s[0].id.as_deref(), Some("uuid"));
    }

    #[test]
    fn leaves_already_suffixed_unchanged() {
        let mut s = vec![
            stage(Some("cook-uuid-preheat"), "preheat"),
            stage(Some("cook-uuid"), "cook"),
        ];
        rewrite_preheat_stage_ids(&mut s);
        assert_eq!(s[0].id.as_deref(), Some("cook-uuid-preheat"));
    }

    #[test]
    fn skips_when_next_stage_has_no_id() {
        let mut s = vec![stage(Some("uuid"), "preheat"), stage(None, "cook")];
        rewrite_preheat_stage_ids(&mut s);
        assert_eq!(s[0].id.as_deref(), Some("uuid"));
    }
}
