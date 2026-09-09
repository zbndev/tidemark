//! The environment a plugin's `parse` runs in, and the limits it runs under.
//!
//! Built rather than restricted: the VM starts with only the three libraries the format
//! documents, and every base-library name the format does not document is removed by name.
//! That way a new `mlua` release cannot quietly reintroduce a library, and the list of what a
//! plugin can reach is a list in this file rather than a claim about a default.
//!
//! Two removals need saying out loud. **`pairs` and `next` are gone**, so a plugin cannot
//! iterate a JSON object in hash order — the result would differ between platforms, and the
//! output order is part of what a plugin publishes. `sorted_keys` is the documented
//! replacement. **`pcall` is gone**, so a plugin cannot swallow the error that a limit was
//! hit; a limit is the daemon's verdict, not a condition to recover from.
//!
//! Every execution gets its own `Lua`. Nothing is shared between accounts or polls.

pub mod json;

use super::{PluginError, limits};
use mlua::{HookTriggers, Lua, LuaOptions, StdLib, Value};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// One finished execution, holding the VM its value belongs to.
///
/// The `Lua` travels with the value because an `mlua::Value` borrows its state: dropping the
/// VM to return only the value would be a use-after-free the type system already refuses.
#[derive(Debug)]
pub struct Executed {
    /// The VM the value lives in.
    pub lua: Lua,
    /// What `parse` returned.
    pub value: Value,
}

/// Compiles a chunk without running it: the check import performs before storing a file.
pub fn compile(source: &str) -> Result<(), PluginError> {
    let lua =
        sandbox(&Arc::new(AtomicBool::new(false))).map_err(|error| PluginError::LuaCompile {
            reason: bounded(&error.to_string()),
        })?;
    lua.load(source)
        .set_name("plugin")
        .into_function()
        .map(|_| ())
        .map_err(|error| PluginError::LuaCompile {
            reason: bounded(&error.to_string()),
        })
}

/// Runs `parse(response, context)` once, under every limit.
pub fn run(
    source: &str,
    response: &serde_json::Value,
    captured_at: i64,
) -> Result<Executed, PluginError> {
    let exhausted = Arc::new(AtomicBool::new(false));
    let lua = sandbox(&exhausted).map_err(|error| runtime(&error, &exhausted))?;

    let value = (|| -> mlua::Result<Value> {
        json::install_null(&lua)?;
        lua.load(source).set_name("plugin").exec()?;

        let context = lua.create_table()?;
        context.set("captured_at", captured_at)?;
        let response = json::to_lua(&lua, response)?;

        let parse: mlua::Function = lua.globals().get("parse")?;
        parse.call((response, context))
    })();

    match value {
        Ok(value) => Ok(Executed { lua, value }),
        Err(error) => Err(runtime(&error, &exhausted)),
    }
}

/// A VM with nothing in it but the documented subset.
fn sandbox(exhausted: &Arc<AtomicBool>) -> mlua::Result<Lua> {
    let lua = Lua::new_with(
        StdLib::MATH | StdLib::STRING | StdLib::TABLE,
        LuaOptions::default(),
    )?;
    lua.set_memory_limit(limits::LUA_HEAP_BYTES)?;

    let globals = lua.globals();
    // Everything the base library brings that the format does not document. Removed by name
    // rather than trusted to be absent: the base library is always loaded, and a future
    // release could add a name to it.
    for name in [
        "collectgarbage",
        "dofile",
        "load",
        "loadfile",
        "loadstring",
        "require",
        "next",
        "pairs",
        "pcall",
        "xpcall",
        "rawequal",
        "rawget",
        "rawset",
        "rawlen",
        "getmetatable",
        "setmetatable",
        "print",
        "unpack",
        "coroutine",
        "io",
        "os",
        "package",
        "debug",
        "utf8",
        "arg",
        "_G",
    ] {
        globals.set(name, Value::Nil)?;
    }
    // The two libraries that are kept still carry functions that load a chunk or reach for
    // entropy. A plugin is a pure function of its input; neither belongs in one.
    if let Ok(string) = globals.get::<mlua::Table>("string") {
        string.set("dump", Value::Nil)?;
    }
    if let Ok(math) = globals.get::<mlua::Table>("math") {
        math.set("random", Value::Nil)?;
        math.set("randomseed", Value::Nil)?;
    }

    let flag = Arc::clone(exhausted);
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(limits::LUA_INSTRUCTIONS),
        move |_lua, _debug| {
            flag.store(true, Ordering::Relaxed);
            Err(mlua::Error::runtime("instruction limit reached"))
        },
    )?;
    Ok(lua)
}

/// Classifies a failure: a limit that was hit, or the plugin's own error.
fn runtime(error: &mlua::Error, exhausted: &Arc<AtomicBool>) -> PluginError {
    if exhausted.load(Ordering::Relaxed) {
        return PluginError::LuaExhausted {
            what: "instruction",
        };
    }
    let text = error.to_string();
    if text.contains("not enough memory") || matches!(error, mlua::Error::MemoryError(_)) {
        return PluginError::LuaExhausted { what: "memory" };
    }
    if text.contains("stack overflow") {
        return PluginError::LuaExhausted { what: "stack" };
    }
    PluginError::LuaRuntime {
        reason: bounded(&text),
    }
}

