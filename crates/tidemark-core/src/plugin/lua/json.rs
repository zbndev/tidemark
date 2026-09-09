//! JSON as the sandbox sees it.
//!
//! The mapping is the format's contract and is documented for plugin authors: objects are
//! tables with string keys, arrays are one-based sequences, and scalars keep their meaning.
//! `null` is the one case with a decision in it — Lua's `nil` cannot be told apart from a
//! missing key, and "the provider said the limit is null" and "the provider did not mention
//! a limit" are different facts a plugin must be able to branch on. So null becomes a
//! read-only sentinel table bound to the global `null`.

use mlua::{Lua, Table, Value};
use serde_json::Value as Json;

/// The global a JSON null arrives as.
pub const NULL_SENTINEL: &str = "null";

/// Creates the sentinel and binds it. Called once per execution.
pub fn install_null(lua: &Lua) -> mlua::Result<()> {
    let sentinel = lua.create_table()?;
    // Read-only through a metatable, because Lua 5.4 has no readonly bit: a plugin that
    // could write into the sentinel could make one poll's null look like another's, and the
    // sentinel is shared by every null in the document. `__metatable` hides the guard, so
    // the table cannot be un-guarded by replacing it either.
    let guard = lua.create_table()?;
    guard.set(
        "__newindex",
        lua.create_function(|_, _: mlua::MultiValue| -> mlua::Result<()> {
            Err(mlua::Error::runtime("the null sentinel is read-only"))
        })?,
    )?;
    guard.set("__metatable", NULL_SENTINEL)?;
    sentinel.set_metatable(Some(guard))?;
    lua.globals().set(NULL_SENTINEL, sentinel)?;
    Ok(())
}

/// The sentinel, for comparisons.
fn sentinel(lua: &Lua) -> mlua::Result<Value> {
    lua.globals().get(NULL_SENTINEL)
}

/// Whether a value is the JSON null sentinel.
pub fn is_null(lua: &Lua, value: &Value) -> bool {
    match (sentinel(lua), value) {
        (Ok(Value::Table(expected)), Value::Table(found)) => expected == *found,
        _ => false,
    }
}

/// One JSON document as Lua values.
///
/// Recursive, and safely so: the document came from `serde_json`, whose parser refuses
/// nesting past its own recursion limit long before this could reach the host stack.
pub fn to_lua(lua: &Lua, value: &Json) -> mlua::Result<Value> {
    Ok(match value {
        Json::Null => sentinel(lua).unwrap_or(Value::Nil),
        Json::Bool(flag) => Value::Boolean(*flag),
        Json::Number(number) => match number.as_i64() {
            Some(integer) => Value::Integer(integer),
            None => Value::Number(number.as_f64().unwrap_or(f64::NAN)),
        },
        Json::String(text) => Value::String(lua.create_string(text)?),
        Json::Array(items) => {
            let table: Table = lua.create_table_with_capacity(items.len(), 0)?;
            for (index, item) in items.iter().enumerate() {
                table.set(index + 1, to_lua(lua, item)?)?;
            }
            Value::Table(table)
        }
        Json::Object(fields) => {
            let table: Table = lua.create_table_with_capacity(0, fields.len())?;
            for (key, field) in fields {
                table.set(key.as_str(), to_lua(lua, field)?)?;
            }
            Value::Table(table)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn convert(value: serde_json::Value, expression: &str) -> String {
        let lua = mlua::Lua::new();
        let converted = to_lua(&lua, &value).expect("converts");
        lua.globals().set("response", converted).expect("bound");
        lua.load(format!("return tostring({expression})"))
            .eval()
            .expect("evaluates")
    }

    #[test]
    fn objects_become_tables_and_arrays_become_one_based_sequences() {
        assert_eq!(
            convert(json!({"a": {"b": [10, 20]}}), "response.a.b[1]"),
            "10"
        );
        assert_eq!(convert(json!({"a": {"b": [10, 20]}}), "#response.a.b"), "2");
    }

    #[test]
    fn scalars_keep_their_json_meaning() {
        assert_eq!(convert(json!({"n": 1.5}), "response.n"), "1.5");
        assert_eq!(convert(json!({"n": 3}), "response.n"), "3");
        assert_eq!(convert(json!({"s": "12"}), "response.s"), "12");
        assert_eq!(convert(json!({"b": true}), "response.b"), "true");
    }

    #[test]
    fn json_null_is_a_sentinel_and_not_a_missing_key() {
        let lua = mlua::Lua::new();
        install_null(&lua).expect("installs");
        let value = to_lua(&lua, &json!({"present": null})).expect("converts");
        lua.globals().set("response", value).expect("bound");
        let answer: String = lua
            .load("return tostring(response.present ~= nil) .. tostring(response.absent == nil)")
            .eval()
            .expect("evaluates");
        assert_eq!(answer, "truetrue", "null is present; a missing key is not");
    }

    #[test]
    fn the_sentinel_is_the_same_value_everywhere_and_cannot_be_written_to() {
        let lua = mlua::Lua::new();
        install_null(&lua).expect("installs");
        let value = to_lua(&lua, &json!({"a": null, "b": null})).expect("converts");
        lua.globals().set("response", value).expect("bound");
        let same: bool = lua
            .load("return response.a == response.b and response.a == null")
            .eval()
            .expect("evaluates");
        assert!(
            same,
            "every null is one sentinel a plugin can compare against"
        );
        assert!(
            lua.load("null.x = 1").exec().is_err(),
            "writing into the sentinel would change what another null means"
        );
    }

    #[test]
    fn a_non_finite_number_cannot_enter_the_sandbox() {
        // serde_json cannot hold one, so this guards the boundary the other way: a value
        // the parser produced must be rejected downstream, never silently become nil here.
        assert!(serde_json::from_str::<serde_json::Value>("NaN").is_err());
    }
}
