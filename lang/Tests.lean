import Omar.Compiler

open Lean
open Omar

def assertEqual [BEq α] [ToString α] (label : String) (actual expected : α) : IO Unit :=
  if actual == expected then pure ()
  else throw (IO.userError s!"{label}: expected {expected}, got {actual}")

/-- Lean core has no substring test on String; splitting is the cheap one. -/
def mentions (haystack needle : String) : Bool := (haystack.splitOn needle).length > 1

structure TopologyCase where
  file : String
  team : String
  agents : Nat
  ports : Nat
  connections : Nat := 0
  reactions : Nat
  instructions : Nat
  codexOnly : Bool := true

def topologyCases : Array TopologyCase := #[
  { file := "AllTypes.omar", team := "AllTypes", agents := 1, ports := 18,
    reactions := 1, instructions := 23 },
  { file := "BusinessLoop.omar", team := "BusinessLoop", agents := 8,
    ports := 20, reactions := 11, instructions := 42 },
  -- `main WithinTest` names the program, so the team is not the file stem.
  { file := "DeadlineProbe.omar", team := "WithinTest", agents := 3, ports := 3,
    reactions := 3, instructions := 13 },
  { file := "EffectContracts.omar", team := "EffectContracts", agents := 1, ports := 6,
    reactions := 3, instructions := 13 },
  { file := "FanOutFanIn.omar", team := "FanOutFanIn", agents := 4, ports := 5,
    reactions := 4, instructions := 16 },
  { file := "HR.omar", team := "HR", agents := 3, ports := 6,
    reactions := 4, instructions := 16, codexOnly := false },
  { file := "HRCodex.omar", team := "HRCodex", agents := 3, ports := 6,
    reactions := 4, instructions := 16 },
  { file := "OrTriggers.omar", team := "OrTriggers", agents := 3, ports := 5,
    reactions := 3, instructions := 14 },
  { file := "OrderedWrites.omar", team := "OrderedWrites", agents := 4, ports := 3,
    reactions := 4, instructions := 14 },
  { file := "Recurrence.omar", team := "Recurrence", agents := 1, ports := 3,
    reactions := 1, instructions := 8 },
  -- Three instances of one team, so every count is the team's times three.
  { file := "Ring.omar", team := "Ring", agents := 3, ports := 9,
    connections := 3, reactions := 3, instructions := 23 },
  { file := "SameAgentSerial.omar", team := "SameAgentSerial", agents := 2, ports := 4,
    reactions := 3, instructions := 12 },
  { file := "SuperdenseTime.omar", team := "SuperdenseTime", agents := 3, ports := 6,
    connections := 1, reactions := 3, instructions := 16 }
]