/// A diagnostic cut to the documented bound, on a character boundary.
///
/// Bounded rather than whole: a plugin's `error()` message is data an endpoint may have
/// influenced, and an unbounded one would carry a response body into a log.
fn bounded(reason: &str) -> String {
    let mut end = reason.len().min(limits::MAX_ERROR_BYTES);
    while end > 0 && !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// What `parse` returned, as a string. `mlua` has no `Value::as_str`, so the two steps
    /// are spelled out once here rather than at every assertion.
    fn text(value: &Value) -> String {
        value
            .as_string()
            .expect("parse returned a string")
            .to_str()
            .expect("valid UTF-8")
            .to_string()
    }

    /// Runs a chunk whose `parse` returns whatever the fragment evaluates to, as a string,
    /// so a test can assert on the sandbox without going through output validation.
    fn evaluate(body: &str) -> Result<String, PluginError> {
        let source = format!("function parse(response, context)\n  return tostring({body})\nend");
        let executed = run(&source, &json!({}), 1_788_870_896)?;
        Ok(text(&executed.value))
    }

    #[test]
    fn a_parse_function_receives_the_response_and_a_deterministic_context() {
        let source = "function parse(response, context)\n  \
                      return tostring(response.a.b) .. \"@\" .. tostring(context.captured_at)\nend";
        let executed = run(source, &json!({"a": {"b": 7}}), 1_788_870_896).expect("runs");
        assert_eq!(text(&executed.value), "7@1788870896");
    }

    #[test]
    fn the_dangerous_standard_libraries_are_not_merely_hidden_but_absent() {
        for global in [
            "io",
            "os",
            "package",
            "debug",
            "require",
            "dofile",
            "load",
            "loadfile",
            "collectgarbage",
            "pairs",
            "next",
            "pcall",
            "xpcall",
            "getmetatable",
            "setmetatable",
            "rawset",
            "rawget",
            "coroutine",
            "arg",
            "print",
        ] {
            assert_eq!(
                evaluate(global).expect("the chunk itself is fine").as_str(),
                "nil",
                "{global} must not exist in the sandbox"
            );
        }
        assert_eq!(evaluate("string.dump").unwrap(), "nil");
        assert_eq!(evaluate("math.random").unwrap(), "nil");
        assert_eq!(evaluate("math.randomseed").unwrap(), "nil");
    }

    #[test]
    fn the_documented_standard_library_subset_is_present() {
        for global in [
            "assert", "error", "ipairs", "select", "tostring", "type", "math", "string", "table",
        ] {
            assert_ne!(
                evaluate(global).unwrap().as_str(),
                "nil",
                "{global} is documented as available"
            );
        }
        assert_eq!(evaluate("math.floor(3.7)").unwrap(), "3");
        assert_eq!(evaluate("string.upper(\"ab\")").unwrap(), "AB");
    }

    #[test]
    fn there_is_no_way_to_read_the_clock() {
        assert_eq!(evaluate("now").unwrap(), "nil");
        assert_eq!(
            evaluate("os").unwrap(),
            "nil",
            "os.time is the clock, and os is gone"
        );
    }

    #[test]
    fn an_infinite_loop_is_stopped_at_the_instruction_limit() {
        let source = "function parse(response, context)\n  while true do end\nend";
        assert!(matches!(
            run(source, &json!({}), 0),
            Err(PluginError::LuaExhausted {
                what: "instruction"
            })
        ));
    }

    #[test]
    fn unbounded_recursion_is_stopped_rather_than_overflowing_the_host_stack() {
        let source = "local function f(n) return f(n + 1) end\n\
                      function parse(response, context) return f(1) end";
        assert!(matches!(
            run(source, &json!({}), 0),
            Err(PluginError::LuaExhausted { .. }) | Err(PluginError::LuaRuntime { .. })
        ));
    }

    #[test]
    fn unbounded_allocation_is_stopped_at_the_memory_limit() {
        let source = "function parse(response, context)\n  \
                      local t = {}\n  local i = 1\n  \
                      while true do t[i] = string.rep(\"x\", 4096) i = i + 1 end\nend";
        assert!(matches!(
            run(source, &json!({}), 0),
            Err(PluginError::LuaExhausted { .. })
        ));
    }

    #[test]
    fn a_chunk_that_does_not_compile_says_where() {
        let error = compile("function parse( then end").expect_err("this is not Lua");
        let PluginError::LuaCompile { reason } = error else {
            panic!("wrong stage: {error:?}")
        };
        assert!(
            reason.contains('1'),
            "a compile diagnostic carries a line: {reason}"
        );
        assert!(reason.len() <= limits::MAX_ERROR_BYTES);
    }

    #[test]
    fn a_chunk_with_no_parse_function_fails_at_run_time_with_its_own_message() {
        let error = run("local x = 1", &json!({}), 0).expect_err("there is nothing to call");
        assert!(matches!(error, PluginError::LuaRuntime { .. }));
    }

    #[test]
    fn top_level_evaluation_runs_under_the_same_sandbox() {
        let source =
            "local escaped = os\nfunction parse(response, context) return tostring(escaped) end";
        assert_eq!(
            text(&run(source, &json!({}), 0).expect("runs").value),
            "nil",
            "the chunk's top level cannot see what parse cannot"
        );
    }

    #[test]
    fn an_error_raised_by_the_plugin_reaches_the_caller_bounded() {
        let long = "e".repeat(limits::MAX_ERROR_BYTES * 4);
        let source = format!("function parse(response, context) error(\"{long}\") end");
        let PluginError::LuaRuntime { reason } = run(&source, &json!({}), 0).expect_err("raises")
        else {
            panic!("a plugin error is a runtime failure")
        };
        assert!(reason.len() <= limits::MAX_ERROR_BYTES);
    }

    #[test]
    fn two_executions_share_no_state() {
        let source = "counter = (counter or 0) + 1\n\
                      function parse(response, context) return tostring(counter) end";
        assert_eq!(text(&run(source, &json!({}), 0).unwrap().value), "1");
        assert_eq!(
            text(&run(source, &json!({}), 0).unwrap().value),
            "1",
            "a fresh environment per execution, so one account cannot see another's"
        );
    }
}
