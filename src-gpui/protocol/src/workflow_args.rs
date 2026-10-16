//! Launch-args helpers for saved dynamic workflows — port of
//! `src/lib/workflow-args.ts` plus the typed form's report computation
//! (`src/components/config/WorkflowArgsForm.tsx`), the pure halves the
//! desktop client renders from.
//!
//! Two form shapes share this module: the TYPED form (the workflow's meta
//! declares its args — string / number / boolean inputs, anything else a
//! per-field JSON textarea) and the FREE-FORM fallback (one JSON textarea).
//! Both produce an args object for `POST …/start`; the typed form may
//! instead produce the error that keeps the submit disabled.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::workflow::WorkflowArgDeclaration;

/// The free-form textarea's parse result (TS `parseArgsInput`): `""` = no
/// args; an error keeps the submit disabled.
#[derive(Debug, Clone, PartialEq)]
pub struct ArgsInput {
    pub args: Option<Map<String, Value>>,
    pub error: Option<String>,
}

/// Parse the optional start-args textarea; "" = no args.
pub fn parse_args_input(text: &str) -> ArgsInput {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return ArgsInput {
            args: None,
            error: None,
        };
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Object(map)) => ArgsInput {
            args: Some(map),
            error: None,
        },
        Ok(_) => ArgsInput {
            args: None,
            error: Some("args must be a JSON object".to_string()),
        },
        Err(_) => ArgsInput {
            args: None,
            error: Some("invalid JSON".to_string()),
        },
    }
}

/// Extract the declared-args map from a workflow's meta (upstream
/// `zcodeSavedWorkflowArgsDeclarationSchema`). `None` when the workflow
/// declares no args — the launcher then falls back to the free-form JSON
/// textarea. Insertion order is preserved by the serde_json Map on the wire.
pub fn args_declaration(meta: Option<&Value>) -> Option<BTreeMap<String, WorkflowArgDeclaration>> {
    let raw = meta?.get("args")?.as_object()?;
    let mut out = BTreeMap::new();
    for (k, v) in raw {
        let Some(fields) = v.as_object() else {
            continue;
        };
        // A non-object entry is skipped (TS `continue`) — it cannot drive a
        // field and must not break the rest of the declaration.
        let decl = WorkflowArgDeclaration {
            r#type: fields.get("type").and_then(Value::as_str).map(str::to_string),
            description: fields
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            required: fields.get("required").and_then(Value::as_bool),
            default: fields.get("default").cloned(),
        };
        out.insert(k.clone(), decl);
    }
    (!out.is_empty()).then_some(out)
}

/// The typed form's mount values (TS WorkflowArgsForm's mount effect):
/// booleans prefill their declared `default: true` so the first report
/// carries them; everything else starts empty.
pub fn args_initial_values(
    decls: &BTreeMap<String, WorkflowArgDeclaration>,
) -> BTreeMap<String, Value> {
    let mut initial = BTreeMap::new();
    for (name, decl) in decls {
        if decl.r#type.as_deref().unwrap_or("string") == "boolean" && decl.default == Some(Value::Bool(true))
        {
            initial.insert(name.clone(), Value::Bool(true));
        }
    }
    initial
}

