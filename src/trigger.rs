//! Build [`TriggerRequest`] messages from configuration objects.
//!
//! The watcher fires two kinds of triggers:
//!
//! * **Pattern triggers** ([`build_pattern_trigger`]): produced when a
//!   line of wrapped output matched a `[[patterns]]` entry. The trace id
//!   is `watch-<pattern_name>-<unix_ms>` so the daemon's history shows
//!   exactly which pattern fired.
//! * **Exit triggers** ([`build_exit_trigger`]): produced once when the
//!   wrapped command finishes, success or failure. The trace id is
//!   `watch-on-exit-<unix_ms>` so the daemon can distinguish them from
//!   pattern fires.
//!
//! Neither builder validates that the named sensation exists on the target
//! backend — that's the daemon's job at receipt. This module never fails;
//! all input combinations produce a well-formed [`TriggerRequest`].

use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Pattern;
use crate::proto::smited::v1::{trigger_request::Sensation, TriggerRequest};

/// Construct a [`TriggerRequest`] from a matched pattern.
///
/// Falls back to `default_backend_id` when the pattern has no `backend_id`
/// override. If the pattern's optional `intensity_scale` is unset, the
/// daemon falls back to the sensation's own default; same for `priority`,
/// which defaults to 0 in the proto when omitted by the client.
pub fn build_pattern_trigger(pattern: &Pattern, default_backend_id: &str) -> TriggerRequest {
    let backend_id = pattern
        .backend_id
        .as_deref()
        .unwrap_or(default_backend_id)
        .to_string();
    TriggerRequest {
        backend_id,
        zone_ids: Vec::new(),
        intensity_scale: pattern.intensity_scale,
        priority: pattern.priority.unwrap_or(0),
        client_trace_id: format!("watch-{}-{}", pattern.name, unix_ms_now()),
        sensation: Some(Sensation::SensationName(pattern.sensation.clone())),
    }
}

/// Construct the once-on-exit [`TriggerRequest`].
///
/// Used for both `success_sensation` and `failure_sensation`; the caller
/// chooses which name to pass. Trace id is `watch-on-exit-<unix_ms>`.
pub fn build_exit_trigger(sensation: &str, backend_id: &str) -> TriggerRequest {
    TriggerRequest {
        backend_id: backend_id.to_string(),
        zone_ids: Vec::new(),
        intensity_scale: None,
        priority: 0,
        client_trace_id: format!("watch-on-exit-{}", unix_ms_now()),
        sensation: Some(Sensation::SensationName(sensation.to_string())),
    }
}

fn unix_ms_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pat_min(name: &str, sensation: &str) -> Pattern {
        Pattern {
            name: name.into(),
            regex: ".".into(),
            sensation: sensation.into(),
            backend_id: None,
            debounce_ms: 500,
            intensity_scale: None,
            priority: None,
            compiled: None,
        }
    }

    #[test]
    fn pattern_trigger_uses_default_backend_when_pattern_omits_one() {
        let p = pat_min("ts", "compile_error_mild");
        let req = build_pattern_trigger(&p, "mock-owo");
        assert_eq!(req.backend_id, "mock-owo");
    }

    #[test]
    fn pattern_trigger_uses_pattern_backend_override_when_present() {
        let mut p = pat_min("ts", "compile_error_mild");
        p.backend_id = Some("owo-primary".into());
        let req = build_pattern_trigger(&p, "mock-owo");
        assert_eq!(req.backend_id, "owo-primary");
    }

    #[test]
    fn pattern_trigger_sets_sensation_oneof_to_name() {
        let p = pat_min("ts", "compile_error_mild");
        let req = build_pattern_trigger(&p, "mock-owo");
        match req.sensation {
            Some(Sensation::SensationName(name)) => assert_eq!(name, "compile_error_mild"),
            other => panic!("expected SensationName variant, got {other:?}"),
        }
    }

    #[test]
    fn pattern_trigger_propagates_optional_intensity_and_priority() {
        let mut p = pat_min("ts", "compile_error_mild");
        p.intensity_scale = Some(75);
        p.priority = Some(25);
        let req = build_pattern_trigger(&p, "mock-owo");
        assert_eq!(req.intensity_scale, Some(75));
        assert_eq!(req.priority, 25);
    }

    #[test]
    fn pattern_trigger_priority_defaults_to_zero() {
        let p = pat_min("ts", "compile_error_mild");
        let req = build_pattern_trigger(&p, "mock-owo");
        assert_eq!(req.priority, 0);
    }

    #[test]
    fn pattern_trigger_intensity_defaults_to_none() {
        let p = pat_min("ts", "compile_error_mild");
        let req = build_pattern_trigger(&p, "mock-owo");
        assert_eq!(req.intensity_scale, None);
    }

    #[test]
    fn pattern_trigger_trace_id_format_includes_pattern_name() {
        let p = pat_min("vite_error", "compile_error_severe");
        let req = build_pattern_trigger(&p, "mock-owo");
        assert!(
            req.client_trace_id.starts_with("watch-vite_error-"),
            "got {}",
            req.client_trace_id
        );
        // Trailing portion is unix-ms; just sanity-check it's all digits.
        let suffix = req.client_trace_id.trim_start_matches("watch-vite_error-");
        assert!(
            !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()),
            "got suffix {suffix:?}"
        );
    }

    #[test]
    fn exit_trigger_trace_id_format() {
        let req = build_exit_trigger("deploy_success", "mock-owo");
        assert!(
            req.client_trace_id.starts_with("watch-on-exit-"),
            "got {}",
            req.client_trace_id
        );
        assert_eq!(req.backend_id, "mock-owo");
        match req.sensation {
            Some(Sensation::SensationName(n)) => assert_eq!(n, "deploy_success"),
            other => panic!("expected SensationName, got {other:?}"),
        }
        assert_eq!(req.intensity_scale, None);
        assert_eq!(req.priority, 0);
    }
}