def testTopology (test : TopologyCase) : IO Unit := do
  let path := s!"../tests/topology/src/{test.file}"
  let source ← IO.FS.readFile path
  let programName := (System.FilePath.mk test.file).fileStem.getD "main"
  let program ← match lex source >>= parse programName with
    | .ok program => pure program
    | .error message => throw (IO.userError s!"{test.file}: {message}")
  assertEqual s!"{test.file} team" program.team test.team
  assertEqual s!"{test.file} agent count" program.agents.size test.agents
  assertEqual s!"{test.file} port count" program.ports.size test.ports
  assertEqual s!"{test.file} connection count" program.connections.size test.connections
  assertEqual s!"{test.file} reaction count" program.reactions.size test.reactions
  if test.codexOnly then
    for agent in program.agents do
      assertEqual s!"{test.file} backend for {agent.name}" agent.backend "Codex"
  let bytecode ← match compileSource programName source with
    | .ok bytecode => pure bytecode
    | .error message => throw (IO.userError s!"{test.file}: {message}")
  let secondCompile ← match compileSource programName source with
    | .ok bytecode => pure bytecode
    | .error message => throw (IO.userError s!"{test.file}: {message}")
  assertEqual s!"{test.file} deterministic bytecode" bytecode secondCompile
  match Json.parse bytecode >>= (·.getObjVal? "instructions") with
  | .ok (.arr instructions) =>
      assertEqual s!"{test.file} instruction count" instructions.size test.instructions
  | _ => throw (IO.userError s!"{test.file}: compiler did not emit an instruction array")
  if test.file == "AllTypes.omar" then
    assertEqual "list type" (program.ports.any (·.type == "list<int>")) true
    assertEqual "option type" (program.ports.any (·.type == "option<string>")) true
    assertEqual "nested type" (program.ports.any (·.type == "list<option<int>>")) true
  if test.file == "Ring.omar" then
    -- Instances are structure, not a naming convention: the bytecode says which
    -- container each name belongs to rather than leaving it to be guessed from
    -- the dot.
    assertEqual "one instance per instantiation" program.instances.size 3
    assertEqual "instances name their team"
      (program.instances.all (·.team == "Node")) true
    assertEqual "every port belongs to an instance"
      (program.ports.all (fun port => port.instance_ != "")) true
    assertEqual "members are grouped by instance"
      (program.ports.filter (·.instance_ == "n2") |>.size) 3
    -- Instantiation is a renaming: nothing the team declared keeps its bare
    -- name, and each instance's agent is its own.
    assertEqual "instance-qualified agents"
      (program.agents.map (·.name)) #["n1.agent", "n2.agent", "n3.agent"]
    assertEqual "instance-qualified ports"
      (program.ports.any (·.name == "n2.token")) true
    assertEqual "no unqualified port survives"
      (program.ports.any (·.name == "token")) false
    -- The runtime matches these against qualified writes and trigger values,
    -- so a contract or prompt left bare would fail at run time, not compile.
    assertEqual "contract is qualified"
      (program.reactions.any (·.contract == "( n2.out | n2.done )")) true
    assertEqual "prompt port reference is qualified"
      (program.reactions.all (fun reaction => !mentions reaction.prompt "$(token)")) true
    -- Each instance gets its own argument baked into its prompt.
    for index in [1, 2, 3] do
      assertEqual s!"parameter substituted for n{index}"
        (program.reactions.any (fun reaction => mentions reaction.prompt s!"node {index} of")) true
    assertEqual "no parameter reference survives"
      (program.reactions.all (fun reaction => !mentions reaction.prompt "$(idx)")) true
    assertEqual "ring is wired head to tail"
      (program.connections.map fun connection => s!"{connection.source}->{connection.target}")
      #["n1.out->n2.token", "n2.out->n3.token", "n3.out->n1.token"]
  if test.file == "DeadlineProbe.omar" then
    -- Nanoseconds, and only on the reactions that asked for one.
    assertEqual "generous deadline"
      (program.reactions.any (·.within == some 600000000000)) true
    assertEqual "1ms deadline"
      (program.reactions.any (·.within == some 1000000)) true
    assertEqual "no deadline on the reporter"
      (program.reactions.any (·.within == none)) true
    -- The optional contract is what turns expiry into a completed tag rather
    -- than a failed run, so it has to survive instantiation.
    assertEqual "expiry-absorbing contract"
      (program.reactions.any (·.contract == "probe.missed ?")) true
  if test.file == "SuperdenseTime.omar" then
    -- Names are instance-qualified now that every program instantiates.
    assertEqual "fixed action delay"
      (program.ports.any fun port => port.name == "time.fixed" && port.delay == some 2) true
    assertEqual "connected action delay"
      (program.ports.any fun port => port.name == "time.connected" && port.delay == some 1) true
    assertEqual "connection delay"
      (program.connections.any fun connection =>
        connection.source == "time.immediate" &&
        connection.target == "time.connected" &&
        connection.delay == 3) true
  IO.println s!"{test.file} compiler test passed"

/-- A team with one parameter and one agent, for the `main` error cases. -/
def nodeTeam : String :=
  "team Node(idx : int)[agent : Codex] {
     input token : int
     output out : int
     prompt agent(token) -> out \"node $(idx) got $(token)\"
   }"

/-- One reaction carrying whatever deadline the caller writes. -/
def deadlineTeam (deadline : String) : String :=
  "team Desk[a : Codex] {
     input req : string
     output res : string
     prompt a(req) -> res? within(" ++ deadline ++ ") \"do it\"
   }
   main { d = Desk() }"

/-- A deadline is nanoseconds, and it is not part of the contract the runtime
    checks writes against — `within` sits between the two on the page. -/
def testWithin : IO Unit := do
  for (written, nanos) in [("30s", 30000000000), ("100ms", 100000000),
                           ("2min", 120000000000), ("1hr", 3600000000000)] do
    match lex (deadlineTeam written) >>= parse "Desk" with
    | .ok program => do
        assertEqual s!"within {written}"
          (program.reactions.any (·.within == some nanos)) true
        assertEqual s!"contract unpolluted by {written}"
          (program.reactions.any (·.contract == "d.res ?")) true
    | .error message => throw (IO.userError s!"within {written}: {message}")
  -- No deadline is not a zero deadline: the run-wide timeout still applies.
  let plain := "team Desk[a : Codex] { input req : string output res : string
                prompt a(req) -> res \"go\" } main { d = Desk() }"
  match lex plain >>= parse "Desk" with
  | .ok program =>
      assertEqual "no deadline" (program.reactions.all (·.within == none)) true
  | .error message => throw (IO.userError s!"no deadline: {message}")
  IO.println "within tests passed"

/-- A team with two ports and whatever declarations the caller writes between
    them, for exercising delays without an agent or a reaction. -/
def wireTeam (body : String) : String :=
  "team Wire { input src : int output dst : int " ++ body ++ " }
   main { w = Wire() }"

/-- Every delay in the language is a duration, and only zero may go without a
    unit. A bare number would say nothing about its magnitude. -/
