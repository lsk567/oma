use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::config;
use crate::deploy::{self, DeploymentState};
use crate::diagram::{DiagramServer, NoopTopologyObserver, TopologyObserver};
use crate::manager::{self, McpLaunchContext, TopologyMcpContext};
use crate::tmux::flatten_agent_name;
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
    /// A team instantiated by `main`. The container a diagram draws.
    DeclareInstance {
        name: String,
        team: String,
        /// The instance that declared it, empty for one `main` declared.
        #[serde(default)]
        parent: String,
    },
    SpawnAgent {
        name: String,
        backend: String,
        #[serde(default)]
        instance: String,
    },
    DefinePort {
        name: String,
        kind: PortKind,
        #[serde(rename = "type")]
        ty: String,
        #[serde(default)]
        delay: Option<u64>,
        #[serde(default)]
        instance: String,
    },
    /// A trigger the runtime fires itself, from the logical clock.
    DeclareTimer {
        name: String,
        offset: u64,
        period: u64,
        #[serde(default)]
        instance: String,
    },
    ConnectPorts {
        source: String,
        target: String,
        /// Absent for a plain connection, which is instantaneous. `Some(0)` is
        /// `after 0` and costs a microstep; larger values are nanoseconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delay: Option<u64>,
    },
    InstallReaction {
        id: String,
        agent: String,
        triggers: Vec<String>,
        effects: Vec<String>,
        contract: String,
        prompt: String,
        #[serde(default)]
        instance: String,
        /// Nanoseconds one invocation may take before it is given up on.
        /// Absent leaves it to the run-wide timeout.
        #[serde(default)]
        within: Option<u64>,
    },
    CommitPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum PortKind {
    Input,
    Output,
    Action,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    pub backend: String,
    /// Which instance declared it. Empty only for bytecode that predates
    /// instances being carried through.
    #[serde(default)]
    pub instance: String,
}

/// A team instantiated by `main`, and the team it came from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceState {
    pub team: String,
    /// Which instance declared it. Empty for a top-level one, and for bytecode
    /// that predates teams being able to instantiate teams.
    #[serde(default)]
    pub parent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortState {
    pub kind: PortKind,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<u64>,
    #[serde(default)]
    pub instance: String,
}

