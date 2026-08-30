//! n8n node-type -> tinyflows `NodeKind` + config mapping.

use serde_json::{Map, Value, json};
use tinyflows::model::NodeKind;

use super::expr::translate_config;

/// Maps a single n8n node `type` + `parameters` to a tinyflows kind and config.
/// Unrecognized types return a `transform` placeholder carrying the original
/// type/params under `_n8n_import` and record a warning.
pub(super) fn map_node(
    n8n_type: &str,
    params: &Value,
    n8n_name: &str,
    warnings: &mut Vec<String>,
) -> (NodeKind, Value) {
    // Strip the vendor prefix so both `n8n-nodes-base.if` and a bare `if` match.
    let short = n8n_type
        .rsplit_once('.')
        .map(|(_, s)| s)
        .unwrap_or(n8n_type);

    match short {
        "if" => (
            NodeKind::Condition,
            map_condition(params, warnings, n8n_name),
        ),
        "switch" => (NodeKind::Switch, map_switch(params, warnings, n8n_name)),
        "merge" => (
            NodeKind::Merge,
            translate_config(params, warnings, n8n_name),
        ),
        "splitOut" => (
            NodeKind::SplitOut,
            map_split_out(params, warnings, n8n_name),
        ),
        // `itemLists` is a multi-operation node (split-out, aggregate, sort,
        // remove-duplicates, ...); only its split-out operation has a
        // tinyflows equivalent. An explicit non-split-out `operation` falls
        // through to the unmapped-type placeholder below instead of being
        // force-mapped to `split_out`, which would silently discard whatever
        // the node actually did.
        "itemLists"
            if params
                .get("operation")
                .and_then(Value::as_str)
                .is_none_or(|op| op == "splitOutItems") =>
        {
            (
                NodeKind::SplitOut,
                map_split_out(params, warnings, n8n_name),
            )
        }
        "httpRequest" => (
            NodeKind::HttpRequest,
            map_http_request(params, warnings, n8n_name),
        ),
        "code" | "function" | "functionItem" => map_code_node(params, warnings, n8n_name),
        "scheduleTrigger" | "cron" | "interval" => (
            NodeKind::Trigger,
            trigger_config("schedule", params, warnings, n8n_name),
        ),
        "webhook" => (
            NodeKind::Trigger,
            trigger_config("webhook", params, warnings, n8n_name),
        ),
        "manualTrigger" | "start" => (
            NodeKind::Trigger,
            trigger_config("manual", params, warnings, n8n_name),
        ),
        _ => {
            warnings.push(format!(
                "Node '{n8n_name}' has n8n type '{n8n_type}', which has no tinyflows equivalent — \
                 imported as an editable placeholder that carries its original configuration. \
                 Replace it with a supported node before enabling the flow."
            ));
            let config = json!({
                "_n8n_import": {
                    "original_type": n8n_type,
                    "note": "Unmapped n8n node imported as a placeholder; original parameters preserved below.",
                },
                "parameters": params,
            });
            (NodeKind::Transform, config)
        }
    }
}

