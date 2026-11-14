//! Scripting with rhai (#67).

use alloc::string::{String, ToString};

/// Evaluate `expr` and show its value.
pub fn eval(expr: &str) -> Result<String, String> {
    let engine = rhai::Engine::new();
    engine.eval::<rhai::Dynamic>(expr).map(|v| v.to_string()).map_err(|e| e.to_string())
}
