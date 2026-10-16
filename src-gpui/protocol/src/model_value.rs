//! Session configOption model-id fallback — port of `src/lib/modelValue.ts`.
//!
//! Session configOption values spell a model two ways: the bridge's
//! `providerId\modelId` (backslash) and the agent-definition
//! `custom:<encoded provider>:<model>`. A current value that missed the
//! option list must still read as a name — dumping the raw pair is how the
//! same model came to display as a full id string in one place and a name in
//! another.

/// Percent-decode a URI component with JS `decodeURIComponent` semantics:
/// `%XX` sequences decode as bytes of a UTF-8 sequence; a malformed escape
/// (truncated hex pair) or an invalid UTF-8 sequence is an error, which the
/// caller turns into "return the raw segment" (the TS catch branch).
fn decode_uri_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // A '%' needs two hex digits after it; anything shorter is the
            // JS URIError branch.
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            let (hi, lo) = match (hi, lo) {
                (Some(h), Some(l)) => (h, l),
                _ => return None, // malformed escape → URIError in JS
            };
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The bare model id out of a configOption value, whichever spelling it uses.
///
/// Mirrors the TS regex `/^custom:([^:]+):(.+)$/`: the `custom:` prefix, a
/// non-empty provider segment without a raw colon (an encoded `%3A` is fine —
/// it is not a raw colon), the separator colon, then a non-empty remainder.
/// The remainder is percent-decoded; a malformed escape degrades to the raw
/// segment, not an error. Anything else falls to the backslash spelling:
/// everything after the LAST `\`, or the value unchanged.
pub fn bare_model_id_from_config_value(value: &str) -> String {
    if let Some(rest) = value.strip_prefix("custom:") {
        // `([^:]+):(.+)` — a non-empty provider run without ':', then ':',
        // then a non-empty model run.
        if let Some(sep) = rest.find(':') {
            let provider = &rest[..sep];
            let model = &rest[sep + 1..];
            if !provider.is_empty() && !model.is_empty() {
                return decode_uri_component(model).unwrap_or_else(|| model.to_owned());
            }
        }
    }
    match value.rfind('\\') {
        Some(idx) => value[idx + 1..].to_owned(),
        None => value.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Port of tests/model-value.test.ts — the session panel's current-model
    // fallback: a value that missed the option list must still read as a
    // model NAME, not the raw `provider\model` or `custom:…` spelling it
    // arrived as. Same logic as the config screens' labels (bare id unless
    // qualified), expressed against the two wire spellings.

    #[test]
    fn strips_the_provider_from_the_bridge_backslash_spelling() {
        assert_eq!(
            bare_model_id_from_config_value("builtin:bigmodel-coding-plan\\GLM-5.3"),
            "GLM-5.3"
        );
        assert_eq!(
            bare_model_id_from_config_value("uuid-1\\deepseek-v4"),
            "deepseek-v4"
        );
    }

    #[test]
    fn decodes_the_agent_definition_custom_spelling() {
        assert_eq!(
            bare_model_id_from_config_value(
                "custom:account%3Abigmodel-individual-coding-plan:GLM-5.3"
            ),
            "GLM-5.3"
        );
        // A malformed escape degrades to the raw segment, not a throw.
        assert_eq!(bare_model_id_from_config_value("custom:prov:bad%"), "bad%");
    }

    #[test]
    fn passes_a_bare_value_through_unchanged() {
        // Mode and thought options use bare words; they must not be mangled.
        assert_eq!(bare_model_id_from_config_value("max"), "max");
        assert_eq!(bare_model_id_from_config_value("build"), "build");
    }
}