def testDelayUnits : IO Unit := do
  for (body, nanos) in [("src -> dst after 0", 0), ("src -> dst after 3ns", 3),
                        ("src -> dst after 2ms", 2000000),
                        -- Zero with a unit is still zero, and still allowed.
                        ("src -> dst after 0ms", 0)] do
    match lex (wireTeam body) >>= parse "Wire" with
    | .ok program =>
        assertEqual s!"connection delay for '{body}'"
          (program.connections.any (·.delay == nanos)) true
    | .error message => throw (IO.userError s!"{body}: {message}")

  match lex (wireTeam "src -> dst after 0 timer t(0, 10ns)") >>= parse "Wire" with
  | .ok program => do
      assertEqual "timer offset" (program.timers.any (·.offset == 0)) true
      assertEqual "timer period" (program.timers.any (·.period == 10)) true
  | .error message => throw (IO.userError s!"timer units: {message}")

  -- A unit binds only to a number it touches. `0` here is followed by the word
  -- `action`, which must stay a declaration rather than becoming a unit.
  match lex (wireTeam "src -> dst after 0 action mid : int") >>= parse "Wire" with
  | .ok program =>
      assertEqual "a following declaration is not a unit"
        (program.ports.any (·.name == "w.mid")) true
  | .error message => throw (IO.userError s!"delay then declaration: {message}")
  IO.println "delay unit tests passed"

def rejectionCases : Array (String × String × String) := #[
  -- Only zero may be unitless; every other delay says what it means.
  ("connection delay without a unit",
    wireTeam "src -> dst after 3", "needs a unit"),
  ("timer offset without a unit",
    wireTeam "src -> dst after 0 timer t(5, 0)", "needs a unit"),
  ("timer period without a unit",
    wireTeam "src -> dst after 0 timer t(0, 10)", "needs a unit"),
  ("action delay without a unit",
    wireTeam "action mid(delay=2) : int src -> dst after 0", "needs a unit"),
  -- 'm' and 'ms' differ by one character and five orders of magnitude.
  ("ambiguous duration unit", deadlineTeam "5m", "ambiguous"),
  -- A bare number is logical delay elsewhere in the language; a duration is
  -- wall-clock, and the two must not read alike.
  ("duration without a unit", deadlineTeam "30", "needs a unit"),
  ("unknown duration unit", deadlineTeam "5weeks", "unknown duration unit"),
  ("unknown team",
    nodeTeam ++ " main { a = Missing(1) }", "unknown team 'Missing'"),
  ("argument count",
    nodeTeam ++ " main { a = Node(1, 2) }", "supplies 2"),
  ("argument type",
    nodeTeam ++ " main { a = Node(\"one\") }", "passes string to parameter"),
  ("unknown instance in a connection",
    nodeTeam ++ " main { a = Node(1) a.out -> b.token }", "unknown instance 'b'"),
  ("duplicate instance",
    nodeTeam ++ " main { a = Node(1) a = Node(2) }", "duplicate instance 'a'"),
  ("no main block at all",
    nodeTeam, "no main block"),
  ("a main block that instantiates nothing",
    nodeTeam ++ " main { }", "instantiates no team")
]

def testRejection (label : String) (source : String) (expected : String) : IO Unit :=
  match lex source >>= parse "check" with
  | .ok program =>
      throw (IO.userError s!"{label}: expected a rejection, compiled team '{program.team}'")
  | .error message =>
      if mentions message expected then IO.println s!"{label} rejected as expected"
      else throw (IO.userError s!"{label}: expected '{expected}', got '{message}'")

/-- A team with neither parameters nor agents needs neither bracket pair, and
    an unnamed main takes the fallback name — its source file's. -/
def testBareTeamHeader : IO Unit :=
  let source := "team Bare { input a : int output b : int a -> b after 0 }
                 main { only = Bare() }"
  match lex source >>= parse "Widget" with
  | .ok program => do
      assertEqual "program name" program.team "Widget"
      assertEqual "instance-qualified port" (program.ports.any (·.name == "only.a")) true
  | .error message => throw (IO.userError s!"bare team header: {message}")

/-- A named main wins over the fallback, which is the only name a program
    submitted over the wire has. -/
def testNamedMain : IO Unit :=
  let source := "team Bare { input a : int } main Payroll { only = Bare() }"
  match lex source >>= parse "temp-file-name" with
  | .ok program => assertEqual "named main" program.team "Payroll"
  | .error message => throw (IO.userError s!"named main: {message}")

def main : IO UInt32 := do
  try
    for test in topologyCases do
      testTopology test
    for (label, source, expected) in rejectionCases do
      testRejection label source expected
    testBareTeamHeader
    testNamedMain
    testWithin
    testDelayUnits
    IO.println "compiler rejection tests passed"
    pure 0
  catch error =>
    IO.eprintln error.toString
    pure 1
