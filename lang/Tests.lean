import Omar.Compiler

open Lean
open Omar

def assertEqual [BEq α] [ToString α] (label : String) (actual expected : α) : IO Unit :=
  if actual == expected then pure ()
  else throw (IO.userError s!"{label}: expected {expected}, got {actual}")

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
    reactions := 1, instructions := 22 },
  { file := "BusinessLoop.omar", team := "BusinessLoop", agents := 8,
    ports := 20, reactions := 11, instructions := 41 },
  { file := "EffectContracts.omar", team := "EffectContracts", agents := 1, ports := 6,
    reactions := 3, instructions := 12 },
  { file := "FanOutFanIn.omar", team := "FanOutFanIn", agents := 4, ports := 5,
    reactions := 4, instructions := 15 },
  { file := "HR.omar", team := "HR", agents := 3, ports := 6,
    reactions := 4, instructions := 15, codexOnly := false },
  { file := "HRCodex.omar", team := "HR", agents := 3, ports := 6,
    reactions := 4, instructions := 15 },
  { file := "OrTriggers.omar", team := "OrTriggers", agents := 3, ports := 5,
    reactions := 3, instructions := 13 },
  { file := "OrderedWrites.omar", team := "OrderedWrites", agents := 4, ports := 3,
    reactions := 4, instructions := 13 },
  { file := "Recurrence.omar", team := "Recurrence", agents := 1, ports := 3,
    reactions := 1, instructions := 7 },
  { file := "SameAgentSerial.omar", team := "SameAgentSerial", agents := 2, ports := 4,
    reactions := 3, instructions := 11 },
  { file := "SuperdenseTime.omar", team := "SuperdenseTime", agents := 3, ports := 6,
    connections := 1, reactions := 3, instructions := 15 }
]

def testTopology (test : TopologyCase) : IO Unit := do
  let path := s!"../tests/topology/src/{test.file}"
  let source ← IO.FS.readFile path
  let program ← match lex source >>= parse with
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
  let bytecode ← match compileSource source with
    | .ok bytecode => pure bytecode
    | .error message => throw (IO.userError s!"{test.file}: {message}")
  let secondCompile ← match compileSource source with
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
  if test.file == "SuperdenseTime.omar" then
    assertEqual "fixed action delay"
      (program.ports.any fun port => port.name == "fixed" && port.delay == some 2) true
    assertEqual "connected action delay"
      (program.ports.any fun port => port.name == "connected" && port.delay == some 1) true
    assertEqual "connection delay"
      (program.connections.any fun connection =>
        connection.source == "immediate" &&
        connection.target == "connected" &&
        connection.delay == 3) true
  IO.println s!"{test.file} compiler test passed"

def main : IO UInt32 := do
  try
    for test in topologyCases do
      testTopology test
    pure 0
  catch error =>
    IO.eprintln error.toString
    pure 1