/// The typed form's per-change report (TS WorkflowArgsForm `report`):
/// derive the args object from the field values (string/number/boolean)
/// and the raw text of JSON fields; the first problem becomes the error
/// that disables the submit. Unset optional fields are simply absent —
/// the backend applies declared defaults itself.
pub fn typed_args_report(
    decls: &BTreeMap<String, WorkflowArgDeclaration>,
    values: &BTreeMap<String, Value>,
    json_texts: &BTreeMap<String, String>,
) -> ArgsInput {
    let mut args = Map::new();
    let mut error: Option<String> = None;
    for (name, decl) in decls {
        let field_type = decl.r#type.clone().unwrap_or_else(|| "string".to_string());
        let value = values.get(name);
        match field_type.as_str() {
            "number" => match value {
                Some(Value::Number(n)) if n.as_f64().is_some_and(f64::is_finite) => {
                    args.insert(name.clone(), Value::Number(n.clone()));
                }
                // The input layer hands the RAW text (TS `Number(text)`):
                // blank = absent, parseable = the number, else the error.
                Some(Value::String(s)) => {
                    let text = s.trim();
                    if !text.is_empty() {
                        match text.parse::<f64>() {
                            Ok(n) if n.is_finite() => {
                                let num = if n.fract() == 0.0 && n.abs() < 1e15 {
                                    serde_json::Number::from(n as i64)
                                } else {
                                    // Guarded finite, so from_f64 is Some.
                                    serde_json::Number::from_f64(n).unwrap()
                                };
                                args.insert(name.clone(), Value::Number(num));
                            }
                            _ => error = Some(format!("{name}: invalid number")),
                        }
                    }
                }
                _ => {}
            },
            "boolean" => {
                // Booleans always carry a value (desktop contract):
                // unchecked = false.
                args.insert(name.clone(), Value::Bool(value == Some(&Value::Bool(true))));
            }
            "string" => {
                if let Some(Value::String(s)) = value {
                    let s = s.trim();
                    if !s.is_empty() {
                        args.insert(name.clone(), Value::String(s.to_string()));
                    }
                }
            }
            _ => {
                let text = json_texts
                    .get(name)
                    .map(String::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !text.is_empty() {
                    match serde_json::from_str::<Value>(&text) {
                        Ok(v) => {
                            args.insert(name.clone(), v);
                        }
                        Err(_) => error = Some(format!("{name}: invalid JSON")),
                    }
                }
            }
        }
        // Required only when the backend has no declared default to fall
        // back on — a required+default field may stay empty; the server
        // fills it.
        if error.is_none()
            && decl.required.unwrap_or(false)
            && decl.default.is_none()
            && !args.contains_key(name)
        {
            error = Some(format!("{name} is required"));
        }
    }
    if error.is_some() {
        ArgsInput {
            args: None,
            error,
        }
    } else {
        ArgsInput {
            args: Some(args),
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decls_json() -> Value {
        serde_json::json!({
            "args": {
                "target": { "type": "string", "required": true },
                "limit": { "type": "number", "default": 5 },
                "verbose": { "type": "boolean", "default": true },
                "filter": { "type": "object" },
                "broken": "not-an-object"
            }
        })
    }

    #[test]
    fn declaration_extracts_object_entries_only() {
        let meta = decls_json();
        let decls = args_declaration(Some(&meta)).expect("declaration");
        assert_eq!(decls.len(), 4, "the non-object entry is skipped");
        assert_eq!(decls["target"].r#type.as_deref(), Some("string"));
        assert_eq!(decls["target"].required, Some(true));
        assert_eq!(decls["limit"].default, Some(serde_json::json!(5)));
        assert!(!decls.contains_key("broken"));
    }

    #[test]
    fn declaration_is_none_without_an_args_object() {
        assert!(args_declaration(None).is_none());
        assert!(args_declaration(Some(&serde_json::json!({}))).is_none());
        assert!(args_declaration(Some(&serde_json::json!({ "args": [] }))).is_none());
        // An args object with no usable entry also falls back to free-form.
        assert!(args_declaration(Some(&serde_json::json!({ "args": { "x": 1 } }))).is_none());
    }

    #[test]
    fn initial_values_prefill_defaulted_booleans_only() {
        let decls = args_declaration(Some(&decls_json())).unwrap();
        let initial = args_initial_values(&decls);
        assert_eq!(initial.len(), 1);
        assert_eq!(initial["verbose"], Value::Bool(true));
    }

    #[test]
    fn typed_report_assembles_and_validates() {
        let decls = args_declaration(Some(&decls_json())).unwrap();
        let mut values = args_initial_values(&decls);
        values.insert("target".into(), Value::String("src/".into()));
        values.insert("limit".into(), serde_json::json!(12));
        let mut json = BTreeMap::new();
        json.insert("filter".to_string(), "{\"kind\": \"doc\"}".to_string());

        let report = typed_args_report(&decls, &values, &json);
        assert_eq!(report.error, None);
        let args = report.args.unwrap();
        assert_eq!(args["target"], serde_json::json!("src/"));
        assert_eq!(args["limit"], serde_json::json!(12));
        assert_eq!(args["verbose"], serde_json::json!(true));
        assert_eq!(args["filter"], serde_json::json!({ "kind": "doc" }));
    }

    #[test]
    fn typed_report_gates_on_required_without_default() {
        let decls = args_declaration(Some(&decls_json())).unwrap();
        let values = args_initial_values(&decls);
        let report = typed_args_report(&decls, &values, &BTreeMap::new());
        assert_eq!(report.error.as_deref(), Some("target is required"));
        assert!(report.args.is_none());
    }

    #[test]
    fn typed_report_rejects_bad_number_and_bad_json() {
        let decls = args_declaration(Some(&decls_json())).unwrap();
        let mut values = args_initial_values(&decls);
        values.insert("target".into(), Value::String("ok".into()));
        values.insert("limit".into(), Value::String("abc".into()));
        let report = typed_args_report(&decls, &values, &BTreeMap::new());
        assert_eq!(report.error.as_deref(), Some("limit: invalid number"));

        let mut values = args_initial_values(&decls);
        values.insert("target".into(), Value::String("ok".into()));
        let mut json = BTreeMap::new();
        json.insert("filter".to_string(), "{nope".to_string());
        let report = typed_args_report(&decls, &values, &json);
        assert_eq!(report.error.as_deref(), Some("filter: invalid JSON"));
    }

    #[test]
    fn number_raw_text_parses_like_js_number() {
        // The input layer stores raw text; the report owes JS `Number()`
        // semantics — "12" becomes the integer 12, "2.5" the decimal.
        let decls = args_declaration(Some(&decls_json())).unwrap();
        let mut values = args_initial_values(&decls);
        values.insert("target".into(), Value::String("ok".into()));
        values.insert("limit".into(), Value::String("12".into()));
        let report = typed_args_report(&decls, &values, &BTreeMap::new());
        assert!(report.error.is_none());
        assert_eq!(report.args.unwrap()["limit"], serde_json::json!(12));

        let mut values = args_initial_values(&decls);
        values.insert("target".into(), Value::String("ok".into()));
        values.insert("limit".into(), Value::String("2.5".into()));
        let report = typed_args_report(&decls, &values, &BTreeMap::new());
        assert!(report.error.is_none());
        assert_eq!(report.args.unwrap()["limit"], serde_json::json!(2.5));
    }

    #[test]
    fn number_blank_is_absent_but_defaulted_required_passes() {
        // `limit` is not required and carries a default — blank stays absent.
        let decls = args_declaration(Some(&decls_json())).unwrap();
        let mut values = args_initial_values(&decls);
        values.insert("target".into(), Value::String("x".into()));
        values.insert("limit".into(), Value::String(String::new()));
        let report = typed_args_report(&decls, &values, &BTreeMap::new());
        assert!(report.error.is_none());
        assert!(!report.args.as_ref().unwrap().contains_key("limit"));
    }

    #[test]
    fn free_form_parse_shapes() {
        assert_eq!(parse_args_input(""), ArgsInput { args: None, error: None });
        assert_eq!(
            parse_args_input("  "),
            ArgsInput { args: None, error: None }
        );
        let ok = parse_args_input("{\"target\": \"src/\"}");
        assert_eq!(ok.error, None);
        assert_eq!(
            ok.args.unwrap().get("target"),
            Some(&serde_json::json!("src/"))
        );
        assert_eq!(
            parse_args_input("[1,2]").error.as_deref(),
            Some("args must be a JSON object")
        );
        assert_eq!(
            parse_args_input("{nope").error.as_deref(),
            Some("invalid JSON")
        );
    }
}
