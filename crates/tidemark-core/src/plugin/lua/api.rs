//! The small pure API a plugin gets, and nothing else.
//!
//! Every function here is a pure function of its arguments. None of them reaches state, the
//! clock, the filesystem or the network, and none of them can be given a colour, a pixel or
//! a format string: the four widget helpers return *marker tables* naming a semantic widget,
//! and the spelling of the numbers stays Tidemark's, so a plugin card and a built-in card
//! read the same way. See `docs/plugin-providers.md`.

use super::json;
use mlua::{Lua, Table, Value, Variadic};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The key a widget marker names its kind under. Not a Lua-visible feature: it is how
/// `output.rs` recognises what a plugin returned.
pub const MARKER: &str = "__widget";

/// Option keys each widget accepts. Anything else is a mistake worth naming, because a
/// plugin author who typed `colour` expects it to have done something.
const GAUGE_OPTIONS: &[&str] = &["field", "left", "right", "format", "emphasis"];
const VALUE_OPTIONS: &[&str] = &["field", "format", "emphasis"];
const RATIO_OPTIONS: &[&str] = &["left", "right", "format", "emphasis"];
const STATUS_OPTIONS: &[&str] = &["field", "emphasis"];

/// Binds every host global. Called once per execution, before the chunk runs.
pub fn install(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();

    globals.set(
        "number",
        lua.create_function(|_, value: Value| {
            let number = coerce(&value)?;
            // A whole number goes back as a Lua integer, not a float: a plugin that reads a
            // count of requests and returns it should see `42`, not `42.0`, because that
            // spelling reaches a card. `percent` keeps its fraction, and says so by
            // returning a float always.
            Ok(match whole(number) {
                Some(integer) => Value::Integer(integer),
                None => Value::Number(number),
            })
        })?,
    )?;

    globals.set(
        "percent",
        lua.create_function(|_, (value, maximum): (Value, Value)| {
            let (value, maximum) = (coerce(&value)?, coerce(&maximum)?);
            if maximum <= 0.0 {
                return Err(mlua::Error::runtime(
                    "percent needs a maximum greater than zero; a provider that reported none has no percentage",
                ));
            }
            Ok(value / maximum * 100.0)
        })?,
    )?;

    globals.set(
        "parse_time",
        lua.create_function(|_, value: Value| match &value {
            Value::Integer(seconds) => Ok(*seconds),
            Value::Number(seconds) if seconds.is_finite() => Ok(seconds.round() as i64),
            Value::String(text) => {
                let text = text.to_str()?;
                let trimmed = text.trim();
                if let Ok(seconds) = trimmed.parse::<i64>() {
                    return Ok(seconds);
                }
                OffsetDateTime::parse(trimmed, &Rfc3339)
                    .map(OffsetDateTime::unix_timestamp)
                    .map_err(|_| mlua::Error::runtime("parse_time takes RFC 3339 or Unix seconds"))
            }
            other => Err(mlua::Error::runtime(format!(
                "parse_time cannot read a {}",
                other.type_name()
            ))),
        })?,
    )?;

    globals.set(
        "is_null",
        lua.create_function(|lua, value: Value| Ok(json::is_null(lua, &value)))?,
    )?;

    globals.set(
        "sorted_keys",
        lua.create_function(|lua, table: Value| {
            let Value::Table(table) = table else {
                return Err(mlua::Error::runtime("sorted_keys takes a JSON object"));
            };
            let mut keys: Vec<String> = Vec::new();
            // `pairs` is not available to a plugin, but the host may iterate: what is
            // withheld is *unordered* iteration, and this is where the order is imposed.
            for entry in table.pairs::<Value, Value>() {
                let (key, _) = entry?;
                if let Value::String(key) = key {
                    keys.push(key.to_str()?.to_string());
                }
            }
            keys.sort_unstable();
            let out = lua.create_table_with_capacity(keys.len(), 0)?;
            for (index, key) in keys.into_iter().enumerate() {
                out.set(index + 1, key)?;
            }
            Ok(out)
        })?,
    )?;

    widget(lua, "gauge", GAUGE_OPTIONS)?;
    widget(lua, "value", VALUE_OPTIONS)?;
    widget(lua, "ratio", RATIO_OPTIONS)?;
    widget(lua, "status", STATUS_OPTIONS)?;
    Ok(())
}

