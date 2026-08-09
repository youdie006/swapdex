//! Keeping the request's account identity consistent with the token serving it.
//!
//! Every turn carries `metadata.user_id`, a JSON-encoded string that embeds the
//! account's own UUID verbatim (observed 2026-07-27). Serving a turn with account
//! B's token while that field still names account A leaves the request
//! internally inconsistent, so the UUID is substituted. Because the UUID appears
//! literally, this needs no knowledge of the field's inner key names - and only
//! UUIDs belonging to swapdex-managed accounts are ever substituted, so nothing
//! else in the body can be caught by accident. The prompt is never touched.

/// Rewrite `metadata.user_id` so any known account UUID other than `serving`
/// becomes `serving`. Returns `None` when there is nothing to change (not JSON,
/// no such field, or the identity already matches), so the caller forwards the
/// original bytes untouched.
pub fn align_account(body: &[u8], known: &[String], serving: &str) -> Option<Vec<u8>> {
    if serving.is_empty() {
        return None;
    }
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let uid = v.get("metadata")?.get("user_id")?.as_str()?;
    let mut out = uid.to_string();
    for other in known {
        if other != serving && !other.is_empty() && out.contains(other.as_str()) {
            out = out.replace(other.as_str(), serving);
        }
    }
    if out == uid {
        return None;
    }
    *v.get_mut("metadata")?.get_mut("user_id")? = serde_json::Value::String(out);
    serde_json::to_vec(&v).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "11111111-2222-3333-4444-555555555555";
    const B: &str = "66666666-7777-8888-9999-000000000000";

    fn body_with(uid: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "model": "claude-opus-5",
            "messages": [{"role": "user", "content": "keep me intact"}],
            "metadata": {"user_id": uid},
        }))
        .unwrap()
    }

    #[test]
    fn the_serving_accounts_uuid_replaces_the_other_ones() {
        let uid = format!("{{\"device_id\":\"d1\",\"account_uuid\":\"{A}\",\"session\":\"{A}\"}}");
        let out = align_account(&body_with(&uid), &[A.into(), B.into()], B).expect("rewritten");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let new_uid = v["metadata"]["user_id"].as_str().unwrap();
        assert!(!new_uid.contains(A), "the other account's uuid is gone");
        assert_eq!(new_uid.matches(B).count(), 2, "both occurrences replaced");
        // Nothing else changed.
        assert_eq!(v["model"], "claude-opus-5");
        assert_eq!(v["messages"][0]["content"], "keep me intact");
    }

    #[test]
    fn nothing_to_change_returns_none_so_the_body_is_forwarded_verbatim() {
        let uid = format!("{{\"account_uuid\":\"{B}\"}}");
        // Already the serving account.
        assert!(align_account(&body_with(&uid), &[A.into(), B.into()], B).is_none());
        // A uuid we do not manage is left alone.
        let foreign = "{\"account_uuid\":\"deadbeef-0000-0000-0000-000000000000\"}";
        assert!(align_account(&body_with(foreign), &[A.into(), B.into()], B).is_none());
    }

    #[test]
    fn a_body_without_the_field_or_without_json_is_left_alone() {
        let no_meta = serde_json::to_vec(&serde_json::json!({"model": "x"})).unwrap();
        assert!(align_account(&no_meta, &[A.into()], B).is_none());
        assert!(align_account(b"not json at all", &[A.into()], B).is_none());
        // An empty serving identity must never blank out a real one.
        let uid = format!("{{\"account_uuid\":\"{A}\"}}");
        assert!(align_account(&body_with(&uid), &[A.into()], "").is_none());
    }
}

/// Swap the model a request asks for, when the account serving it has nothing
/// left and there is nowhere else to go.
///
/// This is the last thing swapdex will do before a turn fails, never the first.
/// Rotating to an account with room is always better, because it gives the user
/// what they asked for; changing the model gives them something else. So it is
/// opt-in, it only applies once every account is past the threshold, and the
/// proxy says so on the turn it happens.
///
/// `None` when there is nothing to change - no model field, or it already asks
/// for the fallback - so a caller can tell "rewritten" from "left alone".
pub fn swap_model(body: &[u8], fallback: &str) -> Option<Vec<u8>> {
    if fallback.is_empty() {
        return None;
    }
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let current = v.get("model")?.as_str()?;
    if current == fallback {
        return None;
    }
    *v.get_mut("model")? = serde_json::Value::String(fallback.to_string());
    serde_json::to_vec(&v).ok()
}

#[cfg(test)]
mod model_tests {
    use super::*;

    fn model_of(b: &[u8]) -> String {
        serde_json::from_slice::<serde_json::Value>(b).unwrap()["model"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn the_model_is_replaced_and_nothing_else_is() {
        let body = br#"{"model":"claude-opus-5","max_tokens":100,"messages":[{"role":"user"}]}"#;
        let out = swap_model(body, "claude-sonnet-5").expect("rewritten");
        assert_eq!(model_of(&out), "claude-sonnet-5");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["max_tokens"], 100, "the rest of the request is untouched");
        assert!(v["messages"].is_array());
    }

    /// "Nothing to do" has to be distinguishable from "done", so the caller only
    /// announces a change that happened.
    #[test]
    fn asking_for_the_fallback_already_is_left_alone() {
        let body = br#"{"model":"claude-sonnet-5"}"#;
        assert!(swap_model(body, "claude-sonnet-5").is_none());
    }

    #[test]
    fn a_request_with_no_model_is_left_alone() {
        assert!(swap_model(br#"{"messages":[]}"#, "claude-sonnet-5").is_none());
        assert!(swap_model(b"not json", "claude-sonnet-5").is_none());
    }

    /// An empty setting is how "off" is spelled, and off must never rewrite.
    #[test]
    fn no_fallback_configured_changes_nothing() {
        assert!(swap_model(br#"{"model":"claude-opus-5"}"#, "").is_none());
    }
}