/// `timer t(offset, period)`.
///
/// It occupies the same trigger namespace as ports but is not one: nothing can
/// write to it, and it carries no value of its own beyond the time it fired.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimerState {
    pub offset: u64,
    /// `0` fires once. Anything else re-arms, forever.
    pub period: u64,
    #[serde(default)]
    pub instance: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionState {
    pub source: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionState {
    pub order: usize,
    pub agent: String,
    #[serde(default)]
    pub instance: String,
    pub triggers: Vec<String>,
    pub effects: Vec<String>,
    pub contract: String,
    pub prompt: String,
    /// Nanoseconds one invocation may take. `None` uses the run-wide timeout.
    #[serde(default)]
    pub within: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmState {
    pub version: u32,
    pub team: String,
    #[serde(default)]
    pub instances: BTreeMap<String, InstanceState>,
    pub agents: BTreeMap<String, AgentState>,
    #[serde(default)]
    pub timers: BTreeMap<String, TimerState>,
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
        instances: BTreeMap::new(),
        agents: BTreeMap::new(),
        timers: BTreeMap::new(),
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
            Instruction::DeclareInstance { name, team, parent } => {
                require_identifier("instance", name)?;
                if team.trim().is_empty() {
                    bail!("instance '{name}' names no team");
                }
                // A parent is declared before its children, so this also
                // rejects a cycle: neither end could be declared first.
                if !parent.is_empty() && !state.instances.contains_key(parent) {
                    bail!("instance '{name}' names undeclared parent '{parent}'");
                }
                if state
                    .instances
                    .insert(
                        name.clone(),
                        InstanceState {
                            team: team.clone(),
                            parent: parent.clone(),
                        },
                    )
                    .is_some()
                {
                    bail!("duplicate instance '{name}'");
                }
            }
            Instruction::SpawnAgent {
                name,
                backend,
                instance,
            } => {
                check_instance(&state, "agent", name, instance)?;
                require_identifier("agent", name)?;
                if backend.trim().is_empty() {
                    bail!("agent '{name}' has an empty backend");
                }
                // Agents live in tmux sessions, and qualification is flattened
                // to get there, so two names that differ only by a '.' would
                // land in one session and answer each other's invocations.
                if let Some(clash) = state
                    .agents
                    .keys()
                    .find(|existing| flatten_agent_name(existing) == flatten_agent_name(name))
                {
                    bail!("agents '{clash}' and '{name}' need the same tmux session");
                }
                if state
                    .agents
                    .insert(
                        name.clone(),
                        AgentState {
                            backend: backend.clone(),
                            instance: instance.clone(),
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
                instance,
            } => {
                require_identifier("port", name)?;
                check_instance(&state, "port", name, instance)?;
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
                            instance: instance.clone(),
                        },
                    )
                    .is_some()
                {
                    bail!("duplicate port '{name}'");
                }
            }
            Instruction::DeclareTimer {
                name,
                offset,
                period,
                instance,
            } => {
                require_identifier("timer", name)?;
                check_instance(&state, "timer", name, instance)?;
                // Triggers are looked up by name in one namespace, so a timer
                // that shadowed a port would silently take its reactions.
                if state.ports.contains_key(name) {
                    bail!("timer '{name}' is also a port; a trigger has one name");
                }
                if *offset == 0 && *period == 0 {
                    bail!("timer '{name}' never fires; give it an offset, a period, or both");
                }
                if state
                    .timers
                    .insert(
                        name.clone(),
                        TimerState {
                            offset: *offset,
                            period: *period,
                            instance: instance.clone(),
                        },
                    )
                    .is_some()
                {
                    bail!("duplicate timer '{name}'");
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
                // Targeting an input is how one instance feeds the next, so it
                // is allowed; the port kinds only have to agree on type.
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
                instance,
                within,
            } => {
                let order = state.reactions.len();
                check_instance(&state, "reaction", id, instance)?;
                if !state.agents.contains_key(agent) {
                    bail!("reaction '{id}' references unknown agent '{agent}'");
                }
                for trigger in triggers {
                    if state.timers.contains_key(trigger) {
                        continue;
                    }
                    let port = state.ports.get(trigger).with_context(|| {
                        format!("reaction '{id}' has unknown trigger '{trigger}'")
                    })?;
                    // A reaction may read the output of a team its own team
                    // instantiated — that is how a parent observes what it
                    // contains. Its own output is what it is there to write.
                    if port.kind == PortKind::Output && port.instance == *instance {
                        bail!("reaction '{id}' cannot be triggered by output '{trigger}'");
                    }
                }
                for effect in effects {
                    if state.timers.contains_key(effect) {
                        bail!("reaction '{id}' cannot write to timer '{effect}'");
                    }
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
                            instance: instance.clone(),
                            agent: agent.clone(),
                            triggers: triggers.clone(),
                            effects: effects.clone(),
                            contract: contract.clone(),
                            prompt: prompt.clone(),
                            within: *within,
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
    reject_causality_loops(&state)?;
    Ok(state)
}

/// Ports a value written to `port` reaches without any logical delay.
///
/// A write incurs the target's own fixed delay, and a connection adds its own,
/// so a hop is instantaneous only when both are zero. Following those hops is
/// what says which reactions must be ordered against each other at one tag.
fn zero_delay_reach(state: &VmState, port: &str) -> BTreeSet<String> {
    let mut reached = BTreeSet::new();
    if !port_delay(state, port).is_instant() {
        return reached;
    }
    let mut frontier = vec![port.to_string()];
    while let Some(name) = frontier.pop() {
        if !reached.insert(name.clone()) {
            continue;
        }
        for connection in &state.connections {
            if connection.source != name {
                continue;
            }
            let hop = connection_delay(connection)
                .then(port_delay(state, &connection.target))
                .unwrap_or(Delay::MICROSTEP);
            if hop.is_instant() {
                frontier.push(connection.target.clone());
            }
        }
    }
    reached
}

/// Reaction ids that must run after `id` at a tag it fires in.
///
/// Three reasons to be ordered: one writes a port the other is triggered by,
/// so the second cannot decide whether its trigger is present until the first
/// has run; both write the same port, so the later declaration wins; or they
/// share an agent, which answers one invocation at a time.
pub fn must_follow(state: &VmState, id: &str) -> BTreeSet<String> {
    let Some(reaction) = state.reactions.get(id) else {
        return BTreeSet::new();
    };
    // An action carries a microstep even at zero delay, so writing one settles
    // nothing at this tag and orders nobody.
    let instant_writes: BTreeSet<String> = reaction
        .effects
        .iter()
        .flat_map(|effect| zero_delay_reach(state, effect))
        .collect();
    state
        .reactions
        .iter()
        .filter(|(other, _)| other.as_str() != id)
        .filter(|(other, state_of)| {
            let reads = state_of
                .triggers
                .iter()
                .any(|trigger| instant_writes.contains(trigger));
            let shares_port = state_of
                .effects
                .iter()
                .any(|effect| reaction.effects.contains(effect));
            let shares_agent = state_of.agent == reaction.agent;
            // Declaration order decides the last two; only the first is a
            // dependency the program states rather than a tie to break.
            reads
                || ((shares_port || shares_agent)
                    && state_of.order > reaction.order
                    && other.as_str() != id)
        })
        .map(|(other, _)| other.clone())
        .collect()
}

/// A cycle in that ordering is a tag that cannot be scheduled.
///
/// Every hop in it is instantaneous, so each reaction would have to run before
/// the other at one tag. Lingua Franca rejects the same shape for the same
/// reason. A loop through an action is not one: an action carries a microstep,
/// so the second firing is at a later tag and the order is decided by time
/// rather than by precedence.
fn reject_causality_loops(state: &VmState) -> Result<()> {
    let mut colour: BTreeMap<&str, u8> = state
        .reactions
        .keys()
        .map(|id| (id.as_str(), 0u8))
        .collect();
    let edges: BTreeMap<String, BTreeSet<String>> = state
        .reactions
        .keys()
        .map(|id| (id.clone(), must_follow(state, id)))
        .collect();

    // Iterative depth-first search: a topology can nest deeply enough that
    // recursion is a stack overflow rather than a compile error.
    for root in state.reactions.keys() {
        if colour[root.as_str()] != 0 {
            continue;
        }
        let mut path: Vec<String> = Vec::new();
        let mut stack: Vec<(String, bool)> = vec![(root.clone(), false)];
        while let Some((id, leaving)) = stack.pop() {
            if leaving {
                colour.insert(state.reactions.get_key_value(&id).unwrap().0.as_str(), 2);
                path.pop();
                continue;
            }
            match colour[id.as_str()] {
                1 => {
                    let from = path.iter().position(|step| *step == id).unwrap_or(0);
                    let mut cycle: Vec<&str> = path[from..].iter().map(String::as_str).collect();
                    cycle.push(&id);
                    bail!(
                        "causality loop: {} — every hop is instantaneous, so none of these can run first",
                        cycle.join(" -> ")
                    );
                }
                2 => continue,
                _ => {}
            }
            colour.insert(state.reactions.get_key_value(&id).unwrap().0.as_str(), 1);
            path.push(id.clone());
            stack.push((id.clone(), true));
            for next in edges.get(&id).into_iter().flatten() {
                if colour[next.as_str()] != 2 {
                    stack.push((next.clone(), false));
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct InvocationRecord {
    id: String,
    team: String,
    agent: String,
    reaction: String,
    contract: String,
    allowed_effects: BTreeMap<String, String>,
    /// What the reaction was triggered by, and the instruction as the agent
    /// would read it. An agent in a pane is handed both in its prompt; a human
    /// has no pane, so the record is where a panel reads them from.
    trigger_values: BTreeMap<String, Value>,
    prompt: String,
    writes: BTreeMap<String, Value>,
    completed: bool,
}

/// One invocation waiting to be answered, as a panel needs to draw it.
///
/// Only what the invocation already scopes: the values fed in, and the ports
/// this reaction may write with their types. Nothing else in the run is
/// reachable from here, because nothing else was wired to it.
#[derive(Debug, Serialize, Deserialize)]
struct PendingInvocation {
    invocation_id: String,
    reaction: String,
    contract: String,
    prompt: String,
    trigger_values: BTreeMap<String, Value>,
    allowed_effects: BTreeMap<String, String>,
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
    /// What this agent owes right now. An agent in a pane is told; a human has
    /// to be able to ask, because a panel can be opened at any time and a
    /// reload must not lose the invocation.
    Pending,
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

    /// Whether this invocation has already been answered.
    ///
    /// Delivery needs this because an agent that reads its pane a line at a
    /// time answers before the submit, leaving the terminal with no change to
    /// show for it. An entry that has gone counts as answered: there is
    /// nothing left to deliver to either way.
    fn answered(&self, invocation_id: &str) -> bool {
        match self.entries.lock() {
            Ok(entries) => entries
                .get(invocation_id)
                .is_none_or(|entry| entry.record.completed),
            // A poisoned lock is not evidence of an answer, and claiming one
            // would turn a lost delivery into a silent hang.
            Err(_) => false,
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
            InvocationCommand::Pending => {
                let pending: Vec<PendingInvocation> = entries
                    .values()
                    .filter(|entry| {
                        !entry.record.completed
                            && entry.record.team == team
                            && entry.record.agent == agent
                    })
                    .map(|entry| PendingInvocation {
                        invocation_id: entry.record.id.clone(),
                        reaction: entry.record.reaction.clone(),
                        contract: entry.record.contract.clone(),
                        prompt: entry.record.prompt.clone(),
                        trigger_values: entry.record.trigger_values.clone(),
                        allowed_effects: entry.record.allowed_effects.clone(),
                    })
                    .collect();
                Ok(json!({ "pending": pending }))
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

/// What this agent owes right now, for a panel to draw.
///
/// Scoped to the asking agent, so what comes back is what that agent's
/// reactions were wired to see and allowed to set, and nothing else in the run.
pub(crate) fn pending_invocations(context: &TopologyMcpContext) -> Result<Value> {
    send_invocation_command(context, InvocationCommand::Pending)
}

fn panel_context(access: &PanelAccess, team: &str, agent: &str) -> TopologyMcpContext {
    TopologyMcpContext {
        team: team.to_string(),
        agent: agent.to_string(),
        endpoint: access.endpoint.clone(),
        token: access.token.clone(),
    }
}

/// Everything the run's web agents are waiting on, each tagged with whose it is.
///
/// Asked per agent because an invocation is addressed to one, which is also
/// what keeps a panel from being handed work that belongs to a pane.
pub fn panel_pending(access: &PanelAccess, team: &str) -> Result<Vec<Value>> {
    let mut waiting = Vec::new();
    for agent in &access.agents {
        let answer = pending_invocations(&panel_context(access, team, agent))?;
        let Some(items) = answer.get("pending").and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            let mut item = item.clone();
            if let Some(object) = item.as_object_mut() {
                object.insert("agent".to_string(), json!(agent));
            }
            waiting.push(item);
        }
    }
    Ok(waiting)
}

/// Answer one invocation: every value, then complete.
///
/// The whole batch lands as one completion, so a reaction reading several of
/// these ports sees them together — the same reason the runtime drains a tag's
/// writes at once rather than per reaction. A rejected value fails here, before
/// anything is completed, so a bad field cannot leave a half-answered
/// invocation behind.
pub fn panel_submit(
    access: &PanelAccess,
    team: &str,
    agent: &str,
    invocation_id: &str,
    values: &BTreeMap<String, Value>,
) -> Result<()> {
    let context = panel_context(access, team, agent);
    for (port, value) in values {
        send_invocation_command(
            &context,
            InvocationCommand::SetPort(SetPortArgs {
                invocation_id: invocation_id.to_string(),
                port: port.clone(),
                value: value.clone(),
            }),
        )?;
    }
    send_invocation_command(
        &context,
        InvocationCommand::Complete(CompleteArgs {
            invocation_id: invocation_id.to_string(),
        }),
    )?;
    Ok(())
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
    /// What the program allows this invocation, overriding the run-wide
    /// timeout. `None` means the program said nothing and the run's applies.
    within: Option<Duration>,
}

trait ReactionExecutor: Sync {
    fn invoke(&self, invocation: InvocationSpec) -> Result<BTreeMap<String, Value>>;
}

/// What an expired deadline means, which the effect contract decides.
///
/// A contract that writing nothing already satisfies is one the program said
/// may stay silent, so the tag completes with no writes and whatever those
/// ports feed simply does not fire. A contract that requires an effect has not
/// been honoured and there is no value to invent, so the run stops rather than
/// carrying on from a decision nobody made.
fn expired(invocation: &InvocationSpec, deadline: Duration) -> Result<BTreeMap<String, Value>> {
    match validate_contract(&invocation.contract, &BTreeMap::new()) {
        Ok(()) => Ok(BTreeMap::new()),
        Err(_) => bail!(
            "reaction '{}' invocation '{}': '{}' did not answer within {:?}, and contract '{}' requires an effect",
            invocation.reaction_id,
            invocation.id,
            invocation.agent,
            deadline,
            invocation.contract
        ),
    }
}

struct AgentReactionExecutor {
    client: TmuxClient,
    team: String,
    registry: InvocationRegistry,
    timeout: Duration,
    /// Agents on the `web` backend. Nothing was spawned for them, so there is
    /// no pane to deliver a prompt to — the registration is the work item, and
    /// a web client picks it up from there.
    web: BTreeSet<String>,
}

impl ReactionExecutor for AgentReactionExecutor {
    fn invoke(&self, invocation: InvocationSpec) -> Result<BTreeMap<String, Value>> {
        let invocation_id = invocation.id.clone();
        let rendered = render_prompt(&invocation.prompt, &invocation.trigger_values)?;
        let record = InvocationRecord {
            id: invocation_id.clone(),
            team: self.team.clone(),
            agent: invocation.agent.clone(),
            reaction: invocation.reaction_id.clone(),
            contract: invocation.contract.clone(),
            allowed_effects: invocation.allowed_effects.clone(),
            trigger_values: invocation.trigger_values.clone(),
            prompt: rendered.clone(),
            writes: BTreeMap::new(),
            completed: false,
        };
        let completion = self.registry.register(record)?;

        // A web agent has no pane to deliver to: nothing was spawned for it,
        // and the registration above is the work item a panel asks for. What
        // follows is the same either way — a client answering is a slow agent,
        // and the registry does not care which kind it is waiting on.
        if !self.web.contains(&invocation.agent) {
            let message = format!(
                "OMAR INVOCATION\ninvocation_id: {}\ntriggers: {}\neffects: {}\ncontract: {}\n\n{}\n\nUse omar_set_port for each effect you choose, then call omar_complete exactly once. For a signal effect, set its value to null. Do not address another agent directly.",
                invocation.id,
                serde_json::to_string(&invocation.trigger_values)?,
                serde_json::to_string(&invocation.allowed_effects)?,
                invocation.contract,
                rendered
            );
            let session = self.client.session_for(&invocation.agent);
            if let Err(error) = self
                .client
                .deliver_prompt_until(&session, &message, &DeliveryOptions::default(), &|| {
                    self.registry.answered(&invocation_id)
                })
                .with_context(|| format!("failed to deliver {}", invocation.reaction_id))
            {
                self.registry.remove(&invocation_id);
                return Err(error);
            }
        }

        let deadline = invocation.within.unwrap_or(self.timeout);
        let result = match completion.recv_timeout(deadline) {
            Ok(writes) => Ok(writes),
            Err(mpsc::RecvTimeoutError::Timeout) => expired(&invocation, deadline),
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
    /// How the run's logical clock is paced against the wall clock.
    pub pace: Pace,
    pub diagram_address: Option<std::net::SocketAddr>,
    /// Receives the bound diagram address once the server is up. `omar serve`
    /// needs it while the run is still in flight; `omar run` prints it instead.
    pub diagram_ready: Option<mpsc::Sender<std::net::SocketAddr>>,
    /// Receives the invocation service's address and token once it is up.
    ///
    /// A `web` agent has nothing spawned for it, so nothing is ever handed the
    /// credentials a pane's MCP sidecar gets from its context file. Without
    /// this the daemon cannot answer on a client's behalf, and a web-backed
    /// reaction would always sit until its deadline.
    pub panel_ready: Option<mpsc::Sender<PanelAccess>>,
}

/// Where a run's invocations are answered, and the secret that authorises it.
///
/// Loopback-only and per run, like the service it points at. Held by the daemon
/// rather than handed to a browser: it authorises writing any effect of any
/// invocation in the run, which is not a capability a page should carry.
#[derive(Debug, Clone)]
pub struct PanelAccess {
    pub endpoint: String,
    pub token: String,
    /// The `web` agents in this run. Everything else answers through its own
    /// pane, and a panel has no business offering to answer for them.
    pub agents: BTreeSet<String>,
}

/// How a run ended when it did not fail: it drained its queue, or an
/// operator's stop closed it at a tag boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEnd {
    Completed,
    Stopped,
}

/// Advance the shared record and persist it, as one step.
fn advance_record(
    record: &Arc<Mutex<deploy::DeploymentRecord>>,
    dir: &Path,
    next: DeploymentState,
    detail: Option<&str>,
) -> Result<()> {
    let mut guard = record
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment record lock poisoned"))?;
    guard.advance(next, detail)?;
    guard.save(dir)
}

/// The failure funnel: keep pane output as logs, kill the sessions, record
/// FAILED. Best effort; the caller reports the error already on its way out.
fn fail_deployment(
    record: &Arc<Mutex<deploy::DeploymentRecord>>,
    dir: &Path,
    host: &dyn deploy::SessionHost,
    sessions: &BTreeMap<String, String>,
    error: &anyhow::Error,
) {
    for failure in deploy::teardown_sessions(host, sessions, &deploy::logs_dir(dir)) {
        eprintln!("warning: session not cleaned up: {failure}");
    }
    if let Ok(mut guard) = record.lock() {
        let message = format!("{error:#}");
        let _ = guard.advance(DeploymentState::Failed, Some(&message));
        guard.error = Some(message);
        let _ = guard.save(dir);
    }
    let _ = deploy::clear_stop(dir);
}

pub fn run_topology(bytecode: &Bytecode, config: TopologyRunConfig<'_>) -> Result<RunEnd> {
    let state = verify(bytecode)?;
    let runtime_dir = deploy::dir_for(config.omar_dir, config.ea_id, &state.team);
    fs::create_dir_all(&runtime_dir)?;
    // One live run per team: its sessions are named by team and agent, so a
    // second run would be answered by the first run's panes.
    if let Some(existing) = deploy::DeploymentRecord::load(&runtime_dir)? {
        if existing.is_active() && existing.pid != std::process::id() && existing.runner_alive() {
            bail!(
                "deployment '{}' is {} (pid {}); stop it first",
                state.team,
                existing.state,
                existing.pid
            );
        }
    }
    // A stop left over from an earlier run must not end this one.
    deploy::clear_stop(&runtime_dir)?;
    let obsolete_invocations = runtime_dir.join("invocations");
    if obsolete_invocations.exists() {
        fs::remove_dir_all(&obsolete_invocations)?;
    }
    let client = TmuxClient::new(crate::ea::ea_prefix(config.ea_id, config.base_prefix));
    let planned_sessions: BTreeMap<String, String> = state
        .agents
        .iter()
        .filter(|(_, agent)| !is_web_backend(&agent.backend))
        .map(|(name, _)| (name.clone(), client.session_for(name)))
        .collect();
    let record = Arc::new(Mutex::new(deploy::DeploymentRecord::create(
        &state.team,
        planned_sessions,
        config.timeout.as_secs(),
    )));
    record
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment record lock poisoned"))?
        .save(&runtime_dir)?;
    let diagram_server = config
        .diagram_address
        .map(|address| DiagramServer::start(&state, address))
        .transpose()?;
    if let Some(server) = &diagram_server {
        println!("Diagram server: http://{}", server.address());
        if let Some(sender) = &config.diagram_ready {
            let _ = sender.send(server.address());
        }
    }
    let diagram_publisher = diagram_server.as_ref().map(DiagramServer::publisher);
    let noop_observer = NoopTopologyObserver;
    let observer: &dyn TopologyObserver = diagram_publisher
        .as_ref()
        .map(|publisher| publisher as &dyn TopologyObserver)
        .unwrap_or(&noop_observer);
    observer.run_started();

    // Setup can fail — a backend that never reaches its ready banner, a missing
    // input — and those failures are just as much a failed run as one in the
    // event loop. Report them on the stream too, or an observer sees it go
    // quiet with no reason given.
    advance_record(&record, &runtime_dir, DeploymentState::Deploying, None)?;
    // Only what this run spawned. A failure must not tear down a session it
    // refused to replace.
    let mut spawned: BTreeMap<String, String> = BTreeMap::new();
    let prepared = (|| -> Result<(InvocationServer, BTreeMap<String, Value>)> {
        let invocation_server = InvocationServer::start()?;
        spawn_topology_agents(
            &state,
            &client,
            &runtime_dir,
            &invocation_server,
            &config,
            &mut spawned,
        )?;
        let inputs = parse_inputs(&state, config.inputs)?;
        Ok((invocation_server, inputs))
    })();
    let (invocation_server, inputs) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            observer.run_failed(&error.to_string());
            fail_deployment(&record, &runtime_dir, &client, &spawned, &error);
            return Err(error);
        }
    };
    let web: BTreeSet<String> = state
        .agents
        .iter()
        .filter(|(_, agent)| is_web_backend(&agent.backend))
        .map(|(name, _)| name.clone())
        .collect();
    // Only when the program has one. A run with nothing web-backed gives the
    // daemon no panel to offer, rather than an empty one.
    if !web.is_empty() {
        if let Some(sender) = &config.panel_ready {
            let _ = sender.send(PanelAccess {
                endpoint: invocation_server.endpoint.clone(),
                token: invocation_server.token.clone(),
                agents: web.clone(),
            });
        }
    }
    let executor = AgentReactionExecutor {
        client,
        team: state.team.clone(),
        registry: invocation_server.registry.clone(),
        timeout: config.timeout,
        web,
    };
    advance_record(&record, &runtime_dir, DeploymentState::Running, None)?;
    // Flip RUNNING to STOPPING the moment a stop lands, even while the loop
    // is blocked mid-tag, so an operator polling status sees it acknowledged.
    let watcher_shutdown = Arc::new(AtomicBool::new(false));
    let watcher = {
        let record = record.clone();
        let dir = runtime_dir.clone();
        let shutdown = watcher_shutdown.clone();
        thread::spawn(move || {
            while !shutdown.load(Ordering::Acquire) {
                if deploy::stop_requested(&dir) {
                    if let Ok(mut guard) = record.lock() {
                        if guard.state == DeploymentState::Running {
                            let _ = guard.advance(
                                DeploymentState::Stopping,
                                Some("stop requested; waiting for the current tag to close"),
                            );
                            let _ = guard.save(&dir);
                        }
                    }
                    return;
                }
                thread::sleep(Duration::from_millis(250));
            }
        })
    };
    let outcome = run_event_loop_observed(
        &state,
        inputs,
        &executor,
        observer,
        config.pace,
        Some(&runtime_dir),
    );
    watcher_shutdown.store(true, Ordering::Release);
    let _ = watcher.join();
    let end = match outcome {
        Ok(end) => end,
        Err(error) => {
            observer.run_failed(&error.to_string());
            let sessions = record
                .lock()
                .map(|guard| guard.sessions.clone())
                .unwrap_or_default();
            fail_deployment(&record, &runtime_dir, &executor.client, &sessions, &error);
            return Err(error);
        }
    };
    let (outputs, stopped) = match end {
        LoopEnd::Completed(outputs) => (outputs, false),
        LoopEnd::Stopped(outputs) => (outputs, true),
    };
    observer.run_completed(&outputs);
    write_json_atomic(&runtime_dir.join("state.json"), &state)?;
    write_json_atomic(&deploy::outputs_path(&runtime_dir), &outputs)?;
    let sessions = record
        .lock()
        .map(|guard| guard.sessions.clone())
        .unwrap_or_default();
    for failure in
        deploy::teardown_sessions(&executor.client, &sessions, &deploy::logs_dir(&runtime_dir))
    {
        eprintln!("warning: session not cleaned up: {failure}");
    }
    {
        let mut guard = record
            .lock()
            .map_err(|_| anyhow::anyhow!("deployment record lock poisoned"))?;
        if stopped && guard.state == DeploymentState::Running {
            guard.advance(
                DeploymentState::Stopping,
                Some("stop honoured at a tag boundary"),
            )?;
        }
        let detail = if stopped {
            "stopped by request"
        } else {
            "run completed"
        };
        guard.advance(DeploymentState::Terminated, Some(detail))?;
        guard.save(&runtime_dir)?;
    }
    deploy::clear_stop(&runtime_dir)?;
    if stopped {
        println!("Topology '{}' stopped", state.team);
    } else {
        println!("Topology '{}' completed", state.team);
    }
    for (port, value) in outputs {
        println!("Output {port} = {value}");
    }
    Ok(if stopped {
        RunEnd::Stopped
    } else {
        RunEnd::Completed
    })
}

fn spawn_topology_agents(
    state: &VmState,
    client: &TmuxClient,
    runtime_dir: &Path,
    invocation_server: &InvocationServer,
    config: &TopologyRunConfig<'_>,
    spawned: &mut BTreeMap<String, String>,
) -> Result<()> {
    let protocol = "You are an OMAR topology agent. Only act on OMAR INVOCATION messages. You cannot message other agents. For each invocation, use only omar_set_port to set allowed effects and omar_complete to finish. Port writes are buffered and repeated writes use last-writer-wins semantics.";
    for (name, agent) in &state.agents {
        // A web agent is not spawned. There is no command to resolve, no pane
        // to put it in, and no readiness to wait for — the agent exists as a
        // name in the topology and an inbox in the registry.
        if is_web_backend(&agent.backend) {
            continue;
        }
        let session = client.session_for(name);
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
            // Topology agents answer invocations; they do not chat with the
            // operator, so they get no serve context.
            serve: None,
            topology: Some(TopologyMcpContext {
                team: state.team.clone(),
                agent: name.clone(),
                endpoint: invocation_server.endpoint.clone(),
                token: invocation_server.token.clone(),
            }),
        };
        let command = manager::build_agent_command(&base_command, &prompt_file, &[], &context);
        client.new_session(&session, &command, Some(config.default_workdir))?;
        spawned.insert(name.clone(), session);
    }

    for (name, agent) in &state.agents {
        if is_web_backend(&agent.backend) {
            continue;
        }
        let markers = crate::tmux::backend_readiness_markers(canonical_backend(&agent.backend));
        if !markers.is_empty()
            && !client.wait_for_markers(
                &client.session_for(name),
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

/// Input ports nothing inside the topology writes to.
///
/// Reported by `/v1/programs/check` as a diagnostic, not as something to set: a
/// program that closes its loop has none, and one that has some has a port
/// nothing will ever drive. Naming them while the operator is still editing is
/// cheaper than discovering it from a run that sits still.
pub fn open_inputs(state: &VmState) -> Vec<String> {
    state
        .ports
        .iter()
        .filter(|(name, port)| {
            port.kind == PortKind::Input
                && !state
                    .connections
                    .iter()
                    .any(|connection| connection.target == **name)
        })
        .map(|(name, _)| name.clone())
        .collect()
}

pub fn parse_inputs(state: &VmState, raw_inputs: &[String]) -> Result<BTreeMap<String, Value>> {
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
        // An input fed by a connection gets its value from inside the topology,
        // so the operator has nothing to supply. Supplying one anyway is still
        // allowed: that is how a feedback loop is seeded.
        let connected = state
            .connections
            .iter()
            .any(|connection| connection.target == *name);
        if port.kind == PortKind::Input && !connected && !inputs.contains_key(name) {
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

    fn advance(self, delay: Delay) -> Result<Self> {
        if delay.time > 0 {
            return Ok(Self {
                timestamp: self
                    .timestamp
                    .checked_add(delay.time)
                    .context("logical timestamp overflow")?,
                microstep: 0,
            });
        }
        if delay.microstep {
            return Ok(Self {
                timestamp: self.timestamp,
                microstep: self
                    .microstep
                    .checked_add(1)
                    .context("logical microstep overflow")?,
            });
        }
        Ok(self)
    }
}

/// What one hop costs.
///
/// The distinction Lingua Franca draws, and the reason a fixpoint exists to be
/// reached: a plain connection is instantaneous, so a value written at a tag is
/// readable by the rest of that same tag. `after 0` costs a microstep, which is
/// how a loop closes without letting time pass. Anything larger is real time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Delay {
    /// Nanoseconds of logical time. Zero unless the hop was written with one.
    time: u64,
    /// Whether the hop still costs a microstep when no time passes.
    microstep: bool,
}

impl Delay {
    const INSTANT: Self = Self {
        time: 0,
        microstep: false,
    };
    const MICROSTEP: Self = Self {
        time: 0,
        microstep: true,
    };

    /// What `after n` costs: `after 0` buys a microstep, and anything larger
    /// buys that much logical time.
    fn after(time: u64) -> Self {
        if time == 0 {
            Self::MICROSTEP
        } else {
            Self {
                time,
                microstep: false,
            }
        }
    }

    /// A hop that settles within the tag it started in.
    fn is_instant(self) -> bool {
        self.time == 0 && !self.microstep
    }

    /// Two hops in a row: time adds, and a microstep anywhere costs one.
    fn then(self, other: Self) -> Result<Self> {
        Ok(Self {
            time: self
                .time
                .checked_add(other.time)
                .context("logical delay overflow")?,
            microstep: self.microstep || other.microstep,
        })
    }
}

/// What writing to `port` costs, before any connection is followed.
///
/// An action is the one port that carries a microstep of its own: that is what
/// makes it the way to schedule something later without naming a time, and what
/// keeps a self-loop through one from being a causality loop. Every other port
/// is instantaneous — a reaction's effect is readable at the tag it fired at.
fn port_delay(state: &VmState, port: &str) -> Delay {
    match state.ports.get(port) {
        Some(state) if state.kind == PortKind::Action => match state.delay {
            Some(time) if time > 0 => Delay {
                time,
                microstep: false,
            },
            _ => Delay::MICROSTEP,
        },
        _ => Delay::INSTANT,
    }
}

/// What a connection costs on top of its target port.
fn connection_delay(connection: &ConnectionState) -> Delay {
    match connection.delay {
        None => Delay::INSTANT,
        Some(time) => Delay::after(time),
    }
}

/// Write a value to a port, at this tag or a later one.
///
/// Returns whether this tag learned something, which is what tells the settling
/// loop it has more to do. An instantaneous hop joins the tag in progress; only
/// a hop that costs a microstep or real time goes on the queue.
fn deliver(
    state: &VmState,
    events: &mut BTreeMap<String, Value>,
    queue: &mut BTreeMap<Tag, BTreeMap<String, Value>>,
    current_tag: Tag,
    port: &str,
    value: Value,
    additional_delay: Delay,
) -> Result<bool> {
    if !state.ports.contains_key(port) {
        bail!("unknown scheduled port '{port}'");
    }
    let total = port_delay(state, port).then(additional_delay)?;
    if total.is_instant() {
        events.insert(port.to_string(), value);
        return Ok(true);
    }
    queue
        .entry(current_tag.advance(total)?)
        .or_default()
        .insert(port.to_string(), value);
    Ok(false)
}

/// Carry every connection whose source is present, until none is left to carry.
///
/// A connection fires at most once per tag — `carried` is what remembers — so
/// this terminates after at most one round per connection, and a chain of
/// instantaneous connections settles in the tag it started in.
fn settle_connections(
    state: &VmState,
    events: &mut BTreeMap<String, Value>,
    queue: &mut BTreeMap<Tag, BTreeMap<String, Value>>,
    tag: Tag,
    carried: &mut BTreeSet<usize>,
) -> Result<()> {
    loop {
        let mut learned = false;
        for (index, connection) in state.connections.iter().enumerate() {
            if carried.contains(&index) {
                continue;
            }
            let Some(value) = events.get(&connection.source).cloned() else {
                continue;
            };
            carried.insert(index);
            learned |= deliver(
                state,
                events,
                queue,
                tag,
                &connection.target,
                value,
                connection_delay(connection),
            )?;
        }
        if !learned {
            break;
        }
    }
    Ok(())
}

/// Reactions grouped so everything in a layer may run at once, and everything
/// in a later layer runs after.
///
/// Static: the order follows from the wiring, not from what happens to be
/// present. Walking it once per tag is what makes a reaction fire at most once
/// per tag, and only once every reaction that could decide one of its triggers
/// has had its turn — which is what "all triggers known" means operationally.
fn precedence_layers(state: &VmState) -> Result<Vec<Vec<String>>> {
    let mut indegree: BTreeMap<&str, usize> =
        state.reactions.keys().map(|id| (id.as_str(), 0)).collect();
    let mut outgoing: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for id in state.reactions.keys() {
        let after = must_follow(state, id);
        for later in &after {
            if let Some(count) = indegree.get_mut(later.as_str()) {
                *count += 1;
            }
        }
        outgoing.insert(id.as_str(), after);
    }

    let mut remaining: BTreeSet<&str> = state.reactions.keys().map(String::as_str).collect();
    let mut layers = Vec::new();
    while !remaining.is_empty() {
        let layer: Vec<&str> = remaining
            .iter()
            .copied()
            .filter(|id| indegree[id] == 0)
            .collect();
        if layer.is_empty() {
            bail!("causality loop among reactions: {remaining:?}");
        }
        for id in &layer {
            remaining.remove(id);
            for target in &outgoing[id] {
                if let Some(count) = indegree.get_mut(target.as_str()) {
                    *count -= 1;
                }
            }
        }
        layers.push(layer.into_iter().map(str::to_string).collect());
    }
    Ok(layers)
}

/// One logical tag a program would pass through.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimelineStep {
    pub timestamp: u64,
    pub microstep: u64,
    /// Ports and timers carrying a value at this tag, in name order.
    pub events: Vec<String>,
    /// Reactions whose triggers are present, in the order they would run.
    pub reactions: Vec<String>,
}

/// The tags a program would pass through, worked out without running it.
///
/// The determinism claim made visible: which tags a program reaches and what
/// fires at each are decided by the program, not by what the agents happen to
/// say. So they can be shown before anything is deployed.
///
/// `present` names the ports to treat as carrying a value at the first tag —
/// the inputs a run would be admitted with. Timers are always seeded; they are
/// what starts a program that starts itself. Presence is all that is needed:
/// a reaction fires on a trigger being there, whatever it holds.
///
/// Values are not worked out, only presence — an agent's answer is the one
/// thing here that is not determined. Two consequences worth knowing:
///
/// - a reaction whose contract offers alternatives (`-> (x|y)`) is shown as
///   writing *both*, because which one it picks is the agent's to decide. This
///   over-approximates, and never misses a step;
/// - it cannot know a value that decides a delay, and nothing in the language
///   has one, which is why this works at all.
///
/// Scheduling is not re-implemented here. `settle_connections`, `deliver` and
/// `precedence_layers` are the same ones the event loop walks, so what settles
/// at a tag — and which tag it settles at — cannot drift between what is shown
/// and what runs.
pub fn timeline(
    state: &VmState,
    present: &BTreeSet<String>,
    max_steps: usize,
) -> (Vec<TimelineStep>, bool) {
    let seed: BTreeMap<String, Value> = present
        .iter()
        .filter(|name| state.ports.contains_key(*name))
        .map(|name| (name.clone(), Value::Null))
        .collect();
    let mut queue = BTreeMap::from([(Tag::START, seed)]);
    for (name, timer) in &state.timers {
        queue
            .entry(Tag {
                timestamp: timer.offset,
                microstep: 0,
            })
            .or_default()
            .insert(name.clone(), Value::Null);
    }

    // A program with a causality loop has no order to project; `verify` refuses
    // it before a run, and there is nothing to show for one here either.
    let Ok(layers) = precedence_layers(state) else {
        return (Vec::new(), false);
    };

    let mut steps = Vec::new();
    while let Some((tag, events)) = queue.pop_first() {
        if events.is_empty() {
            continue;
        }
        if steps.len() >= max_steps {
            return (steps, true);
        }

        let mut events = events;
        let mut carried = BTreeSet::new();
        let _ = settle_connections(state, &mut events, &mut queue, tag, &mut carried);

        for (name, timer) in &state.timers {
            if timer.period > 0 && events.contains_key(name) {
                let Some(next) = tag.timestamp.checked_add(timer.period) else {
                    continue;
                };
                queue
                    .entry(Tag {
                        timestamp: next,
                        microstep: 0,
                    })
                    .or_default()
                    .insert(name.clone(), Value::Null);
            }
        }

        // The same walk down the precedence order the event loop makes, with
        // every effect taken rather than the one an agent would have picked.
        let mut fired = Vec::new();
        for layer in &layers {
            let mut enabled: Vec<_> = layer
                .iter()
                .filter_map(|id| state.reactions.get_key_value(id))
                .filter(|(_, reaction)| {
                    reaction
                        .triggers
                        .iter()
                        .any(|trigger| events.contains_key(trigger))
                })
                .collect();
            if enabled.is_empty() {
                continue;
            }
            enabled.sort_by_key(|(_, reaction)| reaction.order);
            for (id, reaction) in &enabled {
                fired.push((*id).clone());
                for effect in &reaction.effects {
                    let _ = deliver(
                        state,
                        &mut events,
                        &mut queue,
                        tag,
                        effect,
                        Value::Null,
                        Delay::INSTANT,
                    );
                }
            }
            let _ = settle_connections(state, &mut events, &mut queue, tag, &mut carried);
        }

        steps.push(TimelineStep {
            timestamp: tag.timestamp,
            microstep: tag.microstep,
            events: events.keys().cloned().collect(),
            reactions: fired,
        });
    }
    (steps, false)
}

/// How a run's logical clock is held against the wall clock.
///
/// Real time by default: a tag at logical timestamp T does not run before T has
/// elapsed since the run began, which is what makes a delay a promise about
/// when rather than only about order. `Fast` runs the same program as quickly
/// as the work allows, which is what a test wants and what `--fast` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pace {
    RealTime,
    Fast,
}

/// How the loop ended: the queue drained, or a stop closed the run at a tag
/// boundary. Either way the outputs read so far come along.
enum LoopEnd {
    Completed(BTreeMap<String, Value>),
    Stopped(BTreeMap<String, Value>),
}

#[cfg(test)]
fn run_event_loop<E: ReactionExecutor>(
    state: &VmState,
    inputs: BTreeMap<String, Value>,
    executor: &E,
) -> Result<BTreeMap<String, Value>> {
    match run_event_loop_observed(
        state,
        inputs,
        executor,
        &NoopTopologyObserver,
        Pace::Fast,
        None,
    )? {
        LoopEnd::Completed(outputs) | LoopEnd::Stopped(outputs) => Ok(outputs),
    }
}

fn run_event_loop_observed<E: ReactionExecutor>(
    state: &VmState,
    inputs: BTreeMap<String, Value>,
    executor: &E,
    observer: &dyn TopologyObserver,
    pace: Pace,
    deployment_dir: Option<&Path>,
) -> Result<LoopEnd> {
    // When the run's logical clock was started, which is what every tag's
    // timestamp is measured from.
    let origin = Instant::now();
    let mut queue = BTreeMap::from([(Tag::START, inputs)]);
    // Arm every timer for its first firing. A timer's value is the timestamp it
    // fired at, which is what makes `$(t)` in a prompt worth reading.
    for (name, timer) in &state.timers {
        queue
            .entry(Tag {
                timestamp: timer.offset,
                microstep: 0,
            })
            .or_default()
            .insert(name.clone(), json!(timer.offset));
    }
    let mut outputs = BTreeMap::new();

    // No bound on how many tags a run may pass through. A loop that costs a
    // microstep somewhere -- through an action, or a connection written
    // `after 0` -- is a walk through superdense time rather than a knot to be
    // untied, and `verify` has already refused the ones that cost nothing.
    // What used to be a runaway guard mostly cut long-lived programs short —
    // and silently reported success when it did.
    //
    // The real limits are the two `checked_add`s in `Tag::advance`, which end a
    // run that exhausts u64 rather than wrapping it into the past.
    // Fixed before the first tag: precedence follows from the wiring, and the
    // wiring does not change while a run is in flight.
    let layers = precedence_layers(state)?;
    let stop_now = || deployment_dir.is_some_and(deploy::stop_requested);

    while let Some((tag, events)) = queue.pop_first() {
        // An operator's stop ends the run here, between tags: nothing is in
        // flight at a boundary, so no invocation's contract is abandoned.
        if stop_now() {
            return Ok(LoopEnd::Stopped(outputs));
        }
        // A tag nothing is present at is not a moment the run passed through.
        // No reaction can fire at one -- enabling asks whether any trigger is
        // in `events`, which is false for every reaction when it is empty --
        // and no connection, output or timer can move either. Announcing it
        // would tell a client the run had advanced when nothing had. The only
        // way to reach one is a program admitted with no inputs, which seeds
        // the start tag with an empty map.
        if events.is_empty() {
            continue;
        }
        // Everything this tag knows. It grows as the tag settles: an
        // instantaneous connection or a reaction's effect joins it rather than
        // landing on a later tag, and the reactions downstream read it before
        // the tag is over.
        let mut events = events;
        let mut carried = BTreeSet::new();
        let due = Duration::from_nanos(tag.timestamp);
        if pace == Pace::RealTime {
            // Microsteps carry no time, so only the timestamp is owed. A tag
            // whose moment has already passed runs now rather than being
            // pushed further out: being late is not a reason to be later.
            // Sliced so a stop during a long wait is honoured within a beat.
            while let Some(remaining) = due.checked_sub(origin.elapsed()) {
                if stop_now() {
                    return Ok(LoopEnd::Stopped(outputs));
                }
                thread::sleep(remaining.min(Duration::from_millis(250)));
            }
        }
        // How far past its logical time this tag actually ran. Zero while the
        // run is on or ahead of schedule, and growing only when the work
        // outlasts the gap it was given -- which is the one number that says
        // whether a program is keeping the promise its delays make.
        let lag = origin.elapsed().saturating_sub(due).as_nanos() as u64;
        // Settle before observing, so what a tag reports includes the values
        // its connections carried into it rather than only what it was woken
        // with.
        settle_connections(state, &mut events, &mut queue, tag, &mut carried)?;
        observer.tag_advanced(tag.timestamp, tag.microstep, lag, &events);
        for (name, timer) in &state.timers {
            if timer.period > 0 && events.contains_key(name) {
                let next = tag
                    .timestamp
                    .checked_add(timer.period)
                    .context("logical timestamp overflow")?;
                queue
                    .entry(Tag {
                        timestamp: next,
                        microstep: 0,
                    })
                    .or_default()
                    .insert(name.clone(), json!(next));
            }
        }
        // One pass down the precedence order. A layer's reactions cannot
        // decide each other's triggers -- that is what put them in one layer --
        // so running them together is safe, and by the time a layer is reached
        // every reaction that could feed it has already had its turn.
        for layer in &layers {
            let mut enabled: Vec<_> = layer
                .iter()
                .filter_map(|id| state.reactions.get_key_value(id))
                .filter(|(_, reaction)| {
                    reaction
                        .triggers
                        .iter()
                        .any(|trigger| events.contains_key(trigger))
                })
                .collect();
            if enabled.is_empty() {
                continue;
            }
            enabled.sort_by_key(|(_, reaction)| reaction.order);

            let specs: Vec<_> = enabled
                .iter()
                .map(|entry| invocation_spec(state, *entry, &events))
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

            // Declaration order decides who wins a port two reactions in this
            // layer both write, which is the rule the language states.
            let mut completed: Vec<_> = enabled
                .iter()
                .zip(invocation_ids)
                .zip(results)
                .map(|(((id, reaction), invocation_id), writes)| {
                    observer.reaction_completed(
                        tag.timestamp,
                        tag.microstep,
                        id,
                        &invocation_id,
                        &writes,
                    );
                    (reaction.order, writes)
                })
                .collect();
            completed.sort_by_key(|(order, _)| *order);
            for (_, writes) in completed {
                for (port, value) in writes {
                    deliver(
                        state,
                        &mut events,
                        &mut queue,
                        tag,
                        &port,
                        value,
                        Delay::INSTANT,
                    )?;
                }
            }
            settle_connections(state, &mut events, &mut queue, tag, &mut carried)?;
        }

        // Read the outputs once the tag has settled, so a value a reaction
        // wrote at this tag counts as this tag's output.
        for (name, value) in &events {
            if state
                .ports
                .get(name)
                .is_some_and(|p| p.kind == PortKind::Output)
            {
                outputs.insert(name.clone(), value.clone());
            }
        }
    }
    Ok(LoopEnd::Completed(outputs))
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
        within: reaction.within.map(Duration::from_nanos),
    })
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

pub(crate) fn valid_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `instance.member`, the name a team instance gives what it declares. Plain
/// identifiers are qualified names with nothing to qualify.
fn valid_qualified_name(value: &str) -> bool {
    !value.is_empty() && value.split('.').all(valid_identifier)
}

/// Members name the instance that declared them.
///
/// Empty is allowed so bytecode written before instances were carried through
/// still loads; naming one that was never declared is a real inconsistency.
fn check_instance(state: &VmState, kind: &str, name: &str, instance: &str) -> Result<()> {
    if !instance.is_empty() && !state.instances.contains_key(instance) {
        bail!("{kind} '{name}' names undeclared instance '{instance}'");
    }
    Ok(())
}

fn require_identifier(kind: &str, value: &str) -> Result<()> {
    if valid_qualified_name(value) {
        Ok(())
    } else {
        bail!("invalid {kind} name '{value}'")
    }
}

/// Whether a backend is answered by a web client rather than a spawned process.
///
/// Nothing is spawned for one, so it has no pane, no readiness marker and no
/// command to resolve — the difference has to be known before any of that is
/// attempted. It names what answers the reaction, not how the bytes get there:
/// the transport is the panel's business, not the program's.
fn is_web_backend(backend: &str) -> bool {
    canonical_backend(backend) == "web"
}

fn canonical_backend(backend: &str) -> &str {
    match backend.to_ascii_lowercase().as_str() {
        "claude" | "claudecode" => "claude",
        "web" => "web",
        "codex" => "codex",
        "opencode" => "opencode",
        "cursor" => "cursor",
        "agy" => "agy",
        "stub" => "stub",
        _ => backend,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The outputs however the loop ended; these tests never request a stop.
    fn loop_outputs(end: LoopEnd) -> BTreeMap<String, Value> {
        match end {
            LoopEnd::Completed(outputs) | LoopEnd::Stopped(outputs) => outputs,
        }
    }

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
            instances: BTreeMap::new(),
            timers: BTreeMap::new(),
            agents: BTreeMap::from([
                (
                    "manager".into(),
                    AgentState {
                        backend: "claude".into(),
                        instance: String::new(),
                    },
                ),
                (
                    "reviewer1".into(),
                    AgentState {
                        backend: "codex".into(),
                        instance: String::new(),
                    },
                ),
                (
                    "reviewer2".into(),
                    AgentState {
                        backend: "opencode".into(),
                        instance: String::new(),
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
                    instance: String::new(),
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
                    instance: String::new(),
                    agent: agent.into(),
                    triggers: triggers.into_iter().map(str::to_string).collect(),
                    effects: effects.into_iter().map(str::to_string).collect(),
                    contract: contract.into(),
                    prompt: "prompt".into(),
                    within: None,
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
            tag.advance(Delay::after(0)).unwrap(),
            Tag {
                timestamp: 10,
                microstep: 8
            }
        );
        // A plain connection costs nothing, so it stays on the tag it started
        // on -- which is what lets a value be read by the rest of that tag.
        assert_eq!(tag.advance(Delay::INSTANT).unwrap(), tag);
        assert_eq!(
            tag.advance(Delay::after(2)).unwrap(),
            Tag {
                timestamp: 12,
                microstep: 0
            }
        );
    }

    /// A ring node: forward the token until it reaches the limit, then stop.
    /// Effects are qualified by instance, so this also proves the runtime
    /// carries qualified names through invocation and contract checking.
    struct RingExecutor {
        limit: i64,
        hops: Mutex<Vec<String>>,
    }

    impl ReactionExecutor for RingExecutor {
        fn invoke(&self, invocation: InvocationSpec) -> Result<BTreeMap<String, Value>> {
            self.hops
                .lock()
                .unwrap()
                .push(invocation.reaction_id.clone());
            let node = invocation
                .reaction_id
                .split('.')
                .next()
                .expect("reaction ids are qualified");
            let token = invocation
                .trigger_values
                .get(&format!("{node}.token"))
                .and_then(Value::as_i64)
                .expect("the token arrives under its qualified name");
            let writes = if token < self.limit {
                BTreeMap::from([(format!("{node}.out"), json!(token + 1))])
            } else {
                BTreeMap::from([(format!("{node}.done"), json!(token))])
            };
            validate_contract(&invocation.contract, &writes)?;
            Ok(writes)
        }
    }

    fn ring_bytecode() -> Bytecode {
        // As `omarc` elaborates tests/topology/src/Ring.omar: one team, three
        // instances, wired head to tail.
        serde_json::from_str(
            r#"{
              "version": 1,
              "team": "Ring",
              "instructions": [
                {"op":"begin_plan","team":"Ring"},
                {"op":"spawn_agent","name":"n1.agent","backend":"Codex"},
                {"op":"spawn_agent","name":"n2.agent","backend":"Codex"},
                {"op":"spawn_agent","name":"n3.agent","backend":"Codex"},
                {"op":"define_port","kind":"input","name":"n1.token","type":"int"},
                {"op":"define_port","kind":"output","name":"n1.out","type":"int"},
                {"op":"define_port","kind":"output","name":"n1.done","type":"int"},
                {"op":"define_port","kind":"input","name":"n2.token","type":"int"},
                {"op":"define_port","kind":"output","name":"n2.out","type":"int"},
                {"op":"define_port","kind":"output","name":"n2.done","type":"int"},
                {"op":"define_port","kind":"input","name":"n3.token","type":"int"},
                {"op":"define_port","kind":"output","name":"n3.out","type":"int"},
                {"op":"define_port","kind":"output","name":"n3.done","type":"int"},
                {"op":"connect_ports","source":"n1.out","target":"n2.token","delay":0},
                {"op":"connect_ports","source":"n2.out","target":"n3.token","delay":0},
                {"op":"connect_ports","source":"n3.out","target":"n1.token","delay":0},
                {"op":"install_reaction","id":"n1.reaction.0","agent":"n1.agent","triggers":["n1.token"],"effects":["n1.out","n1.done"],"contract":"( n1.out | n1.done )","prompt":"ring"},
                {"op":"install_reaction","id":"n2.reaction.0","agent":"n2.agent","triggers":["n2.token"],"effects":["n2.out","n2.done"],"contract":"( n2.out | n2.done )","prompt":"ring"},
                {"op":"install_reaction","id":"n3.reaction.0","agent":"n3.agent","triggers":["n3.token"],"effects":["n3.out","n3.done"],"contract":"( n3.out | n3.done )","prompt":"ring"},
                {"op":"commit_plan"}
              ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn a_ring_of_instances_passes_a_token_until_a_node_stops_forwarding() {
        let state = verify(&ring_bytecode()).unwrap();
        let executor = RingExecutor {
            limit: 6,
            hops: Mutex::new(Vec::new()),
        };

        let outputs = run_event_loop(
            &state,
            BTreeMap::from([("n1.token".into(), json!(0))]),
            &executor,
        )
        .unwrap();

        // Three laps: each hop is a connection with no delay, which lands on
        // the next microstep, so the loop makes progress instead of deadlocking.
        let hops = executor.hops.lock().unwrap().clone();
        assert_eq!(
            hops,
            [
                "n1.reaction.0",
                "n2.reaction.0",
                "n3.reaction.0",
                "n1.reaction.0",
                "n2.reaction.0",
                "n3.reaction.0",
                "n1.reaction.0",
            ]
        );
        assert_eq!(outputs.get("n1.done"), Some(&json!(6)));
        assert_eq!(outputs.get("n2.done"), None);
    }

    #[test]
    fn a_connection_fed_input_needs_no_value_from_the_operator() {
        // n2 and n3 are fed by the ring; only the seed is the operator's to
        // give. Requiring all three would make instance wiring pointless.
        let state = verify(&ring_bytecode()).unwrap();

        let inputs = parse_inputs(&state, &["n1.token=0".to_string()]).unwrap();
        assert_eq!(inputs.get("n1.token"), Some(&json!(0)));
        assert_eq!(inputs.len(), 1);
    }

    #[test]
    fn agents_that_would_share_a_tmux_session_are_rejected() {
        // tmux flattens '.' to '_', so these two would answer for each other.
        let mut bytecode = ring_bytecode();
        bytecode.instructions.insert(
            2,
            Instruction::SpawnAgent {
                name: "n1_agent".to_string(),
                backend: "Codex".to_string(),
                instance: String::new(),
            },
        );

        let error = verify(&bytecode).unwrap_err().to_string();

        assert!(error.contains("same tmux session"), "{error}");
    }

    #[test]
    fn a_nested_instance_names_the_instance_that_declared_it() {
        // A team can instantiate another team, so instances form a tree rather
        // than a list. The VM keeps one flat namespace, so the tree only
        // survives if each instance says who declared it.
        let bytecode: Bytecode = serde_json::from_str(
            r#"{
              "version": 1,
              "team": "Nest",
              "instructions": [
                {"op":"begin_plan","team":"Nest"},
                {"op":"declare_instance","name":"b","team":"B","parent":""},
                {"op":"declare_instance","name":"b.a","team":"A","parent":"b"},
                {"op":"spawn_agent","name":"b.a.agent","backend":"Codex","instance":"b.a"},
                {"op":"define_port","kind":"input","name":"b.a.inp","type":"int","instance":"b.a"},
                {"op":"define_port","kind":"output","name":"b.done","type":"int","instance":"b"},
                {"op":"install_reaction","id":"b.a.reaction.0","agent":"b.a.agent","triggers":["b.a.inp"],"effects":["b.done"],"contract":"b.done","prompt":"x","instance":"b.a"},
                {"op":"commit_plan"}
              ]
            }"#,
        )
        .unwrap();

        let state = verify(&bytecode).unwrap();

        assert_eq!(state.instances["b"].parent, "");
        assert_eq!(state.instances["b.a"].parent, "b");
        assert_eq!(state.instances["b.a"].team, "A");

        // A parent has to exist. Since one is always declared before its
        // children, this also rules out a cycle: neither end could come first.
        let orphan: Bytecode = serde_json::from_str(
            r#"{
              "version": 1,
              "team": "Orphan",
              "instructions": [
                {"op":"begin_plan","team":"Orphan"},
                {"op":"declare_instance","name":"b.a","team":"A","parent":"b"},
                {"op":"commit_plan"}
              ]
            }"#,
        )
        .unwrap();
        let error = verify(&orphan).unwrap_err().to_string();
        assert!(error.contains("undeclared parent 'b'"), "{error}");
    }

    /// One reaction under a contract of the caller's choosing, with or without
    /// a deadline the program set for itself.
    fn deadline_spec(contract: &str, within: Option<u64>) -> InvocationSpec {
        let within = match within {
            Some(nanos) => format!(r#","within":{nanos}"#),
            None => String::new(),
        };
        let bytecode: Bytecode = serde_json::from_str(&format!(
            r#"{{
              "version": 1,
              "team": "Desk",
              "instructions": [
                {{"op":"begin_plan","team":"Desk"}},
                {{"op":"spawn_agent","name":"clerk","backend":"Codex"}},
                {{"op":"define_port","kind":"input","name":"topic","type":"string"}},
                {{"op":"define_port","kind":"output","name":"memo","type":"string"}},
                {{"op":"install_reaction","id":"reaction.0","agent":"clerk",
                 "triggers":["topic"],"effects":["memo"],
                 "contract":"{contract}","prompt":"p"{within}}},
                {{"op":"commit_plan"}}
              ]
            }}"#
        ))
        .unwrap();
        let state = verify(&bytecode).unwrap();
        let (id, reaction) = state.reactions.get_key_value("reaction.0").unwrap();
        invocation_spec(
            &state,
            (id, reaction),
            &BTreeMap::from([("topic".to_string(), json!("x"))]),
        )
        .unwrap()
    }

    #[test]
    fn a_program_sets_its_own_deadline() {
        assert_eq!(
            deadline_spec("memo", Some(30_000_000_000)).within,
            Some(Duration::from_secs(30))
        );
        // Saying nothing leaves the run-wide timeout in charge rather than
        // inventing a deadline the program never asked for.
        assert_eq!(deadline_spec("memo", None).within, None);
    }

    #[test]
    fn silence_completes_the_tag_when_the_contract_permits_it() {
        let spec = deadline_spec("memo ?", Some(1));
        assert_eq!(
            expired(&spec, Duration::from_secs(30)).unwrap(),
            BTreeMap::new()
        );
    }

    #[test]
    fn silence_stops_the_run_when_the_contract_requires_an_effect() {
        let spec = deadline_spec("memo", Some(1));
        let error = expired(&spec, Duration::from_secs(30))
            .unwrap_err()
            .to_string();
        // Names the contract, so an operator reads which promise went unkept
        // rather than only that something timed out, and names the invocation
        // so the line can be found in a log next to the rest of its run.
        assert!(error.contains("requires an effect"), "{error}");
        assert!(error.contains("memo"), "{error}");
        assert!(error.contains("reaction.0"), "{error}");
        assert!(error.contains(&spec.id), "{error}");
    }

    /// Answers nothing, which is what an expired deadline does to a contract
    /// that permits silence.
    struct SilentExecutor;

    impl ReactionExecutor for SilentExecutor {
        fn invoke(&self, _invocation: InvocationSpec) -> Result<BTreeMap<String, Value>> {
            Ok(BTreeMap::new())
        }
    }

    #[test]
    fn a_silent_reaction_leaves_what_it_feeds_unfired() {
        // Silence is absence, not a value: the port it would have written is
        // not present, so the reaction reading it never runs and the run ends
        // rather than stalling on a value that is never coming.
        let bytecode: Bytecode = serde_json::from_str(
            r#"{
              "version": 1,
              "team": "Desk",
              "instructions": [
                {"op":"begin_plan","team":"Desk"},
                {"op":"spawn_agent","name":"clerk","backend":"Codex"},
                {"op":"spawn_agent","name":"editor","backend":"Codex"},
                {"op":"define_port","kind":"input","name":"topic","type":"string"},
                {"op":"define_port","kind":"action","name":"memo","type":"string"},
                {"op":"define_port","kind":"output","name":"report","type":"string"},
                {"op":"install_reaction","id":"reaction.0","agent":"clerk",
                 "triggers":["topic"],"effects":["memo"],
                 "contract":"memo ?","prompt":"p","within":1},
                {"op":"install_reaction","id":"reaction.1","agent":"editor",
                 "triggers":["memo"],"effects":["report"],
                 "contract":"report","prompt":"p"},
                {"op":"commit_plan"}
              ]
            }"#,
        )
        .unwrap();
        let state = verify(&bytecode).unwrap();
        let outputs = run_event_loop(
            &state,
            BTreeMap::from([("topic".to_string(), json!("x"))]),
            &SilentExecutor,
        )
        .unwrap();
        assert!(outputs.is_empty(), "nothing downstream ran: {outputs:?}");
    }

    /// A two-input program with nothing feeding it: exactly the shape that has
    /// to sit still until the operator sends something.
    fn open_input_bytecode() -> Bytecode {
        serde_json::from_str(
            r#"{
              "version": 1,
              "team": "Desk",
              "instructions": [
                {"op":"begin_plan","team":"Desk"},
                {"op":"spawn_agent","name":"clerk","backend":"Codex"},
                {"op":"define_port","kind":"input","name":"topic","type":"string"},
                {"op":"define_port","kind":"input","name":"depth","type":"int"},
                {"op":"define_port","kind":"output","name":"memo","type":"string"},
                {"op":"install_reaction","id":"reaction.0","agent":"clerk",
                 "triggers":["topic","depth"],"effects":["memo"],
                 "contract":"memo","prompt":"p"},
                {"op":"commit_plan"}
              ]
            }"#,
        )
        .unwrap()
    }

    /// Records the tags a run actually passed through, so a projection can be
    /// held against the thing it claims to predict.
    struct RunTags {
        tags: Mutex<Vec<(u64, u64)>>,
    }

    impl TopologyObserver for RunTags {
        fn tag_advanced(
            &self,
            timestamp: u64,
            microstep: u64,
            _lag: u64,
            _ports: &BTreeMap<String, Value>,
        ) {
            self.tags.lock().unwrap().push((timestamp, microstep));
        }
    }

    fn tags_of_a_real_run<E: ReactionExecutor>(
        state: &VmState,
        inputs: BTreeMap<String, Value>,
        executor: &E,
    ) -> Vec<(u64, u64)> {
        let observer = RunTags {
            tags: Mutex::new(Vec::new()),
        };
        // Fast: this compares the tags a run passes through against the
        // projection of them, and waiting out real delays would say nothing
        // more about whether the two agree.
        run_event_loop_observed(state, inputs, executor, &observer, Pace::Fast, None).unwrap();
        let tags = observer.tags.lock().unwrap().clone();
        tags
    }

    fn timeline_tags(state: &VmState, present: &[&str]) -> Vec<(u64, u64)> {
        let present = present.iter().map(|name| name.to_string()).collect();
        let (steps, truncated) = timeline(state, &present, 256);
        assert!(!truncated, "the timeline ran out of room");
        steps
            .iter()
            .map(|step| (step.timestamp, step.microstep))
            .collect()
    }

    #[test]
    fn a_run_nothing_can_start_reaches_no_tag_at_all() {
        // The counterpart to the timeline showing nothing: a tag nothing is
        // present at is not a moment the run passed through, so the run does
        // not announce one. It used to announce (0, 0) — the start tag, seeded
        // whether or not anything was supplied — which told a client the run
        // had advanced when nothing had.
        let state = verify(&open_input_bytecode()).unwrap();
        let observer = RunTags {
            tags: Mutex::new(Vec::new()),
        };
        let outputs = loop_outputs(
            run_event_loop_observed(
                &state,
                BTreeMap::new(),
                &SilentExecutor,
                &observer,
                Pace::Fast,
                None,
            )
            .unwrap(),
        );
        assert!(observer.tags.lock().unwrap().is_empty(), "announced a tag");
        assert!(outputs.is_empty());
    }

    #[test]
    fn a_program_nothing_can_start_shows_nothing() {
        // No timer, and every input dangling: there is nothing to begin it, so
        // the timeline is empty rather than guessing at a first tag.
        let state = verify(&open_input_bytecode()).unwrap();
        let (steps, truncated) = timeline(&state, &BTreeSet::new(), 256);
        assert!(steps.is_empty(), "{steps:?}");
        assert!(!truncated);
    }

    #[test]
    fn the_timeline_hits_the_tags_the_run_does() {
        // The claim it makes: what a program does is decided by the program. If
        // these disagree, one of them is lying to the operator. A timer starts
        // both, which is the only thing that can start either.
        let state = verify(&timer_bytecode(3, 0)).unwrap();
        let executor = TimerExecutor {
            fired: Mutex::new(Vec::new()),
            stop_after: 0,
        };
        let real = tags_of_a_real_run(&state, BTreeMap::new(), &executor);
        assert!(!real.is_empty(), "the run reached no tags");
        assert_eq!(timeline_tags(&state, &[]), real);
    }

    #[test]
    fn a_periodic_projection_stops_rather_than_running_forever() {
        // A periodic timer has no end. A preview has to have one.
        let state = verify(&timer_bytecode(0, 10)).unwrap();
        let (steps, truncated) = timeline(&state, &BTreeSet::new(), 8);
        assert!(truncated);
        assert_eq!(steps.len(), 8);
        assert_eq!(steps[0].timestamp, 0);
    }

    #[test]
    fn a_client_is_a_backend_however_it_is_spelled() {
        assert!(is_web_backend("Web"));
        assert!(is_web_backend("web"));
        assert!(!is_web_backend("ClaudeCode"));
        assert!(!is_web_backend("Stub"));
    }

    #[test]
    fn a_web_agent_is_answered_through_the_registry() {
        // Nothing is spawned for a web agent, so the registration is the whole
        // of the work item: a panel asks what it owes, answers it, and the tag
        // moves on exactly as it would for an agent in a pane.
        let server = InvocationServer::start().unwrap();
        let bytecode: Bytecode = serde_json::from_str(
            r#"{
              "version": 1,
              "team": "Desk",
              "instructions": [
                {"op":"begin_plan","team":"Desk"},
                {"op":"spawn_agent","name":"panel","backend":"Web"},
                {"op":"define_port","kind":"input","name":"topic","type":"string"},
                {"op":"define_port","kind":"output","name":"verdict","type":"string"},
                {"op":"install_reaction","id":"reaction.0","agent":"panel",
                 "triggers":["topic"],"effects":["verdict"],
                 "contract":"verdict","prompt":"Decide on $(topic)"},
                {"op":"commit_plan"}
              ]
            }"#,
        )
        .unwrap();
        let state = verify(&bytecode).unwrap();
        let executor = AgentReactionExecutor {
            client: TmuxClient::new("omar-test"),
            team: state.team.clone(),
            registry: server.registry.clone(),
            timeout: Duration::from_secs(10),
            web: BTreeSet::from(["panel".to_string()]),
        };
        let context = TopologyMcpContext {
            team: state.team.clone(),
            agent: "panel".into(),
            endpoint: server.endpoint.clone(),
            token: server.token.clone(),
        };

        let person = thread::spawn(move || {
            for _ in 0..200 {
                let waiting = pending_invocations(&context).unwrap();
                if let Some(item) = waiting["pending"].as_array().unwrap().first() {
                    let id = item["invocation_id"].as_str().unwrap().to_string();
                    mcp_set_port(
                        &context,
                        json!({"invocation_id": id, "port": "verdict", "value": "ship"}),
                    )
                    .unwrap();
                    mcp_complete(&context, json!({"invocation_id": id})).unwrap();
                    return item.clone();
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!("no invocation was ever offered to the panel");
        });

        let outputs = run_event_loop(
            &state,
            BTreeMap::from([("topic".to_string(), json!("x"))]),
            &executor,
        )
        .unwrap();
        let offered = person.join().unwrap();

        assert_eq!(outputs["verdict"], json!("ship"));
        // The prompt reaches a person already interpolated, so the panel shows
        // the instruction an agent would have read rather than the template.
        assert_eq!(offered["prompt"], json!("Decide on x"));
        // What it may set and what it may see, and nothing else in the run.
        assert_eq!(offered["allowed_effects"], json!({"verdict": "string"}));
        assert_eq!(offered["trigger_values"], json!({"topic": "x"}));
    }

    #[test]
    fn one_agents_pending_work_is_not_anothers() {
        let server = InvocationServer::start().unwrap();
        server
            .registry
            .register(InvocationRecord {
                id: "invocation-3".into(),
                team: "Desk".into(),
                agent: "panel".into(),
                reaction: "reaction.0".into(),
                contract: "verdict".into(),
                allowed_effects: BTreeMap::from([("verdict".into(), "string".into())]),
                trigger_values: BTreeMap::new(),
                prompt: "decide".into(),
                writes: BTreeMap::new(),
                completed: false,
            })
            .unwrap();

        let mine = server
            .registry
            .execute("Desk", "panel", InvocationCommand::Pending)
            .unwrap();
        assert_eq!(mine["pending"].as_array().unwrap().len(), 1);

        // Asking as someone else returns nothing rather than someone else's
        // work: an invocation is addressed to one agent.
        let theirs = server
            .registry
            .execute("Desk", "clerk", InvocationCommand::Pending)
            .unwrap();
        assert!(theirs["pending"].as_array().unwrap().is_empty());
        server.registry.remove("invocation-3");
    }

    /// A connection carrying `delay` nanoseconds, so the run reaches its
    /// output only at that logical timestamp — and under `Pace::RealTime` owes
    /// that much wall-clock time before it gets there.
    fn delayed_bytecode(delay: u64) -> Bytecode {
        serde_json::from_str(&format!(
            r#"{{
              "version": 1,
              "team": "Desk",
              "instructions": [
                {{"op":"begin_plan","team":"Desk"}},
                {{"op":"spawn_agent","name":"clerk","backend":"Codex"}},
                {{"op":"define_port","kind":"input","name":"topic","type":"string"}},
                {{"op":"define_port","kind":"action","name":"staged","type":"string"}},
                {{"op":"define_port","kind":"output","name":"memo","type":"string"}},
                {{"op":"connect_ports","source":"staged","target":"memo","delay":{delay}}},
                {{"op":"install_reaction","id":"reaction.0","agent":"clerk",
                 "triggers":["topic"],"effects":["staged"],
                 "contract":"staged","prompt":"p"}},
                {{"op":"commit_plan"}}
              ]
            }}"#
        ))
        .unwrap()
    }

    /// Writes its one effect and returns at once, so the only time the run
    /// takes is the time the topology asked for.
    struct PromptExecutor;

    impl ReactionExecutor for PromptExecutor {
        fn invoke(&self, invocation: InvocationSpec) -> Result<BTreeMap<String, Value>> {
            let writes = BTreeMap::from([("staged".to_string(), json!("done"))]);
            validate_contract(&invocation.contract, &writes)?;
            Ok(writes)
        }
    }

    /// The delay these tests are built around.
    ///
    /// Large enough that the gap between sleeping and not sleeping dwarfs any
    /// scheduling jitter a loaded CI host adds, which is what keeps a
    /// wall-clock assertion from being a coin flip.
    const TEST_DELAY: Duration = Duration::from_millis(300);

    #[test]
    fn real_time_owes_the_delay_a_program_asked_for() {
        let state = verify(&delayed_bytecode(TEST_DELAY.as_nanos() as u64)).unwrap();
        let started = Instant::now();
        let outputs = loop_outputs(
            run_event_loop_observed(
                &state,
                BTreeMap::from([("topic".to_string(), json!("x"))]),
                &PromptExecutor,
                &NoopTopologyObserver,
                Pace::RealTime,
                None,
            )
            .unwrap(),
        );
        let elapsed = started.elapsed();
        assert_eq!(outputs["memo"], json!("done"));
        assert!(
            elapsed >= TEST_DELAY,
            "a delay is a wait, not only an ordering: {elapsed:?}"
        );
    }

    #[test]
    fn fast_runs_the_same_program_without_the_wait() {
        let state = verify(&delayed_bytecode(TEST_DELAY.as_nanos() as u64)).unwrap();
        let started = Instant::now();
        let outputs = loop_outputs(
            run_event_loop_observed(
                &state,
                BTreeMap::from([("topic".to_string(), json!("x"))]),
                &PromptExecutor,
                &NoopTopologyObserver,
                Pace::Fast,
                None,
            )
            .unwrap(),
        );
        let elapsed = started.elapsed();
        // Same outputs, same tags, no wall clock. Only the waiting is skipped.
        assert_eq!(outputs["memo"], json!("done"));
        // Half the delay: a run that slept would take all of it, and one that
        // did not has nothing to do, so the slack is for the host rather than
        // for the behaviour under test.
        assert!(
            elapsed < TEST_DELAY / 2,
            "fast should not sleep: {elapsed:?}"
        );
    }

    /// Records the lag reported at every tag.
    struct LagRecorder {
        lags: Mutex<Vec<u64>>,
    }

    impl TopologyObserver for LagRecorder {
        fn tag_advanced(
            &self,
            _timestamp: u64,
            _microstep: u64,
            lag: u64,
            _ports: &BTreeMap<String, Value>,
        ) {
            self.lags.lock().unwrap().push(lag);
        }
    }

    #[test]
    fn lag_is_how_far_past_its_logical_time_a_tag_ran() {
        // A reaction that outlasts the gap to the next tag puts the run behind
        // by the difference, and that is the whole of what lag reports.
        const REACTION: Duration = Duration::from_millis(400);
        let state = verify(&delayed_bytecode(TEST_DELAY.as_nanos() as u64)).unwrap();
        struct SlowExecutor;
        impl ReactionExecutor for SlowExecutor {
            fn invoke(&self, invocation: InvocationSpec) -> Result<BTreeMap<String, Value>> {
                thread::sleep(REACTION);
                let writes = BTreeMap::from([("staged".to_string(), json!("done"))]);
                validate_contract(&invocation.contract, &writes)?;
                Ok(writes)
            }
        }
        let observer = LagRecorder {
            lags: Mutex::new(Vec::new()),
        };
        run_event_loop_observed(
            &state,
            BTreeMap::from([("topic".to_string(), json!("x"))]),
            &SlowExecutor,
            &observer,
            Pace::RealTime,
            None,
        )
        .unwrap();

        let lags = observer.lags.lock().unwrap().clone();
        // Tag 0 runs immediately, so nothing is owed yet.
        assert!(
            lags[0] < Duration::from_millis(50).as_nanos() as u64,
            "the first tag is not late: {lags:?}"
        );
        // The tag after the reaction was due at TEST_DELAY and reached at
        // REACTION, so it is behind by the difference.
        let behind = (REACTION - TEST_DELAY).as_nanos() as u64;
        let last = *lags.last().expect("a tag ran");
        assert!(
            last >= behind,
            "the run is at least {behind}ns behind: {lags:?}"
        );
        assert!(
            last < behind + TEST_DELAY.as_nanos() as u64,
            "and not a whole delay more than that: {lags:?}"
        );
    }

    #[test]
    fn fast_reports_a_run_that_is_ahead_of_its_schedule_as_unlagged() {
        // Nothing waits, so every tag is reached before its logical time and
        // the run is never behind. Saturating rather than signed: "ahead by
        // 300ms" is not a number an operator has a use for.
        let state = verify(&delayed_bytecode(TEST_DELAY.as_nanos() as u64)).unwrap();
        let observer = LagRecorder {
            lags: Mutex::new(Vec::new()),
        };
        run_event_loop_observed(
            &state,
            BTreeMap::from([("topic".to_string(), json!("x"))]),
            &PromptExecutor,
            &observer,
            Pace::Fast,
            None,
        )
        .unwrap();
        let lags = observer.lags.lock().unwrap().clone();
        assert_eq!(*lags.last().expect("a tag ran"), 0, "{lags:?}");
    }

    #[test]
    fn a_tag_already_late_is_not_made_later() {
        // The reaction outlasts the gap to the next tag, so that tag is due
        // before it is reached — and must run at once rather than waiting out
        // a schedule it has already missed.
        const REACTION: Duration = Duration::from_millis(400);
        let state = verify(&delayed_bytecode(TEST_DELAY.as_nanos() as u64)).unwrap();
        struct SlowExecutor;
        impl ReactionExecutor for SlowExecutor {
            fn invoke(&self, invocation: InvocationSpec) -> Result<BTreeMap<String, Value>> {
                thread::sleep(REACTION);
                let writes = BTreeMap::from([("staged".to_string(), json!("done"))]);
                validate_contract(&invocation.contract, &writes)?;
                Ok(writes)
            }
        }
        let started = Instant::now();
        run_event_loop_observed(
            &state,
            BTreeMap::from([("topic".to_string(), json!("x"))]),
            &SlowExecutor,
            &NoopTopologyObserver,
            Pace::RealTime,
            None,
        )
        .unwrap();
        let elapsed = started.elapsed();
        // Correct is the reaction alone; pushing the missed tag out would add
        // the whole delay on top. The bound sits halfway between the two, so
        // neither a slow host nor the bug can be mistaken for the other.
        assert!(
            elapsed < REACTION + TEST_DELAY / 2,
            "a missed tag should not be pushed further out: {elapsed:?}"
        );
    }

    /// Records the tag every invocation arrived at, so a timer's schedule can
    /// be read off the run rather than inferred from its outputs.
    struct TimerExecutor {
        fired: Mutex<Vec<u64>>,
        /// Firings after which this executor stops the run.
        ///
        /// A periodic timer has no end of its own, so something has to say
        /// when. It used to be the tag cap, which stopped every run at 1024
        /// whether or not that was the program's intent.
        stop_after: usize,
    }

    impl ReactionExecutor for TimerExecutor {
        fn invoke(&self, invocation: InvocationSpec) -> Result<BTreeMap<String, Value>> {
            let at = invocation
                .trigger_values
                .values()
                .next()
                .and_then(Value::as_u64)
                .expect("a timer carries the time it fired at");
            let count = {
                let mut fired = self.fired.lock().unwrap();
                fired.push(at);
                fired.len()
            };
            if self.stop_after > 0 && count >= self.stop_after {
                bail!("enough");
            }
            let writes = BTreeMap::from([("note".into(), json!("tick"))]);
            validate_contract(&invocation.contract, &writes)?;
            Ok(writes)
        }
    }

    fn timer_bytecode(offset: u64, period: u64) -> Bytecode {
        serde_json::from_str(&format!(
            r#"{{
              "version": 1,
              "team": "Poller",
              "instructions": [
                {{"op":"begin_plan","team":"Poller"}},
                {{"op":"spawn_agent","name":"agent","backend":"Codex"}},
                {{"op":"define_port","kind":"output","name":"note","type":"string"}},
                {{"op":"declare_timer","name":"t","offset":{offset},"period":{period}}},
                {{"op":"install_reaction","id":"reaction.0","agent":"agent","triggers":["t"],"effects":["note"],"contract":"note","prompt":"tick at $(t)"}},
                {{"op":"commit_plan"}}
              ]
            }}"#
        ))
        .unwrap()
    }

    #[test]
    fn exhausting_a_tag_field_ends_the_run_rather_than_wrapping_it() {
        // With no cap on tags, these two are the only limits left. Wrapping
        // either would put an event in the run's own past, where the queue
        // would hand it back before things that already happened.
        let last_microstep = Tag {
            timestamp: 7,
            microstep: u64::MAX,
        };
        let error = last_microstep
            .advance(Delay::after(0))
            .unwrap_err()
            .to_string();
        assert!(error.contains("microstep overflow"), "{error}");

        let last_timestamp = Tag {
            timestamp: u64::MAX,
            microstep: 0,
        };
        let error = last_timestamp
            .advance(Delay::after(1))
            .unwrap_err()
            .to_string();
        assert!(error.contains("timestamp overflow"), "{error}");

        // And one short of each still advances, so the guard is at the edge
        // rather than a step inside it.
        assert_eq!(
            Tag {
                timestamp: 7,
                microstep: u64::MAX - 1,
            }
            .advance(Delay::after(0))
            .unwrap(),
            Tag {
                timestamp: 7,
                microstep: u64::MAX,
            }
        );
    }

    /// Every program in the corpus still verifies.
    ///
    /// Rejecting a causality loop is a rejection the compiler cannot make on
    /// its own — it needs the whole elaborated topology — so without this a
    /// program that stops verifying is only found when someone deploys it.
    /// Skipped where `omarc` has not been built, which is why CI builds it.
    #[test]
    fn the_topology_corpus_verifies() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let omarc = root.join("lang/.lake/build/bin/omarc");
        if !omarc.exists() {
            eprintln!("skipping: {} has not been built", omarc.display());
            return;
        }

        let mut checked = 0;
        let mut sources: Vec<_> = std::fs::read_dir(root.join("tests/topology/src"))
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|kind| kind == "omar"))
            .collect();
        sources.sort();

        for source in sources {
            let bytecode_path = std::env::temp_dir().join(format!(
                "omar-verify-{}.json",
                source.file_stem().unwrap().to_string_lossy()
            ));
            let compiled = std::process::Command::new(&omarc)
                .arg(&source)
                .arg(&bytecode_path)
                .output()
                .unwrap();
            assert!(
                compiled.status.success(),
                "{} did not compile: {}",
                source.display(),
                String::from_utf8_lossy(&compiled.stderr)
            );

            let bytecode: Bytecode =
                serde_json::from_str(&std::fs::read_to_string(&bytecode_path).unwrap()).unwrap();
            verify(&bytecode)
                .unwrap_or_else(|error| panic!("{} does not verify: {error}", source.display()));
            let _ = std::fs::remove_file(&bytecode_path);
            checked += 1;
        }
        assert!(checked > 0, "the corpus is empty");
    }

    /// A parent triggered by two children at different depths fires once.
    ///
    /// This is the whole of the fixpoint rule in one program. `deep` is three
    /// instantaneous hops from `start` and `shallow` is one, so under a
    /// semantics where a hop costs a microstep the watcher would be woken by
    /// `shallow`, run with `deep` absent, and then be woken again — reading a
    /// half-built picture the first time and reporting twice. Because no hop
    /// costs anything, both land at one tag and the watcher runs after both.
    #[test]
    fn a_parent_reading_two_depths_fires_once_with_both_in_hand() {
        let bytecode: Bytecode = serde_json::from_str(
            r#"{
              "version": 1,
              "team": "Depths",
              "instructions": [
                {"op":"begin_plan","team":"Depths"},
                {"op":"spawn_agent","name":"near","backend":"Stub"},
                {"op":"spawn_agent","name":"far","backend":"Stub"},
                {"op":"spawn_agent","name":"deeper","backend":"Stub"},
                {"op":"spawn_agent","name":"watcher","backend":"Stub"},
                {"op":"define_port","kind":"input","name":"start","type":"int"},
                {"op":"define_port","kind":"output","name":"shallow","type":"int"},
                {"op":"define_port","kind":"input","name":"shallow_in","type":"int"},
                {"op":"define_port","kind":"output","name":"middle","type":"int"},
                {"op":"define_port","kind":"input","name":"middle_in","type":"int"},
                {"op":"define_port","kind":"output","name":"deep","type":"int"},
                {"op":"define_port","kind":"input","name":"deep_in","type":"int"},
                {"op":"define_port","kind":"output","name":"verdict","type":"int"},
                {"op":"connect_ports","source":"shallow","target":"shallow_in"},
                {"op":"connect_ports","source":"middle","target":"middle_in"},
                {"op":"connect_ports","source":"deep","target":"deep_in"},
                {"op":"install_reaction","id":"reaction.0","agent":"near",
                 "triggers":["start"],"effects":["shallow"],
                 "contract":"shallow","prompt":"p"},
                {"op":"install_reaction","id":"reaction.1","agent":"far",
                 "triggers":["shallow_in"],"effects":["middle"],
                 "contract":"middle","prompt":"p"},
                {"op":"install_reaction","id":"reaction.2","agent":"deeper",
                 "triggers":["middle_in"],"effects":["deep"],
                 "contract":"deep","prompt":"p"},
                {"op":"install_reaction","id":"reaction.3","agent":"watcher",
                 "triggers":["shallow_in","deep_in"],"effects":["verdict"],
                 "contract":"verdict","prompt":"p"},
                {"op":"commit_plan"}
              ]
            }"#,
        )
        .unwrap();
        let state = verify(&bytecode).unwrap();

        /// Records what the watcher was holding each time it ran.
        #[derive(Default)]
        struct Watch {
            saw: Mutex<Vec<BTreeMap<String, Value>>>,
        }
        impl ReactionExecutor for Watch {
            fn invoke(&self, invocation: InvocationSpec) -> Result<BTreeMap<String, Value>> {
                if invocation.reaction_id == "reaction.3" {
                    self.saw
                        .lock()
                        .unwrap()
                        .push(invocation.trigger_values.clone());
                }
                Ok(invocation
                    .allowed_effects
                    .keys()
                    .map(|port| (port.clone(), json!(1)))
                    .collect())
            }
        }

        let executor = Watch::default();
        run_event_loop(
            &state,
            BTreeMap::from([("start".to_string(), json!(1))]),
            &executor,
        )
        .unwrap();

        let saw = executor.saw.lock().unwrap();
        assert_eq!(saw.len(), 1, "the watcher ran more than once: {saw:?}");
        assert_eq!(
            saw[0].keys().collect::<Vec<_>>(),
            vec!["deep_in", "shallow_in"],
            "the watcher ran without both of its triggers"
        );
    }

    /// The two kinds of connection, told apart by the tag they land on.
    #[test]
    fn a_plain_connection_is_instant_and_after_zero_costs_a_microstep() {
        let program = |delay: &str| {
            format!(
                r#"{{
                  "version": 1,
                  "team": "Hop",
                  "instructions": [
                    {{"op":"begin_plan","team":"Hop"}},
                    {{"op":"spawn_agent","name":"downstream","backend":"Stub"}},
                    {{"op":"define_port","kind":"input","name":"source","type":"int"}},
                    {{"op":"define_port","kind":"input","name":"target","type":"int"}},
                    {{"op":"define_port","kind":"output","name":"done","type":"int"}},
                    {{"op":"connect_ports","source":"source","target":"target"{delay}}},
                    {{"op":"install_reaction","id":"reaction.0","agent":"downstream",
                     "triggers":["target"],"effects":["done"],
                     "contract":"done","prompt":"p"}},
                    {{"op":"commit_plan"}}
                  ]
                }}"#
            )
        };

        /// Reports the tag the downstream reaction was invoked at.
        #[derive(Default)]
        struct At {
            tag: Mutex<Option<(u64, u64)>>,
        }
        impl TopologyObserver for At {
            fn reaction_started(&self, timestamp: u64, microstep: u64, _: &str, _: &str) {
                *self.tag.lock().unwrap() = Some((timestamp, microstep));
            }
        }
        struct Yes;
        impl ReactionExecutor for Yes {
            fn invoke(&self, invocation: InvocationSpec) -> Result<BTreeMap<String, Value>> {
                Ok(invocation
                    .allowed_effects
                    .keys()
                    .map(|port| (port.clone(), json!(1)))
                    .collect())
            }
        }

        let ran_at = |delay: &str| {
            let bytecode: Bytecode = serde_json::from_str(&program(delay)).unwrap();
            let state = verify(&bytecode).unwrap();
            let observer = At::default();
            run_event_loop_observed(
                &state,
                BTreeMap::from([("source".to_string(), json!(1))]),
                &Yes,
                &observer,
                Pace::Fast,
                None,
            )
            .unwrap();
            let tag = *observer.tag.lock().unwrap();
            tag.expect("the downstream reaction never ran")
        };

        // No `after`: the value is readable at the tag it was written.
        assert_eq!(ran_at(""), (0, 0));
        // `after 0`: one microstep later, no time passed.
        assert_eq!(ran_at(r#","delay":0"#), (0, 1));
        // `after n`: n nanoseconds later, back at microstep zero.
        assert_eq!(ran_at(r#","delay":5"#), (5, 0));
    }

    /// A cycle that costs nothing to go round is refused before it runs.
    #[test]
    fn a_loop_of_instant_hops_is_refused() {
        let program = |delay: &str| {
            format!(
                r#"{{
                  "version": 1,
                  "team": "Knot",
                  "instructions": [
                    {{"op":"begin_plan","team":"Knot"}},
                    {{"op":"spawn_agent","name":"left","backend":"Stub"}},
                    {{"op":"spawn_agent","name":"right","backend":"Stub"}},
                    {{"op":"define_port","kind":"output","name":"a","type":"int"}},
                    {{"op":"define_port","kind":"input","name":"a_in","type":"int"}},
                    {{"op":"define_port","kind":"output","name":"b","type":"int"}},
                    {{"op":"define_port","kind":"input","name":"b_in","type":"int"}},
                    {{"op":"connect_ports","source":"a","target":"a_in"}},
                    {{"op":"connect_ports","source":"b","target":"b_in"{delay}}},
                    {{"op":"install_reaction","id":"reaction.0","agent":"left",
                     "triggers":["a_in"],"effects":["b"],"contract":"b","prompt":"p"}},
                    {{"op":"install_reaction","id":"reaction.1","agent":"right",
                     "triggers":["b_in"],"effects":["a"],"contract":"a","prompt":"p"}},
                    {{"op":"commit_plan"}}
                  ]
                }}"#
            )
        };

        let verified = |delay: &str| {
            let bytecode: Bytecode = serde_json::from_str(&program(delay)).unwrap();
            verify(&bytecode)
        };

        // Every hop instantaneous: neither reaction can go first.
        let error = verified("").unwrap_err().to_string();
        assert!(error.contains("causality loop"), "{error}");

        // One `after 0` anywhere in the cycle turns it into a walk through
        // superdense time, which is a thing a run can do.
        verified(r#","delay":0"#).expect("a microstep breaks the loop");
        // As does real time.
        verified(r#","delay":5"#).expect("a delay breaks the loop");
    }

    #[test]
    fn a_zero_delay_loop_runs_past_where_the_tag_cap_used_to_be() {
        // A graph cycle is not a causality loop when it costs a microstep to go
        // round -- here through an action -- so this is a walk through
        // superdense time. It used to be cut off at 1024 tags with an error;
        // now it ends when the work says so.
        let bytecode: Bytecode = serde_json::from_str(
            r#"{
              "version": 1,
              "team": "Loop",
              "instructions": [
                {"op":"begin_plan","team":"Loop"},
                {"op":"spawn_agent","name":"counter","backend":"Codex"},
                {"op":"define_port","kind":"input","name":"start","type":"int"},
                {"op":"define_port","kind":"action","name":"next","type":"int"},
                {"op":"define_port","kind":"output","name":"result","type":"int"},
                {"op":"install_reaction","id":"reaction.0","agent":"counter",
                 "triggers":["start","next"],"effects":["next","result"],
                 "contract":"( next | result )","prompt":"p"},
                {"op":"commit_plan"}
              ]
            }"#,
        )
        .unwrap();
        let state = verify(&bytecode).unwrap();

        /// Feeds itself until it has gone well past the old bound, then exits
        /// through the other alternative of its contract.
        struct Spinner {
            seen: Mutex<u64>,
        }
        impl ReactionExecutor for Spinner {
            fn invoke(&self, invocation: InvocationSpec) -> Result<BTreeMap<String, Value>> {
                let mut seen = self.seen.lock().unwrap();
                *seen += 1;
                let writes = if *seen >= 4096 {
                    BTreeMap::from([("result".to_string(), json!(*seen))])
                } else {
                    BTreeMap::from([("next".to_string(), json!(*seen))])
                };
                validate_contract(&invocation.contract, &writes)?;
                Ok(writes)
            }
        }

        let executor = Spinner {
            seen: Mutex::new(0),
        };
        let outputs = run_event_loop(
            &state,
            BTreeMap::from([("start".to_string(), json!(0))]),
            &executor,
        )
        .unwrap();
        assert_eq!(outputs["result"], json!(4096));
    }

    #[test]
    fn a_timer_with_no_period_fires_once_at_its_offset() {
        let state = verify(&timer_bytecode(3, 0)).unwrap();
        let executor = TimerExecutor {
            fired: Mutex::new(Vec::new()),
            // A one-shot ends on its own; nothing has to stop it.
            stop_after: 0,
        };

        let outputs = run_event_loop(&state, BTreeMap::new(), &executor).unwrap();

        // Once, at the offset — not at tag 0, and not again afterwards.
        assert_eq!(*executor.fired.lock().unwrap(), vec![3]);
        assert_eq!(outputs, BTreeMap::from([("note".into(), json!("tick"))]));
    }

    #[test]
    fn a_periodic_timer_re_arms_itself_at_every_period() {
        let state = verify(&timer_bytecode(0, 10)).unwrap();
        let executor = TimerExecutor {
            fired: Mutex::new(Vec::new()),
            // Well past the 1024 tags that used to end every run, so this also
            // says the bound is gone rather than merely raised.
            stop_after: 1500,
        };

        // A periodic timer has no end of its own. Nothing in the runtime
        // invents one, so it runs until the work does something about it.
        let error = run_event_loop(&state, BTreeMap::new(), &executor).unwrap_err();
        assert!(error.to_string().contains("enough"), "{error}");

        let fired = executor.fired.lock().unwrap().clone();
        assert_eq!(&fired[..4], &[0, 10, 20, 30], "fired at {fired:?}");
        assert_eq!(fired.len(), 1500, "kept its period the whole way");
        // Every firing is one period on from the last, all the way out.
        assert_eq!(*fired.last().unwrap(), 10 * 1499);
    }

    #[test]
    fn a_timer_cannot_share_a_name_with_a_port_or_be_written_to() {
        let shadowing: Bytecode = serde_json::from_str(
            r#"{
              "version": 1,
              "team": "Clash",
              "instructions": [
                {"op":"begin_plan","team":"Clash"},
                {"op":"define_port","kind":"input","name":"t","type":"int"},
                {"op":"declare_timer","name":"t","offset":1,"period":0},
                {"op":"commit_plan"}
              ]
            }"#,
        )
        .unwrap();
        let error = verify(&shadowing).unwrap_err().to_string();
        assert!(error.contains("a trigger has one name"), "{error}");

        let written: Bytecode = serde_json::from_str(
            r#"{
              "version": 1,
              "team": "Written",
              "instructions": [
                {"op":"begin_plan","team":"Written"},
                {"op":"spawn_agent","name":"agent","backend":"Codex"},
                {"op":"define_port","kind":"input","name":"go","type":"int"},
                {"op":"declare_timer","name":"t","offset":1,"period":0},
                {"op":"install_reaction","id":"reaction.0","agent":"agent","triggers":["go"],"effects":["t"],"contract":"t","prompt":"x"},
                {"op":"commit_plan"}
              ]
            }"#,
        )
        .unwrap();
        let error = verify(&written).unwrap_err().to_string();
        assert!(error.contains("cannot write to timer"), "{error}");
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

        // Delays are where a re-implementation would drift first: a fixed port
        // delay and a connection delay both move a tag, and the projection has
        // to move it the same way. It shares `enqueue_event` with the loop so
        // that it cannot, and this is what says so.
        let real = tags_of_a_real_run(
            &state,
            BTreeMap::from([("start".into(), json!(7))]),
            &SuperdenseExecutor {
                calls: Mutex::new(Vec::new()),
            },
        );
        assert_eq!(timeline_tags(&state, &["start"]), real);
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
    fn precedence_orders_overlapping_effects_and_same_agent() {
        let mut state = hr_state();
        state.reactions.get_mut("reaction.2").unwrap().effects = vec!["opinion1".into()];
        state.reactions.get_mut("reaction.2").unwrap().agent = "reviewer1".into();
        assert!(must_follow(&state, "reaction.1").contains("reaction.2"));

        let layers = precedence_layers(&state).unwrap();
        let layer_of = |id: &str| {
            layers
                .iter()
                .position(|layer| layer.iter().any(|entry| entry == id))
                .unwrap()
        };
        assert!(layer_of("reaction.1") < layer_of("reaction.2"));
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
            trigger_values: BTreeMap::new(),
            prompt: "p".into(),
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
            trigger_values: BTreeMap::new(),
            prompt: "p".into(),
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

    /// Answers its invocation, and files a stop request while doing so.
    struct StopWhileAnsweringExecutor {
        dir: std::path::PathBuf,
        calls: Mutex<usize>,
    }

    impl ReactionExecutor for StopWhileAnsweringExecutor {
        fn invoke(&self, invocation: InvocationSpec) -> Result<BTreeMap<String, Value>> {
            *self.calls.lock().unwrap() += 1;
            crate::deploy::request_stop(&self.dir).unwrap();
            let port = invocation.allowed_effects.keys().next().unwrap().clone();
            Ok(BTreeMap::from([(port, json!("ping"))]))
        }
    }

    /// `a` fires `reaction.0` at one tag; its action effect schedules
    /// `reaction.1` a microstep later, so the run crosses a tag boundary.
    fn two_tag_bytecode() -> Bytecode {
        serde_json::from_str(
            r#"{
              "version": 1,
              "team": "Boundary",
              "instructions": [
                {"op":"begin_plan","team":"Boundary"},
                {"op":"spawn_agent","name":"worker","backend":"stub"},
                {"op":"define_port","kind":"input","name":"a","type":"string"},
                {"op":"define_port","kind":"action","name":"x","type":"string"},
                {"op":"define_port","kind":"output","name":"out","type":"string"},
                {"op":"install_reaction","id":"reaction.0","agent":"worker","triggers":["a"],"effects":["x"],"contract":"x","prompt":"First"},
                {"op":"install_reaction","id":"reaction.1","agent":"worker","triggers":["x"],"effects":["out"],"contract":"out","prompt":"Second"},
                {"op":"commit_plan"}
              ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn a_stop_request_ends_the_run_at_the_next_tag_boundary() {
        let state = verify(&two_tag_bytecode()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let executor = StopWhileAnsweringExecutor {
            dir: dir.path().to_path_buf(),
            calls: Mutex::new(0),
        };
        let inputs = BTreeMap::from([("a".to_string(), json!("go"))]);
        let end = run_event_loop_observed(
            &state,
            inputs.clone(),
            &executor,
            &NoopTopologyObserver,
            Pace::Fast,
            Some(dir.path()),
        )
        .unwrap();
        assert!(matches!(end, LoopEnd::Stopped(_)));
        assert_eq!(*executor.calls.lock().unwrap(), 1);

        // The same program drains both tags when nothing watches for a stop.
        let unwatched = tempfile::tempdir().unwrap();
        let executor = StopWhileAnsweringExecutor {
            dir: unwatched.path().to_path_buf(),
            calls: Mutex::new(0),
        };
        let end = run_event_loop_observed(
            &state,
            inputs,
            &executor,
            &NoopTopologyObserver,
            Pace::Fast,
            None,
        )
        .unwrap();
        match end {
            LoopEnd::Completed(outputs) => assert_eq!(outputs.get("out"), Some(&json!("ping"))),
            LoopEnd::Stopped(_) => panic!("nothing requested a stop"),
        }
        assert_eq!(*executor.calls.lock().unwrap(), 2);
    }
}
