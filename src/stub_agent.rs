//! A topology agent that answers invocations without a model.
//!
//! Every other backend is a real assistant, so nothing could exercise a run end
//! to end without spending model calls — which is exactly the path most worth
//! testing. This stands in for one: it reads `OMAR INVOCATION` messages from
//! its pane, writes a type-correct value to each allowed effect, and completes.
//!
//! Only the thinking is faked. The compiler, VM, scheduler, superdense-time
//! barrier, invocation server and diagram stream all run for real.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::manager::{McpLaunchContext, TopologyMcpContext};

/// Marker the runtime puts on every delivered invocation.
const INVOCATION: &str = "OMAR INVOCATION";

pub fn run(context_file: &Path) -> Result<()> {
    let raw = std::fs::read_to_string(context_file)
        .with_context(|| format!("failed to read {}", context_file.display()))?;
    let context: McpLaunchContext = serde_json::from_str(&raw).context("invalid MCP context")?;
    let topology = context
        .topology
        .context("stub agent requires a topology context")?;

    // Backends announce themselves; do the same so pane-readiness checks and a
    // human watching both see something sensible.
    println!(
        "OMAR stub agent ready: {}/{}",
        topology.team, topology.agent
    );
    std::io::stdout().flush().ok();

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    while let Some(line) = lines.next() {
        let line = line?;
        if !line.contains(INVOCATION) {
            continue;
        }
        // Fields arrive on their own lines directly after the marker.
        let mut fields: BTreeMap<String, String> = BTreeMap::new();
        for next in lines.by_ref() {
            let next = next?;
            if next.trim().is_empty() {
                break;
            }
            if let Some((key, value)) = next.split_once(':') {
                fields.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
        if let Err(error) = answer(&topology, &fields) {
            eprintln!("stub agent could not answer: {error:#}");
        }
    }
    Ok(())
}

fn answer(topology: &TopologyMcpContext, fields: &BTreeMap<String, String>) -> Result<()> {
    let invocation_id = fields
        .get("invocation_id")
        .context("invocation had no id")?
        .clone();
    // `effects` is a JSON object of port name to declared type, which is all a
    // stub needs to produce something the runtime will accept.
    let effects: BTreeMap<String, String> = match fields.get("effects") {
        Some(raw) => serde_json::from_str(raw).context("invalid effects")?,
        None => BTreeMap::new(),
    };

    for (port, ty) in &effects {
        send(
            topology,
            json!({
                "op": "set_port",
                "invocation_id": invocation_id,
                "port": port,
                "value": stub_value(ty),
            }),
        )?;
    }
    send(
        topology,
        json!({"op": "complete", "invocation_id": invocation_id}),
    )?;
    println!("stub agent answered {invocation_id}");
    std::io::stdout().flush().ok();
    Ok(())
}

/// The simplest value the runtime's validator accepts for a declared type.
fn stub_value(ty: &str) -> Value {
    // A refined string admits only what it lists, so "stub" is not among the
    // answers the validator would take. The first one always is.
    if let Some(allowed) = crate::topology::string_enum(ty) {
        return allowed.first().map(|o| json!(o)).unwrap_or(Value::Null);
    }
    if let Some(inner) = ty.strip_prefix("list<").and_then(|t| t.strip_suffix('>')) {
        return json!([stub_value(inner)]);
    }
    if ty.starts_with("option<") {
        return Value::Null;
    }
    match ty {
        "signal" => Value::Null,
        "bool" => json!(true),
        "int" => json!(0),
        "float" => json!(0.0),
        _ => json!("stub"),
    }
}

fn send(topology: &TopologyMcpContext, command: Value) -> Result<()> {
    let mut stream = TcpStream::connect(&topology.endpoint)
        .with_context(|| format!("topology runtime '{}' is unavailable", topology.endpoint))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let request = json!({
        "token": topology.token,
        "team": topology.team,
        "agent": topology.agent,
        "command": command,
    });
    serde_json::to_writer(&mut stream, &request)?;
    writeln!(stream)?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    let parsed: Value = serde_json::from_str(&response).context("invalid runtime response")?;
    if let Some(error) = parsed.get("error").and_then(Value::as_str) {
        bail!("runtime rejected the write: {error}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_values_satisfy_every_declared_type() {
        // The runtime validates writes, so a stub that guesses wrong fails the
        // run rather than the test. Keep these in step with `validate_value`.
        for (ty, check) in [
            ("string", Value::is_string as fn(&Value) -> bool),
            ("path", Value::is_string),
            ("bytes", Value::is_string),
            ("bool", Value::is_boolean),
            ("signal", Value::is_null),
            ("option<int>", Value::is_null),
        ] {
            assert!(check(&stub_value(ty)), "{ty} produced {}", stub_value(ty));
        }
        assert_eq!(stub_value("int").as_i64(), Some(0));
        assert_eq!(stub_value("float").as_f64(), Some(0.0));
        let list = stub_value("list<int>");
        assert_eq!(list.as_array().map(|items| items.len()), Some(1));
        assert_eq!(list[0].as_i64(), Some(0));
    }

    #[test]
    fn a_refined_string_stubs_to_a_value_it_admits() {
        // "stub" is not one of these, so without the refinement branch every
        // program declaring one would fail its first write.
        assert_eq!(
            stub_value("string in [\"continue\",\"stop\"]"),
            json!("continue")
        );
        let list = stub_value("list<string in [\"a\",\"b\"]>");
        assert_eq!(list, json!(["a"]));
    }
}
