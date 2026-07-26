use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::config;
use crate::diagram::{DiagramServer, NoopTopologyObserver, TopologyObserver};
use crate::manager::{self, McpLaunchContext, TopologyMcpContext};
use crate::tmux::DeliveryOptions;
use crate::tmux::TmuxClient;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bytecode {
    pub version: u32,
    pub team: String,
    pub instructions: Vec<Instruction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Instruction {
    BeginPlan {
        team: String,
    },
    SpawnAgent {
        name: String,
        backend: String,
    },
    DefinePort {
        name: String,
        kind: PortKind,
        #[serde(rename = "type")]
        ty: String,
        #[serde(default)]
        delay: Option<u64>,
    },
    ConnectPorts {
        source: String,
        target: String,
        delay: u64,
    },
    InstallReaction {
        id: String,
        agent: String,
        triggers: Vec<String>,
        effects: Vec<String>,
        contract: String,
        prompt: String,
    },
    CommitPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortKind {
    Input,
    Output,
    Action,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    pub backend: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortState {
    pub kind: PortKind,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionState {
    pub source: String,
    pub target: String,
    pub delay: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionState {
    pub order: usize,
    pub agent: String,
    pub triggers: Vec<String>,
    pub effects: Vec<String>,
    pub contract: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmState {
    pub version: u32,
    pub team: String,
    pub agents: BTreeMap<String, AgentState>,
    pub ports: BTreeMap<String, PortState>,
    pub connections: Vec<ConnectionState>,
    pub reactions: BTreeMap<String, ReactionState>,
}

pub fn load_bytecode(path: &std::path::Path) -> Result<Bytecode> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read bytecode {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid bytecode JSON in {}", path.display()))
}

/// Compile and load an OMAR source program.
///
/// Installed builds find `omarc` beside the `omar` executable or on `PATH`;
/// development builds also recognize the compiler built under `lang/.lake`.
pub fn load_program(path: &Path) -> Result<Bytecode> {
    load_program_with_compiler(path, None)
}

fn load_program_with_compiler(path: &Path, compiler: Option<&Path>) -> Result<Bytecode> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("omar") {
        bail!(
            "OMAR programs must use the .omar extension: {}",
            path.display()
        );
    }

    compile_source(path, compiler)
}

fn compile_source(source: &Path, compiler: Option<&Path>) -> Result<Bytecode> {
    let output_file = crate::paths::create_private_temp_file("omar-compile", "json")
        .context("failed to create temporary bytecode file")?;
    let output_path = output_file.path();
    let compiler = compiler
        .map(Path::to_path_buf)
        .unwrap_or_else(resolve_omarc);

    let output = Command::new(&compiler)
        .arg(source)
        .arg(output_path)
        .output()
        .with_context(|| {
            format!(
                "failed to invoke OMAR compiler '{}'; install omarc beside omar, \
                 add it to PATH, or set OMARC_BIN",
                compiler.display()
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let details = if stderr.is_empty() { stdout } else { stderr };
        if details.is_empty() {
            bail!("omarc failed with status {}", output.status);
        }
        bail!("omarc failed: {details}");
    }

    load_bytecode(output_path)
        .with_context(|| format!("omarc failed to compile {}", source.display()))
}

fn resolve_omarc() -> PathBuf {
    if let Some(path) = std::env::var_os("OMARC_BIN") {
        return PathBuf::from(path);
    }

    let executable_name = format!("omarc{}", std::env::consts::EXE_SUFFIX);
    if let Ok(current_executable) = std::env::current_exe() {
        if let Some(directory) = current_executable.parent() {
            let sibling = directory.join(&executable_name);
            if sibling.is_file() {
                return sibling;
            }
        }
    }

    let development_compiler = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("lang/.lake/build/bin")
        .join(&executable_name);
    if development_compiler.is_file() {
        return development_compiler;
    }

    PathBuf::from(executable_name)
}

pub fn verify(bytecode: &Bytecode) -> Result<VmState> {
    if bytecode.version != 1 {
        bail!("unsupported bytecode version {}", bytecode.version);
    }
    if !valid_identifier(&bytecode.team) {
        bail!("invalid team name '{}'", bytecode.team);
    }
    if bytecode.instructions.len() < 2 {
        bail!("bytecode plan is incomplete");
    }
    match bytecode.instructions.first() {
        Some(Instruction::BeginPlan { team }) if team == &bytecode.team => {}
        Some(Instruction::BeginPlan { team }) => {
            bail!("plan team '{team}' does not match '{}'", bytecode.team)
        }
        _ => bail!("bytecode must begin with begin_plan"),
    }
    if !matches!(bytecode.instructions.last(), Some(Instruction::CommitPlan)) {
        bail!("bytecode must end with commit_plan");
    }

    let mut state = VmState {
        version: bytecode.version,
        team: bytecode.team.clone(),
        agents: BTreeMap::new(),
        ports: BTreeMap::new(),
        connections: Vec::new(),
        reactions: BTreeMap::new(),
    };
    let mut committed = false;

    for (index, instruction) in bytecode.instructions.iter().enumerate() {
        if committed {
            bail!("instruction {index} appears after commit_plan");
        }
        match instruction {
            Instruction::BeginPlan { .. } if index == 0 => {}
            Instruction::BeginPlan { .. } => bail!("duplicate begin_plan at instruction {index}"),
            Instruction::SpawnAgent { name, backend } => {
                require_identifier("agent", name)?;
                if backend.trim().is_empty() {
                    bail!("agent '{name}' has an empty backend");
                }
                if state
                    .agents
                    .insert(
                        name.clone(),
                        AgentState {
                            backend: backend.clone(),
                        },
                    )
                    .is_some()
                {
                    bail!("duplicate agent '{name}'");
                }
            }
            Instruction::DefinePort {
                name,
                kind,
                ty,
                delay,
            } => {
                require_identifier("port", name)?;
                if ty.trim().is_empty() {
                    bail!("port '{name}' has an empty type");
                }
                if delay.is_some() && *kind != PortKind::Action {
                    bail!("only action ports may declare a fixed delay");
                }
                if state
                    .ports
                    .insert(
                        name.clone(),
                        PortState {
                            kind: *kind,
                            ty: ty.clone(),
                            delay: *delay,
                        },
                    )
                    .is_some()
                {
                    bail!("duplicate port '{name}'");
                }
            }
            Instruction::ConnectPorts {
                source,
                target,
                delay,
            } => {
                let source_port = state
                    .ports
                    .get(source)
                    .with_context(|| format!("connection references unknown source '{source}'"))?;
                let target_port = state
                    .ports
                    .get(target)
                    .with_context(|| format!("connection references unknown target '{target}'"))?;
                if target_port.kind == PortKind::Input {
                    bail!("connection cannot target input '{target}'");
                }
                if source_port.ty != target_port.ty {
                    bail!("connection type mismatch from '{source}' to '{target}'");
                }
                if state
                    .connections
                    .iter()
                    .any(|connection| connection.source == *source && connection.target == *target)
                {
                    bail!("duplicate connection from '{source}' to '{target}'");
                }
                state.connections.push(ConnectionState {
                    source: source.clone(),
                    target: target.clone(),
                    delay: *delay,
                });
            }
            Instruction::InstallReaction {
                id,
                agent,
                triggers,
                effects,
                contract,
                prompt,
            } => {
                let order = state.reactions.len();
                if !state.agents.contains_key(agent) {
                    bail!("reaction '{id}' references unknown agent '{agent}'");
                }
                for trigger in triggers {
                    let port = state.ports.get(trigger).with_context(|| {
                        format!("reaction '{id}' has unknown trigger '{trigger}'")
                    })?;
                    if port.kind == PortKind::Output {
                        bail!("reaction '{id}' cannot be triggered by output '{trigger}'");
                    }
                }
                for effect in effects {
                    let port = state.ports.get(effect).with_context(|| {
                        format!("reaction '{id}' has unknown effect '{effect}'")
                    })?;
                    if port.kind == PortKind::Input {
                        bail!("reaction '{id}' cannot affect input '{effect}'");
                    }
                }
                if state
                    .reactions
                    .insert(
                        id.clone(),
                        ReactionState {
                            order,
                            agent: agent.clone(),
                            triggers: triggers.clone(),
                            effects: effects.clone(),
                            contract: contract.clone(),
                            prompt: prompt.clone(),
                        },
                    )
                    .is_some()
                {
                    bail!("duplicate reaction '{id}'");
                }
            }
            Instruction::CommitPlan => committed = true,
        }
    }
    Ok(state)
}

#[derive(Debug, Clone)]
struct InvocationRecord {
    id: String,
    team: String,
    agent: String,
    reaction: String,
    contract: String,
    allowed_effects: BTreeMap<String, String>,
    writes: BTreeMap<String, Value>,
    completed: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct SetPortArgs {
    invocation_id: String,
    port: String,
    value: Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct CompleteArgs {
    invocation_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum InvocationCommand {
    SetPort(SetPortArgs),
    Complete(CompleteArgs),
}

#[derive(Debug, Serialize, Deserialize)]
struct InvocationRequest {
    token: String,
    team: String,
    agent: String,
    command: InvocationCommand,
}

#[derive(Debug, Serialize, Deserialize)]
struct InvocationResponse {
    result: Option<Value>,
    error: Option<String>,
}

struct InvocationEntry {
    record: InvocationRecord,
    completion: Option<mpsc::SyncSender<BTreeMap<String, Value>>>,
}

#[derive(Clone, Default)]
struct InvocationRegistry {
    entries: Arc<Mutex<BTreeMap<String, InvocationEntry>>>,
}

impl InvocationRegistry {
    fn register(
        &self,
        record: InvocationRecord,
    ) -> Result<mpsc::Receiver<BTreeMap<String, Value>>> {
        let id = record.id.clone();
        let (completion, receiver) = mpsc::sync_channel(1);
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!("invocation registry lock poisoned"))?;
        if entries.contains_key(&id) {
            bail!("duplicate invocation '{id}'");
        }
        entries.insert(
            id,
            InvocationEntry {
                record,
                completion: Some(completion),
            },
        );
        Ok(receiver)
    }

    fn remove(&self, invocation_id: &str) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(invocation_id);
        }
    }

    fn execute(&self, team: &str, agent: &str, command: InvocationCommand) -> Result<Value> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!("invocation registry lock poisoned"))?;
        match command {
            InvocationCommand::SetPort(args) => {
                let invocation = entries
                    .get_mut(&args.invocation_id)
                    .with_context(|| format!("unknown invocation '{}'", args.invocation_id))?;
                validate_invocation_owner(team, agent, &invocation.record)?;
                if invocation.record.completed {
                    bail!("invocation '{}' is already complete", invocation.record.id);
                }
                let ty = invocation
                    .record
                    .allowed_effects
                    .get(&args.port)
                    .with_context(|| {
                        format!("port '{}' is not an effect of this invocation", args.port)
                    })?;
                validate_value(ty, &args.value)
                    .with_context(|| format!("invalid value for port '{}'", args.port))?;
                invocation
                    .record
                    .writes
                    .insert(args.port.clone(), args.value);
                Ok(json!({"status":"buffered","port":args.port}))
            }
            InvocationCommand::Complete(args) => {
                let invocation = entries
                    .get_mut(&args.invocation_id)
                    .with_context(|| format!("unknown invocation '{}'", args.invocation_id))?;
                validate_invocation_owner(team, agent, &invocation.record)?;
                if invocation.record.completed {
                    return Ok(json!({"status":"already_complete"}));
                }
                validate_contract(&invocation.record.contract, &invocation.record.writes)
                    .with_context(|| {
                        format!(
                            "reaction '{}' invocation '{}' failed",
                            invocation.record.reaction, invocation.record.id
                        )
                    })?;
                invocation.record.completed = true;
                let writes = invocation.record.writes.clone();
                let completion = invocation
                    .completion
                    .take()
                    .context("invocation completion channel is unavailable")?;
                completion
                    .send(writes)
                    .map_err(|_| anyhow::anyhow!("invocation is no longer active"))?;
                Ok(json!({"status":"complete"}))
            }
        }
    }
}

struct InvocationServer {
    endpoint: String,
    token: String,
    registry: InvocationRegistry,
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl InvocationServer {
    fn start() -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .context("failed to bind topology invocation service")?;
        let endpoint = listener.local_addr()?.to_string();
        let token = Uuid::new_v4().to_string();
        let registry = InvocationRegistry::default();
        let thread_registry = registry.clone();
        let thread_token = token.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = shutdown.clone();
        let thread = thread::spawn(move || {
            for connection in listener.incoming() {
                if thread_shutdown.load(Ordering::Acquire) {
                    break;
                }
                match connection {
                    Ok(stream) => {
                        let _ =
                            serve_invocation_connection(stream, &thread_token, &thread_registry);
                    }
                    Err(_) if thread_shutdown.load(Ordering::Acquire) => break,
                    Err(_) => continue,
                }
            }
        });
        Ok(Self {
            endpoint,
            token,
            registry,
            shutdown,
            thread: Some(thread),
        })
    }
}

impl Drop for InvocationServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = TcpStream::connect(&self.endpoint);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_invocation_connection(
    mut stream: TcpStream,
    token: &str,
    registry: &InvocationRegistry,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut request_line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut request_line)?;
    let response = match serde_json::from_str::<InvocationRequest>(&request_line) {
        Ok(request) if request.token == token => {
            match registry.execute(&request.team, &request.agent, request.command) {
                Ok(result) => InvocationResponse {
                    result: Some(result),
                    error: None,
                },
                Err(error) => InvocationResponse {
                    result: None,
                    error: Some(format!("{error:#}")),
                },
            }
        }
        Ok(_) => InvocationResponse {
            result: None,
            error: Some("invalid topology invocation token".into()),
        },
        Err(error) => InvocationResponse {
            result: None,
            error: Some(format!("invalid topology invocation request: {error}")),
        },
    };
    serde_json::to_writer(&mut stream, &response)?;
    writeln!(stream)?;
    Ok(())
}

fn send_invocation_command(
    context: &TopologyMcpContext,
    command: InvocationCommand,
) -> Result<Value> {
    let mut stream = TcpStream::connect(&context.endpoint)
        .with_context(|| format!("topology runtime '{}' is unavailable", context.endpoint))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let request = InvocationRequest {
        token: context.token.clone(),
        team: context.team.clone(),
        agent: context.agent.clone(),
        command,
    };
    serde_json::to_writer(&mut stream, &request)?;
    writeln!(stream)?;
    let mut response_line = String::new();
    BufReader::new(stream).read_line(&mut response_line)?;
    let response: InvocationResponse =
        serde_json::from_str(&response_line).context("invalid response from topology runtime")?;
    match (response.result, response.error) {
        (Some(result), None) => Ok(result),
        (_, Some(error)) => Err(anyhow::anyhow!(error)),
        _ => bail!("topology runtime returned an empty response"),
    }
}

pub(crate) fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".{}.tmp", Uuid::new_v4()));
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

pub(crate) fn mcp_set_port(context: &TopologyMcpContext, arguments: Value) -> Result<Value> {
    send_invocation_command(
        context,
        InvocationCommand::SetPort(serde_json::from_value(arguments)?),
    )
}

pub(crate) fn mcp_complete(context: &TopologyMcpContext, arguments: Value) -> Result<Value> {
    send_invocation_command(
        context,
        InvocationCommand::Complete(serde_json::from_value(arguments)?),
    )
}

fn validate_invocation_owner(team: &str, agent: &str, invocation: &InvocationRecord) -> Result<()> {
    if invocation.team != team || invocation.agent != agent {
        bail!("invocation does not belong to this topology agent");
    }
    Ok(())
}

fn validate_value(ty: &str, value: &Value) -> Result<()> {
    if let Some(inner) = generic_inner(ty, "list") {
        let values = value
            .as_array()
            .with_context(|| format!("expected {ty}, got {value}"))?;
        for (index, item) in values.iter().enumerate() {
            validate_value(inner, item)
                .with_context(|| format!("invalid element {index} of {ty}"))?;
        }
        return Ok(());
    }
    if let Some(inner) = generic_inner(ty, "option") {
        return if value.is_null() {
            Ok(())
        } else {
            validate_value(inner, value).with_context(|| format!("invalid value for {ty}"))
        };
    }

    let valid = match ty {
        "signal" => value.is_null(),
        "bool" => value.is_boolean(),
        "int" => value.as_i64().is_some(),
        "float" => value.as_f64().is_some(),
        "string" | "path" | "bytes" => value.is_string(),
        _ => false,
    };
    if !valid {
        bail!("expected {ty}, got {value}");
    }
    Ok(())
}

fn generic_inner<'a>(ty: &'a str, outer: &str) -> Option<&'a str> {
    ty.strip_prefix(outer)?
        .strip_prefix('<')?
        .strip_suffix('>')
        .filter(|inner| !inner.is_empty())
}