/// One widget helper: `kind(metric_id, options?)`.
fn widget(lua: &Lua, kind: &'static str, accepted: &'static [&'static str]) -> mlua::Result<()> {
    let function = lua.create_function(move |lua, mut args: Variadic<Value>| {
        let metric = match args.first() {
            Some(Value::String(id)) => id.to_str()?.to_string(),
            _ => {
                return Err(mlua::Error::runtime(format!(
                    "{kind} needs a metric id as its first argument"
                )));
            }
        };
        let marker: Table = lua.create_table()?;
        marker.set(MARKER, kind)?;
        marker.set("metric", metric)?;

        if args.len() > 1 {
            let options = args.remove(1);
            let Value::Table(options) = options else {
                return Err(mlua::Error::runtime(format!(
                    "{kind} options must be a table"
                )));
            };
            for entry in options.pairs::<Value, Value>() {
                let (key, value) = entry?;
                let Value::String(key) = key else {
                    return Err(mlua::Error::runtime(format!("{kind} options are named")));
                };
                let key = key.to_str()?.to_string();
                if !accepted.contains(&key.as_str()) {
                    return Err(mlua::Error::runtime(format!(
                        "{kind} has no {key} option; it may set {}",
                        accepted.join(", ")
                    )));
                }
                let Value::String(value) = value else {
                    return Err(mlua::Error::runtime(format!(
                        "{kind} option {key} must be a string"
                    )));
                };
                marker.set(key, value.to_str()?.to_string())?;
            }
        }
        Ok(marker)
    })?;
    lua.globals().set(kind, function)
}

/// A finite number, or a string that is one. Nothing else, and never a default.
fn coerce(value: &Value) -> mlua::Result<f64> {
    let number = match value {
        Value::Integer(integer) => *integer as f64,
        Value::Number(number) => *number,
        Value::String(text) => text
            .to_str()?
            .trim()
            .parse::<f64>()
            .map_err(|_| mlua::Error::runtime("number takes a number or a numeric string"))?,
        other => {
            return Err(mlua::Error::runtime(format!(
                "number cannot read a {}",
                other.type_name()
            )));
        }
    };
    if !number.is_finite() {
        return Err(mlua::Error::runtime("a quota number must be finite"));
    }
    Ok(number)
}

/// The same value as a Lua integer, when it is one exactly and fits.
fn whole(number: f64) -> Option<i64> {
    let rounded = number.trunc();
    #[allow(clippy::float_cmp)]
    let exact = rounded == number;
    (exact && number.abs() < 9.007_199_254_740_992e15).then_some(rounded as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expression: &str) -> Result<String, String> {
        let lua = mlua::Lua::new();
        super::super::json::install_null(&lua).expect("null");
        install(&lua).expect("api");
        lua.load(format!("return tostring({expression})"))
            .eval()
            .map_err(|error| error.to_string())
    }

    #[test]
    fn number_accepts_a_finite_number_or_a_numeric_string() {
        assert_eq!(eval("number(12.5)").unwrap(), "12.5");
        assert_eq!(eval("number(\"12.5\")").unwrap(), "12.5");
        assert_eq!(eval("number(\" 42 \")").unwrap(), "42");
    }

    #[test]
    fn number_refuses_anything_that_is_not_one() {
        for hostile in [
            "number(\"a lot\")",
            "number(nil)",
            "number(null)",
            "number({})",
            "number(true)",
        ] {
            assert!(eval(hostile).is_err(), "{hostile} must be a parse error");
        }
    }

    #[test]
    fn percent_divides_and_refuses_a_non_positive_maximum() {
        assert_eq!(eval("percent(1, 4)").unwrap(), "25.0");
        assert!(
            eval("percent(1, 0)").is_err(),
            "a zero maximum has no percentage"
        );
        assert!(eval("percent(1, -4)").is_err());
    }

    #[test]
    fn percent_above_one_hundred_is_truthful_rather_than_clamped() {
        assert_eq!(eval("percent(5, 4)").unwrap(), "125.0");
    }

    #[test]
    fn parse_time_reads_rfc_3339_and_unix_seconds() {
        assert_eq!(
            eval("parse_time(\"2026-09-08T12:00:00Z\")").unwrap(),
            "1788868800"
        );
        assert_eq!(eval("parse_time(1788868800)").unwrap(), "1788868800");
        assert_eq!(eval("parse_time(\"1788868800\")").unwrap(), "1788868800");
        assert!(eval("parse_time(\"yesterday\")").is_err());
    }

    #[test]
    fn is_null_is_true_only_for_the_sentinel() {
        assert_eq!(eval("is_null(null)").unwrap(), "true");
        assert_eq!(eval("is_null(nil)").unwrap(), "false");
        assert_eq!(eval("is_null(0)").unwrap(), "false");
        assert_eq!(eval("is_null({})").unwrap(), "false");
    }

    #[test]
    fn sorted_keys_replaces_the_unordered_iteration_that_was_removed() {
        let lua = mlua::Lua::new();
        super::super::json::install_null(&lua).expect("null");
        install(&lua).expect("api");
        let joined: String = lua
            .load(
                "local keys = sorted_keys({ b = 1, a = 2, c = 3 })\n\
                 local out = \"\"\n\
                 for _, key in ipairs(keys) do out = out .. key end\n\
                 return out",
            )
            .eval()
            .expect("evaluates");
        assert_eq!(joined, "abc", "lexical order, so two platforms agree");
    }

    #[test]
    fn sorted_keys_refuses_something_that_is_not_an_object() {
        assert!(eval("sorted_keys(4)").is_err());
    }

    #[test]
    fn a_widget_helper_returns_a_typed_marker_carrying_its_options() {
        let lua = mlua::Lua::new();
        super::super::json::install_null(&lua).expect("null");
        install(&lua).expect("api");
        let table: mlua::Table = lua
            .load("return gauge(\"cost\", { field = \"used_percent\", emphasis = \"compact\" })")
            .eval()
            .expect("evaluates");
        assert_eq!(table.get::<String>(MARKER).unwrap(), "gauge");
        assert_eq!(table.get::<String>("metric").unwrap(), "cost");
        assert_eq!(table.get::<String>("field").unwrap(), "used_percent");
        assert_eq!(table.get::<String>("emphasis").unwrap(), "compact");
    }

    #[test]
    fn every_widget_helper_names_its_own_kind_and_takes_no_options_it_does_not_have() {
        for (call, kind) in [
            ("gauge(\"m\", {})", "gauge"),
            ("value(\"m\", {})", "value"),
            (
                "ratio(\"m\", { left = \"value\", right = \"maximum\" })",
                "ratio",
            ),
            ("status(\"m\", {})", "status"),
        ] {
            assert_eq!(eval(&format!("({call})[\"{MARKER}\"]")).unwrap(), kind);
        }
        assert!(
            eval("gauge(nil, {})").is_err(),
            "a widget must name a metric"
        );
        assert!(
            eval("gauge(\"m\", { colour = \"red\" })").is_err(),
            "there is no colour to choose"
        );
        assert!(eval("gauge(\"m\", { field = 4 })").is_err());
    }

    #[test]
    fn a_widget_helper_takes_its_options_table_optionally() {
        assert_eq!(
            eval(&format!("value(\"m\")[\"{MARKER}\"]")).unwrap(),
            "value"
        );
    }
}