/// Builds a tinyflows `trigger` config carrying the given `trigger_kind`
/// discriminator plus any (expression-translated) source parameters. For
/// `trigger_kind: "schedule"`, also attempts to derive `config.schedule` — the
/// `{kind:"cron",expr,tz?} | {kind:"at",at} | {kind:"every",every_ms}` shape
/// the tinyflows trigger contract requires — from n8n's own (versioned, node-
/// type-dependent) schedule parameters, falling back to a warning when no
/// shape can be confidently derived.
pub(super) fn trigger_config(
    trigger_kind: &str,
    params: &Value,
    warnings: &mut Vec<String>,
    n8n_name: &str,
) -> Value {
    let mut cfg = match translate_config(params, warnings, n8n_name) {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    cfg.insert(
        "trigger_kind".to_string(),
        Value::String(trigger_kind.to_string()),
    );
    if trigger_kind == "schedule" {
        match derive_schedule(params) {
            Some(schedule) => {
                cfg.insert("schedule".to_string(), schedule);
            }
            None => {
                warnings.push(format!(
                    "Node '{n8n_name}' is a schedule trigger, but its n8n timing parameters \
                     could not be translated to tinyflows' `config.schedule` shape \
                     (`{{kind:\"cron\",expr}}` / `{{kind:\"at\",at}}` / `{{kind:\"every\",every_ms}}`). \
                     The flow imported without a schedule and will not fire on a timer until you \
                     set one."
                ));
            }
        }
    }
    Value::Object(cfg)
}

/// Best-effort translation of n8n's schedule parameters to tinyflows' schedule
/// shape, covering the common, well-defined cases:
/// - `cron` node: a raw `cronExpression` string.
/// - `interval` node: `unit` (`seconds`/`minutes`/`hours`) + numeric `value`.
/// - `scheduleTrigger` node: the first `rule.interval` entry, when it is
///   itself a bare cron expression (`field: "cronExpression"`) or a fixed
///   unit/value interval (`field: "seconds"/"minutes"/"hours"/"days"`).
///
/// Returns `None` — rather than guessing — for anything richer (multiple
/// intervals, weekday/month-day schedules, timezone-qualified rules), which
/// n8n's schedule trigger versions represent in ways that vary enough between
/// versions that a wrong guess would be worse than an honest gap.
fn derive_schedule(params: &Value) -> Option<Value> {
    if let Some(expr) = params.get("cronExpression").and_then(Value::as_str) {
        return Some(json!({ "kind": "cron", "expr": expr }));
    }
    if let (Some(unit), Some(value)) = (
        params.get("unit").and_then(Value::as_str),
        params.get("value").and_then(Value::as_f64),
    ) {
        return interval_to_every_ms(unit, value)
            .map(|ms| json!({ "kind": "every", "every_ms": ms }));
    }
    let rules = params
        .get("rule")
        .and_then(|r| r.get("interval"))
        .and_then(Value::as_array)?;
    // Tinyflows currently stores one schedule per trigger. Silently choosing
    // one cadence from an n8n trigger that has several would change when the
    // imported workflow runs, so leave it unscheduled and let the caller emit
    // the actionable fallback warning.
    if rules.len() != 1 {
        return None;
    }
    let first_rule = rules.first()?;
    let field = first_rule.get("field").and_then(Value::as_str)?;
    if field == "cronExpression" {
        let expr = first_rule.get("expression").and_then(Value::as_str)?;
        return Some(json!({ "kind": "cron", "expr": expr }));
    }
    // n8n's `scheduleTrigger` node has spelled the count differently across
    // versions: a bare unit key (`hours`), `<unit>Interval`
    // (`hoursInterval`), or a generic `value` — try each in turn.
    let interval_key = format!("{field}Interval");
    let value = first_rule
        .get(field)
        .and_then(Value::as_f64)
        .or_else(|| {
            first_rule
                .get(interval_key.as_str())
                .and_then(Value::as_f64)
        })
        .or_else(|| first_rule.get("value").and_then(Value::as_f64))?;
    interval_to_every_ms(field, value).map(|ms| json!({ "kind": "every", "every_ms": ms }))
}

/// Converts an n8n unit name + count to milliseconds, for the fixed-interval
/// (not calendar-based) units n8n exposes.
fn interval_to_every_ms(unit: &str, value: f64) -> Option<f64> {
    let ms_per_unit = match unit {
        "seconds" => 1_000.0,
        "minutes" => 60_000.0,
        "hours" => 3_600_000.0,
        "days" => 86_400_000.0,
        _ => return None,
    };
    Some(value * ms_per_unit)
}

/// Maps n8n `if` parameters onto tinyflows' `condition` config.
///
/// n8n's `conditions` structure (operator-typed comparison groups, versioned
/// across n8n releases) has no tinyflows equivalent — `condition` only reads a
/// truthiness check on `config.field`. Rather than guess at a translation that
/// would confidently route the WRONG branch, the original `conditions` are
/// preserved read-only under `_n8n_import` and a warning tells the author the
/// predicate needs to be rebuilt as a tinyflows `field`/`=`-expression.
pub(super) fn map_condition(params: &Value, warnings: &mut Vec<String>, n8n_name: &str) -> Value {
    let mut cfg = match translate_config(params, warnings, n8n_name) {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    if params.get("conditions").is_some() && !cfg.contains_key("field") {
        warnings.push(format!(
            "Node '{n8n_name}' is an n8n IF node with a `conditions` comparison this importer \
             cannot translate — tinyflows' condition node only checks the truthiness of a single \
             `field`. The original conditions were preserved under `_n8n_import` for reference; \
             rebuild the predicate as a `field` or `=`-expression before enabling the flow."
        ));
        cfg.insert(
            "_n8n_import".to_string(),
            json!({
                "original_type": "if",
                "note": "Untranslated n8n conditions preserved below; rebuild as a tinyflows field/expression.",
                "conditions": params.get("conditions").cloned().unwrap_or(Value::Null),
            }),
        );
    }
    Value::Object(cfg)
}

/// Maps n8n `switch` parameters onto tinyflows' `switch` config.
///
/// n8n's `rules` structure (a list of typed comparison groups, one per output)
/// has no tinyflows equivalent — `switch` only routes on the literal value of
/// a `field` or `=`-expression. When the source carries `rules` but no
/// translatable `field`/`expression` survived, every input would silently
/// route to the `default` port, so the original rules are preserved read-only
/// and a warning tells the author to rebuild the routing.
pub(super) fn map_switch(params: &Value, warnings: &mut Vec<String>, n8n_name: &str) -> Value {
    let mut cfg = match translate_config(params, warnings, n8n_name) {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let has_rules = params
        .get("rules")
        .or_else(|| params.get("rules").and_then(|r| r.get("values")))
        .is_some();
    if has_rules && !cfg.contains_key("field") && !cfg.contains_key("expression") {
        warnings.push(format!(
            "Node '{n8n_name}' is an n8n switch node with a `rules` structure this importer \
             cannot translate — tinyflows' switch node only routes on the literal value of a \
             `field` or `=`-expression, and every branch currently falls through to `default`. \
             The original rules were preserved under `_n8n_import` for reference; rebuild the \
             routing as a `field`/`expression` (and rename the downstream edges' ports to match) \
             before enabling the flow."
        ));
        cfg.insert(
            "_n8n_import".to_string(),
            json!({
                "original_type": "switch",
                "note": "Untranslated n8n rules preserved below; rebuild as a tinyflows field/expression.",
                "rules": params.get("rules").cloned().unwrap_or(Value::Null),
            }),
        );
    }
    Value::Object(cfg)
}

/// Maps n8n `httpRequest` parameters onto tinyflows' `{ method, url, ... }`
/// http_request config. n8n uses `url` + `method`/`requestMethod`; anything
/// else is carried through after expression translation.
pub(super) fn map_http_request(
    params: &Value,
    warnings: &mut Vec<String>,
    n8n_name: &str,
) -> Value {
    let translated = translate_config(params, warnings, n8n_name);
    let mut cfg = match translated {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    // Normalize the method key (`requestMethod` is the older n8n spelling).
    if !cfg.contains_key("method") {
        if let Some(method) = cfg.remove("requestMethod") {
            cfg.insert("method".to_string(), method);
        }
    }
    if !cfg.contains_key("body") {
        if let Some(body) = cfg.remove("jsonBody") {
            cfg.insert("body".to_string(), body);
        } else if let Some(body) = cfg.remove("bodyParameters") {
            match named_parameters(&body) {
                Some(body) => {
                    cfg.insert("body".to_string(), body);
                }
                None => warnings.push(format!(
                    "Node '{n8n_name}' has HTTP body parameters in an n8n shape this importer \
                     cannot translate; rebuild `config.body` before enabling the flow."
                )),
            }
        }
    }
    if !cfg.contains_key("headers")
        && let Some(headers) = cfg.remove("headerParameters")
    {
        match named_parameters(&headers) {
            Some(headers) => {
                cfg.insert("headers".to_string(), headers);
            }
            None => warnings.push(format!(
                "Node '{n8n_name}' has HTTP headers in an n8n shape this importer cannot \
                 translate; rebuild `config.headers` before enabling the flow."
            )),
        }
    }
    cfg.entry("method".to_string())
        .or_insert_with(|| Value::String("GET".to_string()));
    Value::Object(cfg)
}

/// Converts n8n's `{parameters:[{name,value}]}` collection to the object shape
/// tinyflows uses for HTTP bodies and headers.
fn named_parameters(value: &Value) -> Option<Value> {
    let entries = value.get("parameters").and_then(Value::as_array)?;
    let mut mapped = Map::new();
    for entry in entries {
        let name = entry.get("name").and_then(Value::as_str)?;
        let value = entry.get("value").cloned().unwrap_or(Value::Null);
        mapped.insert(name.to_string(), value);
    }
    Some(Value::Object(mapped))
}

/// Maps n8n `splitOut`/`itemLists` parameters onto tinyflows' `split_out`
/// config, which reads a single dotted `path` (`crates/tinyflows/src/nodes/control_flow/split_out.rs`).
/// n8n names the selected field `fieldToSplitOut` (or, on some node versions,
/// a bare `field`); both are renamed to `path` when present so the node
/// actually splits the authored field instead of the whole input value.
pub(super) fn map_split_out(params: &Value, warnings: &mut Vec<String>, n8n_name: &str) -> Value {
    let translated = translate_config(params, warnings, n8n_name);
    let mut cfg = match translated {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    if !cfg.contains_key("path") {
        let selected = cfg
            .remove("fieldToSplitOut")
            .or_else(|| cfg.remove("field"));
        if let Some(path) = selected {
            cfg.insert("path".to_string(), path);
        }
    }
    Value::Object(cfg)
}

/// Maps n8n `code`/`function` parameters onto tinyflows' code config, pulling
/// the source out of n8n's `jsCode`/`functionCode`/`pythonCode` fields into the
/// `source` key tinyflows' `code` node actually reads (`vendor/tinyflows/src/nodes/integration/code.rs`)
/// while preserving the language hint.
///
/// The source is carried through **verbatim** — it is not adapted to
/// tinyflows' stdin/stdout code contract, so ordinary n8n code (`$json`,
/// `$input`, `items`, a top-level `return`) will not run as-is. A warning
/// flags the common tell-tales so the author knows to rewrite it rather than
/// discovering it at run time.
pub(super) fn map_code(params: &Value, warnings: &mut Vec<String>, n8n_name: &str) -> Value {
    let translated = translate_config(params, warnings, n8n_name);
    let mut cfg = match translated {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    for (src, lang) in [
        ("jsCode", "javascript"),
        ("functionCode", "javascript"),
        ("pythonCode", "python"),
    ] {
        if let Some(code) = cfg.remove(src) {
            cfg.entry("source".to_string()).or_insert(code);
            cfg.entry("language".to_string())
                .or_insert_with(|| Value::String(lang.to_string()));
        }
    }
    if let Some(source) = cfg.get("source").and_then(Value::as_str)
        && uses_n8n_code_globals(source)
    {
        warnings.push(format!(
            "Node '{n8n_name}' is an n8n code node imported as an editable placeholder — it uses \
             n8n-only globals (`$json`/`$input`/`items`) and/or a top-level `return`, neither of \
             which tinyflows' code node supports (it reads input from stdin and has no n8n \
             runtime globals). Rewrite the script before enabling the flow."
        ));
    }
    Value::Object(cfg)
}

pub(super) fn map_code_node(
    params: &Value,
    warnings: &mut Vec<String>,
    n8n_name: &str,
) -> (NodeKind, Value) {
    let mut config = map_code(params, warnings, n8n_name);
    let incompatible = config
        .get("source")
        .and_then(Value::as_str)
        .is_some_and(uses_n8n_code_globals);
    if !incompatible {
        return (NodeKind::Code, config);
    }
    if let Value::Object(cfg) = &mut config {
        cfg.insert(
            "_n8n_import".to_string(),
            json!({
                "original_type": "code",
                "note": "n8n runtime globals are unavailable; rewrite for tinyflows stdin/stdout before changing this placeholder to code.",
            }),
        );
    }
    (NodeKind::Transform, config)
}

/// Whether `source` looks like it relies on n8n's code-node runtime globals
/// or return convention rather than tinyflows' stdin/stdout contract — a
/// cheap textual tell, not a parser, so it only needs to catch the common
/// cases without false-negatives on the (much rarer) code that happens not
/// to need them.
fn uses_n8n_code_globals(source: &str) -> bool {
    ["$json", "$input", "$node", "items"]
        .iter()
        .any(|needle| source.contains(needle))
        || source
            .split_whitespace()
            .next()
            .is_some_and(|first_word| first_word == "return")
}