#[derive(Debug)]
struct ContractGroup {
    optional: bool,
    alternatives: Vec<(String, Option<Value>)>,
}

fn parse_contract(contract: &str) -> Result<Vec<ContractGroup>> {
    let tokens: Vec<&str> = contract.split_whitespace().collect();
    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut depth = 0usize;
    for token in tokens {
        match token {
            "(" => {
                depth += 1;
                current.push(token);
            }
            ")" => {
                depth = depth.saturating_sub(1);
                current.push(token);
            }
            "," if depth == 0 => {
                groups.push(parse_contract_group(&current)?);
                current.clear();
            }
            _ => current.push(token),
        }
    }
    if !current.is_empty() {
        groups.push(parse_contract_group(&current)?);
    }
    Ok(groups)
}

fn parse_contract_group(tokens: &[&str]) -> Result<ContractGroup> {
    let mut tokens = tokens.to_vec();
    let optional = tokens.last() == Some(&"?");
    if optional {
        tokens.pop();
    }
    if tokens.first() == Some(&"(") && tokens.last() == Some(&")") {
        tokens.remove(0);
        tokens.pop();
    }
    let mut alternatives = Vec::new();
    for atom in tokens.split(|token| *token == "|") {
        let name = atom
            .first()
            .context("empty effect in contract")?
            .to_string();
        let constant = if atom.get(1) == Some(&"=") {
            Some(parse_literal(
                atom.get(2).context("missing constant value")?,
            )?)
        } else {
            None
        };
        alternatives.push((name, constant));
    }
    Ok(ContractGroup {
        optional,
        alternatives,
    })
}

fn parse_literal(value: &str) -> Result<Value> {
    match value {
        "true" => Ok(Value::Bool(true)),
        "false" => Ok(Value::Bool(false)),
        _ if value.parse::<i64>().is_ok() => Ok(json!(value.parse::<i64>()?)),
        _ => Ok(Value::String(value.trim_matches('"').to_string())),
    }
}

fn validate_contract(contract: &str, writes: &BTreeMap<String, Value>) -> Result<()> {
    let groups = parse_contract(contract)?;
    let contract_ports: BTreeSet<_> = groups
        .iter()
        .flat_map(|group| group.alternatives.iter())
        .map(|(port, _)| port.as_str())
        .collect();
    if let Some(port) = writes
        .keys()
        .find(|port| !contract_ports.contains(port.as_str()))
    {
        bail!("effect '{port}' is not permitted by contract '{contract}'");
    }

    for group in groups {
        let present: Vec<_> = group
            .alternatives
            .iter()
            .filter(|(port, _)| writes.contains_key(port))
            .collect();
        if (!group.optional && present.len() != 1) || (group.optional && present.len() > 1) {
            bail!("effect contract '{}' is not satisfied", contract);
        }
        for (port, constant) in present {
            if let Some(expected) = constant {
                if writes.get(port) != Some(expected) {
                    bail!("effect '{}' must equal {}", port, expected);
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct InvocationSpec {
    id: String,
    reaction_id: String,
    agent: String,
    trigger_values: BTreeMap<String, Value>,
    allowed_effects: BTreeMap<String, String>,
    contract: String,
    prompt: String,
}

trait ReactionExecutor: Sync {
    fn invoke(&self, invocation: InvocationSpec) -> Result<BTreeMap<String, Value>>;
}

struct TmuxReactionExecutor {
    client: TmuxClient,
    team: String,
    registry: InvocationRegistry,
    timeout: Duration,
}

impl ReactionExecutor for TmuxReactionExecutor {
    fn invoke(&self, invocation: InvocationSpec) -> Result<BTreeMap<String, Value>> {
        let invocation_id = invocation.id.clone();
        let record = InvocationRecord {
            id: invocation_id.clone(),
            team: self.team.clone(),
            agent: invocation.agent.clone(),
            reaction: invocation.reaction_id.clone(),
            contract: invocation.contract.clone(),
            allowed_effects: invocation.allowed_effects.clone(),
            writes: BTreeMap::new(),
            completed: false,
        };
        let completion = self.registry.register(record)?;

        let rendered = render_prompt(&invocation.prompt, &invocation.trigger_values)?;
        let message = format!(
            "OMAR INVOCATION\ninvocation_id: {}\ntriggers: {}\neffects: {}\ncontract: {}\n\n{}\n\nUse omar_set_port for each effect you choose, then call omar_complete exactly once. For a signal effect, set its value to null. Do not address another agent directly.",
            invocation.id,
            serde_json::to_string(&invocation.trigger_values)?,
            serde_json::to_string(&invocation.allowed_effects)?,
            invocation.contract,
            rendered
        );
        let session = format!("{}{}", self.client.prefix(), invocation.agent);
        if let Err(error) = self
            .client
            .deliver_prompt(&session, &message, &DeliveryOptions::default())
            .with_context(|| format!("failed to deliver {}", invocation.reaction_id))
        {
            self.registry.remove(&invocation_id);
            return Err(error);
        }

        let result = match completion.recv_timeout(self.timeout) {
            Ok(writes) => Ok(writes),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(anyhow::anyhow!(
                "invocation '{}' on agent '{}' timed out",
                invocation_id,
                invocation.agent
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(anyhow::anyhow!(
                "topology invocation service stopped unexpectedly"
            )),
        };
        self.registry.remove(&invocation_id);
        result
    }
}

pub struct TopologyRunConfig<'a> {
    pub ea_id: crate::ea::EaId,
    pub omar_dir: &'a Path,
    pub base_prefix: &'a str,
    pub default_workdir: &'a str,
    pub health_idle_warning: i64,
    pub inputs: &'a [String],
    pub replace: bool,
    pub timeout: Duration,
    pub diagram_address: Option<std::net::SocketAddr>,
}

pub fn run_topology(bytecode: &Bytecode, config: TopologyRunConfig<'_>) -> Result<()> {
    let state = verify(bytecode)?;
    let runtime_dir = crate::ea::ea_state_dir(config.ea_id, config.omar_dir)
        .join("topologies")
        .join(&state.team);
    fs::create_dir_all(&runtime_dir)?;
    let obsolete_invocations = runtime_dir.join("invocations");
    if obsolete_invocations.exists() {
        fs::remove_dir_all(&obsolete_invocations)?;
    }
    let diagram_server = config
        .diagram_address
        .map(|address| DiagramServer::start(&state, address))
        .transpose()?;
    if let Some(server) = &diagram_server {
        println!("Diagram server: http://{}", server.address());
    }
    let diagram_publisher = diagram_server.as_ref().map(DiagramServer::publisher);
    let noop_observer = NoopTopologyObserver;
    let observer: &dyn TopologyObserver = diagram_publisher
        .as_ref()
        .map(|publisher| publisher as &dyn TopologyObserver)
        .unwrap_or(&noop_observer);
    observer.run_started();

    let invocation_server = InvocationServer::start()?;
    let client = TmuxClient::new(crate::ea::ea_prefix(config.ea_id, config.base_prefix));
    spawn_topology_agents(&state, &client, &runtime_dir, &invocation_server, &config)?;
    let inputs = parse_inputs(&state, config.inputs)?;
    let executor = TmuxReactionExecutor {
        client,
        team: state.team.clone(),
        registry: invocation_server.registry.clone(),
        timeout: config.timeout,
    };
    let outputs = match run_event_loop_observed(&state, inputs, &executor, observer) {
        Ok(outputs) => outputs,
        Err(error) => {
            observer.run_failed(&error.to_string());
            return Err(error);
        }
    };
    observer.run_completed(&outputs);
    write_json_atomic(&runtime_dir.join("state.json"), &state)?;
    println!("Topology '{}' completed", state.team);
    for (port, value) in outputs {
        println!("Output {port} = {value}");
    }
    Ok(())
}

fn spawn_topology_agents(
    state: &VmState,
    client: &TmuxClient,
    runtime_dir: &Path,
    invocation_server: &InvocationServer,
    config: &TopologyRunConfig<'_>,
) -> Result<()> {
    let protocol = "You are an OMAR topology agent. Only act on OMAR INVOCATION messages. You cannot message other agents. For each invocation, use only omar_set_port to set allowed effects and omar_complete to finish. Port writes are buffered and repeated writes use last-writer-wins semantics.";
    for (name, agent) in &state.agents {
        let session = format!("{}{}", client.prefix(), name);
        if client.has_session(&session)? {
            if !config.replace {
                bail!(
                    "agent '{}' already exists; use --replace to restart it with scoped topology tools",
                    name
                );
            }
            client.ensure_session_not_attached(&session)?;
            client.kill_session(&session)?;
        }
        let agent_dir = runtime_dir.join("agents").join(name);
        fs::create_dir_all(&agent_dir)?;
        let prompt_file = agent_dir.join("system.md");
        fs::write(&prompt_file, protocol)?;
        let backend = canonical_backend(&agent.backend);
        let base_command = config::resolve_backend(backend).map_err(anyhow::Error::msg)?;
        let context = McpLaunchContext {
            omar_dir: config.omar_dir.to_path_buf(),
            ea_id: config.ea_id,
            session_prefix: config.base_prefix.to_string(),
            default_command: base_command.clone(),
            default_workdir: config.default_workdir.to_string(),
            health_idle_warning: config.health_idle_warning,
            tmux_server: std::env::var("OMAR_TMUX_SERVER").ok(),
            topology: Some(TopologyMcpContext {
                team: state.team.clone(),
                agent: name.clone(),
                endpoint: invocation_server.endpoint.clone(),
                token: invocation_server.token.clone(),
            }),
        };
        let command = manager::build_agent_command(&base_command, &prompt_file, &[], &context);
        client.new_session(&session, &command, Some(config.default_workdir))?;
    }

    for (name, agent) in &state.agents {
        let markers = crate::tmux::backend_readiness_markers(canonical_backend(&agent.backend));
        if !markers.is_empty()
            && !client.wait_for_markers(
                &format!("{}{}", client.prefix(), name),
                markers,
                Duration::from_secs(60),
                Duration::from_millis(250),
            )
        {
            bail!("agent '{}' did not become ready", name);
        }
    }
    Ok(())
}

fn parse_inputs(state: &VmState, raw_inputs: &[String]) -> Result<BTreeMap<String, Value>> {
    let mut inputs = BTreeMap::new();
    for raw in raw_inputs {
        let (name, raw_value) = raw
            .split_once('=')
            .with_context(|| format!("input '{raw}' must use NAME=VALUE"))?;
        let port = state
            .ports
            .get(name)
            .with_context(|| format!("unknown input port '{name}'"))?;
        if port.kind != PortKind::Input {
            bail!("port '{name}' is not an input");
        }
        let value = if port.ty == "path" {
            let path = fs::canonicalize(raw_value)
                .with_context(|| format!("input path '{}' does not exist", raw_value))?;
            Value::String(path.to_string_lossy().into_owned())
        } else {
            parse_input_value(&port.ty, raw_value)?
        };
        validate_value(&port.ty, &value)?;
        inputs.insert(name.to_string(), value);
    }
    for (name, port) in &state.ports {
        if port.kind == PortKind::Input && !inputs.contains_key(name) {
            bail!("missing input '{name}'");
        }
    }
    Ok(inputs)
}

fn parse_input_value(ty: &str, value: &str) -> Result<Value> {
    match ty {
        "bool" => Ok(Value::Bool(value.parse()?)),
        "int" => Ok(json!(value.parse::<i64>()?)),
        "float" => Ok(json!(value.parse::<f64>()?)),
        "string" | "path" | "bytes" => Ok(Value::String(value.to_string())),
        "signal" => Ok(Value::Null),
        _ => serde_json::from_str(value).context("complex input must be valid JSON"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Tag {
    timestamp: u64,
    microstep: u64,
}

impl Tag {
    const START: Self = Self {
        timestamp: 0,
        microstep: 0,
    };

    fn advance(self, delay: u64) -> Result<Self> {
        if delay == 0 {
            Ok(Self {
                timestamp: self.timestamp,
                microstep: self
                    .microstep
                    .checked_add(1)
                    .context("logical microstep overflow")?,
            })
        } else {
            Ok(Self {
                timestamp: self
                    .timestamp
                    .checked_add(delay)
                    .context("logical timestamp overflow")?,
                microstep: 0,
            })
        }
    }
}

fn enqueue_event(
    state: &VmState,
    queue: &mut BTreeMap<Tag, BTreeMap<String, Value>>,
    current_tag: Tag,
    port: &str,
    value: Value,
    additional_delay: u64,
) -> Result<()> {
    let fixed_delay = state
        .ports
        .get(port)
        .with_context(|| format!("unknown scheduled port '{port}'"))?
        .delay
        .unwrap_or(0);
    let total_delay = fixed_delay
        .checked_add(additional_delay)
        .context("logical delay overflow")?;
    let destination_tag = current_tag.advance(total_delay)?;
    queue
        .entry(destination_tag)
        .or_default()
        .insert(port.to_string(), value);
    Ok(())
}

#[cfg(test)]
fn run_event_loop<E: ReactionExecutor>(
    state: &VmState,
    inputs: BTreeMap<String, Value>,
    executor: &E,
) -> Result<BTreeMap<String, Value>> {
    run_event_loop_observed(state, inputs, executor, &NoopTopologyObserver)
}

fn run_event_loop_observed<E: ReactionExecutor>(
    state: &VmState,
    inputs: BTreeMap<String, Value>,
    executor: &E,
    observer: &dyn TopologyObserver,
) -> Result<BTreeMap<String, Value>> {
    let mut queue = BTreeMap::from([(Tag::START, inputs)]);
    let mut outputs = BTreeMap::new();
    let mut steps = 0usize;

    while let Some((tag, events)) = queue.pop_first() {
        observer.tag_advanced(tag.timestamp, tag.microstep, &events);
        steps += 1;
        if steps > 1024 {
            bail!("topology exceeded 1024 logical tags");
        }
        for (name, value) in &events {
            if state
                .ports
                .get(name)
                .is_some_and(|p| p.kind == PortKind::Output)
            {
                outputs.insert(name.clone(), value.clone());
            }
        }

        for connection in &state.connections {
            if let Some(value) = events.get(&connection.source) {
                enqueue_event(
                    state,
                    &mut queue,
                    tag,
                    &connection.target,
                    value.clone(),
                    connection.delay,
                )?;
            }
        }

        let mut enabled: Vec<_> = state
            .reactions
            .iter()
            .filter(|(_, reaction)| {
                reaction
                    .triggers
                    .iter()
                    .any(|trigger| events.contains_key(trigger))
            })
            .collect();
        enabled.sort_by_key(|(_, reaction)| reaction.order);
        if enabled.is_empty() {
            continue;
        }

        let layers = dag_layers(&enabled);
        let mut completed = Vec::new();
        for layer in layers {
            let specs: Vec<_> = layer
                .iter()
                .map(|index| invocation_spec(state, enabled[*index], &events))
                .collect::<Result<_>>()?;
            for spec in &specs {
                observer.reaction_started(
                    tag.timestamp,
                    tag.microstep,
                    &spec.reaction_id,
                    &spec.id,
                );
            }
            let invocation_ids: Vec<_> = specs.iter().map(|spec| spec.id.clone()).collect();
            let results = thread::scope(|scope| {
                let handles: Vec<_> = specs
                    .into_iter()
                    .map(|spec| scope.spawn(move || executor.invoke(spec)))
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| {
                        handle
                            .join()
                            .map_err(|_| anyhow::anyhow!("reaction executor panicked"))?
                    })
                    .collect::<Result<Vec<_>>>()
            })?;
            for ((index, invocation_id), writes) in
                layer.into_iter().zip(invocation_ids).zip(results)
            {
                observer.reaction_completed(
                    tag.timestamp,
                    tag.microstep,
                    enabled[index].0,
                    &invocation_id,
                    &writes,
                );
                completed.push((enabled[index].1.order, writes));
            }
        }

        completed.sort_by_key(|(order, _)| *order);
        for (_, writes) in completed {
            for (port, value) in writes {
                enqueue_event(state, &mut queue, tag, &port, value, 0)?;
            }
        }
    }
    Ok(outputs)
}

fn invocation_spec(
    state: &VmState,
    (reaction_id, reaction): (&String, &ReactionState),
    events: &BTreeMap<String, Value>,
) -> Result<InvocationSpec> {
    let trigger_values = reaction
        .triggers
        .iter()
        .filter_map(|trigger| {
            events
                .get(trigger)
                .map(|value| (trigger.clone(), value.clone()))
        })
        .collect();
    let allowed_effects = reaction
        .effects
        .iter()
        .map(|effect| {
            let port = state
                .ports
                .get(effect)
                .with_context(|| format!("unknown effect port '{effect}'"))?;
            Ok((effect.clone(), port.ty.clone()))
        })
        .collect::<Result<_>>()?;
    Ok(InvocationSpec {
        id: Uuid::new_v4().to_string(),
        reaction_id: reaction_id.clone(),
        agent: reaction.agent.clone(),
        trigger_values,
        allowed_effects,
        contract: reaction.contract.clone(),
        prompt: reaction.prompt.clone(),
    })
}

fn dag_layers(enabled: &[(&String, &ReactionState)]) -> Vec<Vec<usize>> {
    let mut outgoing = vec![Vec::new(); enabled.len()];
    let mut indegree = vec![0usize; enabled.len()];
    for earlier in 0..enabled.len() {
        for later in (earlier + 1)..enabled.len() {
            let left = enabled[earlier].1;
            let right = enabled[later].1;
            let overlaps = left
                .effects
                .iter()
                .any(|effect| right.effects.contains(effect));
            if overlaps || left.agent == right.agent {
                outgoing[earlier].push(later);
                indegree[later] += 1;
            }
        }
    }

    let mut remaining: BTreeSet<usize> = (0..enabled.len()).collect();
    let mut layers = Vec::new();
    while !remaining.is_empty() {
        let layer: Vec<_> = remaining
            .iter()
            .copied()
            .filter(|index| indegree[*index] == 0)
            .collect();
        debug_assert!(!layer.is_empty());
        for index in &layer {
            remaining.remove(index);
            for target in &outgoing[*index] {
                indegree[*target] -= 1;
            }
        }
        layers.push(layer);
    }
    layers
}

fn render_prompt(template: &str, values: &BTreeMap<String, Value>) -> Result<String> {
    let mut rendered = String::new();
    let mut remaining = template;
    while let Some(start) = remaining.find("$(") {
        rendered.push_str(&remaining[..start]);
        let expression = &remaining[start + 2..];
        let end = interpolation_end(expression).context("unterminated prompt interpolation")?;
        let identifier = interpolation_identifier(&expression[..end])?;
        match values.get(&identifier) {
            Some(Value::String(text)) => rendered.push_str(text),
            Some(other) => rendered.push_str(&other.to_string()),
            None => rendered.push_str("<absent>"),
        }
        remaining = &expression[end + 1..];
    }
    rendered.push_str(remaining);
    Ok(rendered)
}

fn interpolation_end(expression: &str) -> Option<usize> {
    let bytes = expression.as_bytes();
    let mut depth = 1usize;
    let mut index = 0usize;
    let mut block_comment = false;
    let mut line_comment = false;
    while index < bytes.len() {
        if line_comment {
            if bytes[index] == b'\n' {
                line_comment = false;
            }
            index += 1;
            continue;
        }
        if block_comment {
            if bytes.get(index..index + 2) == Some(b"*/") {
                block_comment = false;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if bytes.get(index..index + 2) == Some(b"//") {
            line_comment = true;
            index += 2;
        } else if bytes.get(index..index + 2) == Some(b"/*") {
            block_comment = true;
            index += 2;
        } else if bytes[index] == b'(' {
            depth += 1;
            index += 1;
        } else if bytes[index] == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
            index += 1;
        } else {
            index += 1;
        }
    }
    None
}

fn interpolation_identifier(expression: &str) -> Result<String> {
    let mut clean = String::new();
    let mut chars = expression.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(ch) = chars.next() {
                if ch == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
        } else if ch == '/' && chars.peek() == Some(&'/') {
            break;
        } else {
            clean.push(ch);
        }
    }
    clean
        .split_whitespace()
        .next()
        .map(str::to_string)
        .context("empty prompt interpolation")
}

fn valid_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn require_identifier(kind: &str, value: &str) -> Result<()> {
    if valid_identifier(value) {
        Ok(())
    } else {
        bail!("invalid {kind} name '{value}'")
    }
}

fn canonical_backend(backend: &str) -> &str {
    match backend.to_ascii_lowercase().as_str() {
        "claude" | "claudecode" => "claude",
        "codex" => "codex",
        "opencode" => "opencode",
        "cursor" => "cursor",
        "agy" => "agy",
        _ => backend,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn program() -> Bytecode {
        serde_json::from_str(
            r#"{
              "version": 1,
              "team": "Demo",
              "instructions": [
                {"op":"begin_plan","team":"Demo"},
                {"op":"spawn_agent","name":"worker","backend":"Codex"},
                {"op":"define_port","kind":"input","name":"request","type":"string"},
                {"op":"define_port","kind":"output","name":"done","type":"bool"},
                {"op":"install_reaction","id":"reaction.0","agent":"worker","triggers":["request"],"effects":["done"],"contract":"done","prompt":"Work"},
                {"op":"commit_plan"}
              ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn rejects_compiled_json_as_a_user_program() {
        let directory = tempfile::tempdir().unwrap();
        let bytecode_path = directory.path().join("workflow.json");
        fs::write(&bytecode_path, serde_json::to_vec(&program()).unwrap()).unwrap();

        let error = load_program_with_compiler(&bytecode_path, Some(Path::new("missing-omarc")))
            .unwrap_err();

        assert!(error.to_string().contains("must use the .omar extension"));
    }

    #[cfg(unix)]
    #[test]
    fn compiles_omar_source_before_loading_it() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("workflow.omar");
        fs::write(&source_path, serde_json::to_vec(&program()).unwrap()).unwrap();
        let compiler_path = directory.path().join("omarc");
        fs::write(&compiler_path, "#!/bin/sh\ncp \"$1\" \"$2\"\n").unwrap();
        let mut permissions = fs::metadata(&compiler_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&compiler_path, permissions).unwrap();

        let loaded = load_program_with_compiler(&source_path, Some(&compiler_path)).unwrap();

        assert_eq!(loaded.team, "Demo");
    }

    #[test]
    fn verifies_initial_topology() {
        let state = verify(&program()).unwrap();
        assert_eq!(state.agents.len(), 1);
        assert_eq!(state.ports.len(), 2);
        assert_eq!(state.reactions.len(), 1);
    }

    #[test]
    fn rejects_incomplete_topology() {
        let mut program = program();
        program.instructions.pop();
        assert!(verify(&program).is_err());
    }

    #[test]
    fn validates_nested_list_and_option_values_recursively() {
        validate_value("list<option<int>>", &json!([1, null, 2])).unwrap();
        validate_value("option<list<bool>>", &Value::Null).unwrap();
        validate_value("option<list<bool>>", &json!([true, false])).unwrap();

        assert!(validate_value("list<option<int>>", &json!([1, "two"])).is_err());
        assert!(validate_value("option<int>", &json!("one")).is_err());
        assert!(validate_value("list<int>", &json!("not a list")).is_err());
    }

    #[test]
    fn effect_contract_rejects_writes_to_unmentioned_ports() {
        let writes = BTreeMap::from([
            ("declared".to_string(), json!(true)),
            ("omitted".to_string(), json!(true)),
        ]);
        let error = validate_contract("declared", &writes).unwrap_err();
        assert!(error
            .to_string()
            .contains("effect 'omitted' is not permitted"));
    }

    #[test]
    fn atomic_json_write_replaces_complete_document() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        write_json_atomic(&path, &json!({"revision": 1})).unwrap();
        write_json_atomic(&path, &json!({"revision": 2, "ready": true})).unwrap();

        let stored: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(stored, json!({"revision": 2, "ready": true}));
    }

    struct HrExecutor {
        calls: Mutex<Vec<String>>,
    }

    impl ReactionExecutor for HrExecutor {
        fn invoke(&self, invocation: InvocationSpec) -> Result<BTreeMap<String, Value>> {
            self.calls
                .lock()
                .unwrap()
                .push(invocation.reaction_id.clone());
            let writes = match invocation.reaction_id.as_str() {
                "reaction.0" => {
                    BTreeMap::from([("triage".to_string(), json!("strong systems candidate"))])
                }
                "reaction.1" => {
                    assert_eq!(
                        invocation.trigger_values.get("triage"),
                        Some(&json!("strong systems candidate"))
                    );
                    BTreeMap::from([("opinion1".to_string(), json!("strong engineer"))])
                }
                "reaction.2" => {
                    assert_eq!(
                        invocation.trigger_values.get("triage"),
                        Some(&json!("strong systems candidate"))
                    );
                    BTreeMap::from([("opinion2".to_string(), json!("good judgment"))])
                }
                "reaction.3" => {
                    assert_eq!(
                        invocation.trigger_values.get("opinion1"),
                        Some(&json!("strong engineer"))
                    );
                    assert_eq!(
                        invocation.trigger_values.get("opinion2"),
                        Some(&json!("good judgment"))
                    );
                    BTreeMap::from([("hired".to_string(), json!(true))])
                }
                other => panic!("unexpected reaction {other}"),
            };
            validate_contract(&invocation.contract, &writes)?;
            Ok(writes)
        }
    }

    fn hr_state() -> VmState {
        let mut state = VmState {
            version: 1,
            team: "HR".into(),
            agents: BTreeMap::from([
                (
                    "manager".into(),
                    AgentState {
                        backend: "claude".into(),
                    },
                ),
                (
                    "reviewer1".into(),
                    AgentState {
                        backend: "codex".into(),
                    },
                ),
                (
                    "reviewer2".into(),
                    AgentState {
                        backend: "opencode".into(),
                    },
                ),
            ]),
            ports: BTreeMap::new(),
            connections: Vec::new(),
            reactions: BTreeMap::new(),
        };
        for (name, kind, ty) in [
            ("resume", PortKind::Input, "path"),
            ("hired", PortKind::Output, "bool"),
            ("triage", PortKind::Action, "string"),
            ("opinion1", PortKind::Action, "string"),
            ("opinion2", PortKind::Action, "string"),
            ("log", PortKind::Action, "string"),
        ] {
            state.ports.insert(
                name.into(),
                PortState {
                    kind,
                    ty: ty.into(),
                    delay: None,
                },
            );
        }
        for (order, id, agent, triggers, effects, contract) in [
            (
                0,
                "reaction.0",
                "manager",
                vec!["resume"],
                vec!["triage", "hired", "log"],
                "( triage | hired = false ) , log ?",
            ),
            (
                1,
                "reaction.1",
                "reviewer1",
                vec!["triage"],
                vec!["opinion1"],
                "opinion1",
            ),
            (
                2,
                "reaction.2",
                "reviewer2",
                vec!["triage"],
                vec!["opinion2"],
                "opinion2",
            ),
            (
                3,
                "reaction.3",
                "manager",
                vec!["opinion1", "opinion2"],
                vec!["hired"],
                "hired",
            ),
        ] {
            state.reactions.insert(
                id.into(),
                ReactionState {
                    order,
                    agent: agent.into(),
                    triggers: triggers.into_iter().map(str::to_string).collect(),
                    effects: effects.into_iter().map(str::to_string).collect(),
                    contract: contract.into(),
                    prompt: "prompt".into(),
                },
            );
        }
        state
    }

    #[test]
    fn runs_hr_dataflow_to_completion() {
        let executor = HrExecutor {
            calls: Mutex::new(Vec::new()),
        };
        let outputs = run_event_loop(
            &hr_state(),
            BTreeMap::from([("resume".into(), json!("/tmp/resume.txt"))]),
            &executor,
        )
        .unwrap();
        assert_eq!(outputs, BTreeMap::from([("hired".into(), json!(true))]));

        let calls = executor.calls.lock().unwrap();
        assert_eq!(calls.first().map(String::as_str), Some("reaction.0"));
        assert_eq!(calls.last().map(String::as_str), Some("reaction.3"));
        assert!(calls[1..3].contains(&"reaction.1".to_string()));
        assert!(calls[1..3].contains(&"reaction.2".to_string()));
    }

    #[test]
    fn superdense_tags_advance_microstep_or_timestamp() {
        let tag = Tag {
            timestamp: 10,
            microstep: 7,
        };
        assert_eq!(
            tag.advance(0).unwrap(),
            Tag {
                timestamp: 10,
                microstep: 8
            }
        );
        assert_eq!(
            tag.advance(2).unwrap(),
            Tag {
                timestamp: 12,
                microstep: 0
            }
        );
    }

    struct SuperdenseExecutor {
        calls: Mutex<Vec<String>>,
    }

    impl ReactionExecutor for SuperdenseExecutor {
        fn invoke(&self, invocation: InvocationSpec) -> Result<BTreeMap<String, Value>> {
            self.calls
                .lock()
                .unwrap()
                .push(invocation.reaction_id.clone());
            let writes = match invocation.reaction_id.as_str() {
                "reaction.0" => {
                    BTreeMap::from([("immediate".into(), json!(7)), ("fixed".into(), json!(7))])
                }
                "reaction.1" => BTreeMap::from([("fixed_result".into(), json!(7))]),
                "reaction.2" => BTreeMap::from([("connected_result".into(), json!(7))]),
                other => panic!("unexpected reaction {other}"),
            };
            validate_contract(&invocation.contract, &writes)?;
            Ok(writes)
        }
    }

    #[test]
    fn fixed_and_connection_delays_order_reactions_by_timestamp() {
        let bytecode: Bytecode = serde_json::from_str(
            r#"{
              "version": 1,
              "team": "Superdense",
              "instructions": [
                {"op":"begin_plan","team":"Superdense"},
                {"op":"spawn_agent","name":"producer","backend":"Codex"},
                {"op":"spawn_agent","name":"fixed_consumer","backend":"Codex"},
                {"op":"spawn_agent","name":"connected_consumer","backend":"Codex"},
                {"op":"define_port","kind":"input","name":"start","type":"int"},
                {"op":"define_port","kind":"output","name":"fixed_result","type":"int"},
                {"op":"define_port","kind":"output","name":"connected_result","type":"int"},
                {"op":"define_port","kind":"action","name":"immediate","type":"int"},
                {"op":"define_port","kind":"action","name":"fixed","type":"int","delay":2},
                {"op":"define_port","kind":"action","name":"connected","type":"int","delay":1},
                {"op":"connect_ports","source":"immediate","target":"connected","delay":3},
                {"op":"install_reaction","id":"reaction.0","agent":"producer","triggers":["start"],"effects":["immediate","fixed"],"contract":"immediate , fixed","prompt":"produce"},
                {"op":"install_reaction","id":"reaction.1","agent":"fixed_consumer","triggers":["fixed"],"effects":["fixed_result"],"contract":"fixed_result","prompt":"fixed"},
                {"op":"install_reaction","id":"reaction.2","agent":"connected_consumer","triggers":["connected"],"effects":["connected_result"],"contract":"connected_result","prompt":"connected"},
                {"op":"commit_plan"}
              ]
            }"#,
        )
        .unwrap();
        let state = verify(&bytecode).unwrap();
        let executor = SuperdenseExecutor {
            calls: Mutex::new(Vec::new()),
        };
        let outputs = run_event_loop(
            &state,
            BTreeMap::from([("start".into(), json!(7))]),
            &executor,
        )
        .unwrap();

        assert_eq!(
            outputs,
            BTreeMap::from([
                ("connected_result".into(), json!(7)),
                ("fixed_result".into(), json!(7)),
            ])
        );
        assert_eq!(
            *executor.calls.lock().unwrap(),
            vec!["reaction.0", "reaction.1", "reaction.2"]
        );
    }

    struct OrTriggerExecutor {
        invocations: Mutex<Vec<InvocationSpec>>,
    }

    impl ReactionExecutor for OrTriggerExecutor {
        fn invoke(&self, invocation: InvocationSpec) -> Result<BTreeMap<String, Value>> {
            self.invocations.lock().unwrap().push(invocation);
            Ok(BTreeMap::from([("hired".to_string(), json!(true))]))
        }
    }

    #[test]
    fn reaction_fires_when_any_declared_trigger_is_present() {
        let mut state = hr_state();
        state.reactions.retain(|id, _| id.as_str() == "reaction.3");
        let executor = OrTriggerExecutor {
            invocations: Mutex::new(Vec::new()),
        };

        let outputs = run_event_loop(
            &state,
            BTreeMap::from([("opinion1".into(), json!("strong engineer"))]),
            &executor,
        )
        .unwrap();

        assert_eq!(outputs, BTreeMap::from([("hired".into(), json!(true))]));
        let invocations = executor.invocations.lock().unwrap();
        assert_eq!(invocations.len(), 1);
        assert_eq!(
            invocations[0].trigger_values,
            BTreeMap::from([("opinion1".into(), json!("strong engineer"))])
        );
    }

    #[test]
    fn reaction_fires_once_when_multiple_triggers_share_a_tag() {
        let mut state = hr_state();
        state.reactions.retain(|id, _| id.as_str() == "reaction.3");
        let executor = OrTriggerExecutor {
            invocations: Mutex::new(Vec::new()),
        };

        run_event_loop(
            &state,
            BTreeMap::from([
                ("opinion1".into(), json!("strong engineer")),
                ("opinion2".into(), json!("good judgment")),
            ]),
            &executor,
        )
        .unwrap();

        let invocations = executor.invocations.lock().unwrap();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].trigger_values.len(), 2);
    }

    #[test]
    fn absent_trigger_interpolation_is_explicit() {
        let values = BTreeMap::from([("left".into(), json!("ready"))]);
        assert_eq!(
            render_prompt("left=$(left), right=$(right)", &values).unwrap(),
            "left=ready, right=<absent>"
        );
    }

    #[test]
    fn dag_orders_overlapping_effects_and_same_agent() {
        let mut state = hr_state();
        state.reactions.get_mut("reaction.2").unwrap().effects = vec!["opinion1".into()];
        state.reactions.get_mut("reaction.2").unwrap().agent = "reviewer1".into();
        let enabled = [
            (
                "reaction.1".to_string(),
                state.reactions["reaction.1"].clone(),
            ),
            (
                "reaction.2".to_string(),
                state.reactions["reaction.2"].clone(),
            ),
        ];
        let refs: Vec<_> = enabled
            .iter()
            .map(|(id, reaction)| (id, reaction))
            .collect();
        assert_eq!(dag_layers(&refs), vec![vec![0], vec![1]]);
    }

    #[test]
    fn reactive_scoped_port_writes_are_last_writer_wins() {
        let server = InvocationServer::start().unwrap();
        let context = TopologyMcpContext {
            team: "Demo".into(),
            agent: "worker".into(),
            endpoint: server.endpoint.clone(),
            token: server.token.clone(),
        };
        let record = InvocationRecord {
            id: "invocation-1".into(),
            team: "Demo".into(),
            agent: "worker".into(),
            reaction: "reaction.0".into(),
            contract: "opinion".into(),
            allowed_effects: BTreeMap::from([("opinion".into(), "string".into())]),
            writes: BTreeMap::new(),
            completed: false,
        };
        let completion = server.registry.register(record).unwrap();

        for value in ["first", "second"] {
            mcp_set_port(
                &context,
                json!({"invocation_id":"invocation-1", "port":"opinion", "value":value}),
            )
            .unwrap();
        }
        mcp_complete(&context, json!({"invocation_id":"invocation-1"})).unwrap();

        let writes = completion.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(writes["opinion"], json!("second"));
        server.registry.remove("invocation-1");
    }

    #[test]
    fn reactive_invocations_enforce_owner_and_contract() {
        let server = InvocationServer::start().unwrap();
        let context = TopologyMcpContext {
            team: "Demo".into(),
            agent: "worker".into(),
            endpoint: server.endpoint.clone(),
            token: server.token.clone(),
        };
        let record = InvocationRecord {
            id: "invocation-2".into(),
            team: "Demo".into(),
            agent: "worker".into(),
            reaction: "reaction.0".into(),
            contract: "opinion".into(),
            allowed_effects: BTreeMap::from([("opinion".into(), "string".into())]),
            writes: BTreeMap::new(),
            completed: false,
        };
        let completion = server.registry.register(record).unwrap();

        let mut wrong_owner = context.clone();
        wrong_owner.agent = "intruder".into();
        let owner_error = mcp_set_port(
            &wrong_owner,
            json!({"invocation_id":"invocation-2", "port":"opinion", "value":"no"}),
        )
        .unwrap_err();
        assert!(owner_error.to_string().contains("does not belong"));

        let contract_error =
            mcp_complete(&context, json!({"invocation_id":"invocation-2"})).unwrap_err();
        assert!(
            format!("{contract_error:#}").contains("effect contract 'opinion' is not satisfied")
        );

        mcp_set_port(
            &context,
            json!({"invocation_id":"invocation-2", "port":"opinion", "value":"yes"}),
        )
        .unwrap();
        mcp_complete(&context, json!({"invocation_id":"invocation-2"})).unwrap();
        assert_eq!(
            completion.recv_timeout(Duration::from_secs(1)).unwrap(),
            BTreeMap::from([("opinion".into(), json!("yes"))])
        );
        server.registry.remove("invocation-2");
    }
}
