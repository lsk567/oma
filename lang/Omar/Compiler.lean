import Lean

open Lean

namespace Omar

inductive Token where
  | word : String -> Token
  | nat : Nat -> Token
  | text : String -> Token
  | sym : String -> Token
  deriving Repr, BEq

private def isWordStart (c : Char) : Bool := c.isAlpha || c == '_'
private def isWordRest (c : Char) : Bool := c.isAlphanum || c == '_'

private def takeWhile (p : Char -> Bool) : List Char -> List Char × List Char
  | [] => ([], [])
  | c :: cs =>
      if p c then
        let (head, tail) := takeWhile p cs
        (c :: head, tail)
      else
        ([], c :: cs)

private partial def skipBlockComment : Nat -> List Char -> Except String (List Char)
  | _, [] => throw "unterminated block comment"
  | depth, '/' :: '*' :: rest => skipBlockComment (depth + 1) rest
  | 1, '*' :: '/' :: rest => pure rest
  | depth, '*' :: '/' :: rest => skipBlockComment (depth - 1) rest
  | depth, _ :: rest => skipBlockComment depth rest

private partial def readText (acc : List Char) : List Char -> Except String (String × List Char)
  | [] => throw "unterminated prompt string"
  | '\\' :: '"' :: rest => readText ('"' :: acc) rest
  | '\\' :: '\\' :: rest => readText ('\\' :: acc) rest
  | '"' :: rest => pure (String.ofList acc.reverse, rest)
  | c :: rest => readText (c :: acc) rest

private partial def lexChars : List Char -> Except String (List Token)
  | [] => pure []
  | '/' :: '/' :: rest =>
      let (_, tail) := takeWhile (fun c => c != '\n') rest
      lexChars tail
  | '/' :: '*' :: rest => do
      lexChars (← skipBlockComment 1 rest)
  | '-' :: '>' :: rest => do
      pure (Token.sym "->" :: (← lexChars rest))
  | '"' :: rest => do
      let (value, tail) ← readText [] rest
      pure (Token.text value :: (← lexChars tail))
  | c :: rest =>
      if c.isWhitespace then
        lexChars rest
      else if isWordStart c then
        let (suffix, tail) := takeWhile isWordRest rest
        do pure (Token.word (String.ofList (c :: suffix)) :: (← lexChars tail))
      else if c.isDigit then
        let (suffix, tail) := takeWhile (·.isDigit) rest
        let value := String.ofList (c :: suffix)
        match value.toNat? with
        | some value => do pure (Token.nat value :: (← lexChars tail))
        | none => throw s!"invalid natural number '{value}'"
      else if "(),:{}?|=<>[].".contains c then
        do pure (Token.sym c.toString :: (← lexChars rest))
      else
        throw s!"unexpected character '{c}'"

def lex (source : String) : Except String (List Token) := lexChars source.toList

structure Agent where
  name : String
  backend : String
  deriving Repr

inductive PortKind where
  | input | output | action
  deriving Repr, BEq

structure Port where
  name : String
  kind : PortKind
  type : String
  delay : Option Nat := none
  deriving Repr

structure Connection where
  source : String
  target : String
  delay : Nat
  deriving Repr

structure Reaction where
  id : String
  agent : String
  triggers : Array String
  effects : Array String
  contract : String
  prompt : String
  deriving Repr

/-- A compile-time team parameter, supplied by the instantiation in `main`. -/
structure Param where
  name : String
  type : String
  deriving Repr

/-- An argument to a team instantiation. Only the literal forms the lexer
    produces; parameters are constants, so there is nothing to evaluate. -/
inductive Literal where
  | int : Nat -> Literal
  | str : String -> Literal
  deriving Repr, BEq

/-- A team as written. With a `main` block it is a template that `main`
    instantiates; without one it is the program itself. -/
structure TeamDecl where
  name : String
  params : Array Param
  agents : Array Agent
  ports : Array Port
  connections : Array Connection
  reactions : Array Reaction
  deriving Repr

structure Instance where
  name : String
  team : String
  args : Array Literal
  deriving Repr

/-- `a.out -> b.in after 1`. Ends are qualified by instance, so unlike a
    team-local connection this one names four things rather than two. -/
structure InstanceConnection where
  sourceInstance : String
  sourcePort : String
  targetInstance : String
  targetPort : String
  delay : Nat
  deriving Repr

structure Main where
  instances : Array Instance
  connections : Array InstanceConnection
  deriving Repr

/-- The elaborated program: one flat namespace, which is what the VM runs.
    Instances are gone by this point, having been expanded into qualified
    agents, ports and reactions. -/
structure Program where
  team : String
  agents : Array Agent
  ports : Array Port
  connections : Array Connection
  reactions : Array Reaction
  deriving Repr

abbrev Parser (α : Type) := List Token -> Except String (α × List Token)

private def word : Parser String
  | Token.word value :: rest => pure (value, rest)
  | tokens => throw s!"expected identifier, found {reprStr tokens.head?}"

private def expectWord (expected : String) : Parser Unit
  | Token.word actual :: rest =>
      if actual == expected then pure ((), rest)
      else throw s!"expected '{expected}', found '{actual}'"
  | tokens => throw s!"expected '{expected}', found {reprStr tokens.head?}"

private def expectSym (expected : String) : Parser Unit
  | Token.sym actual :: rest =>
      if actual == expected then pure ((), rest)
      else throw s!"expected '{expected}', found '{actual}'"
  | tokens => throw s!"expected '{expected}', found {reprStr tokens.head?}"

private def natural : Parser Nat
  | Token.nat value :: rest => pure (value, rest)
  | tokens => throw s!"expected natural number, found {reprStr tokens.head?}"

private partial def parseType : Parser String
  | Token.word "bool" :: rest => pure ("bool", rest)
  | Token.word "int" :: rest => pure ("int", rest)
  | Token.word "float" :: rest => pure ("float", rest)
  | Token.word "string" :: rest => pure ("string", rest)
  | Token.word "path" :: rest => pure ("path", rest)
  | Token.word "bytes" :: rest => pure ("bytes", rest)
  | Token.word outer :: Token.sym "<" :: rest => do
      if outer != "list" && outer != "option" then
        throw s!"unknown generic type '{outer}'"
      let (inner, rest) ← parseType rest
      let (_, rest) ← expectSym ">" rest
      pure (s!"{outer}<{inner}>", rest)
  | Token.word value :: _ => throw s!"unknown port type '{value}'"
  | tokens => throw s!"expected port type, found {reprStr tokens.head?}"

private partial def parseAgents (tokens : List Token) : Except String (Array Agent × List Token) := do
  match tokens with
  | Token.sym "]" :: _ => pure (#[], tokens)
  | _ =>
      let (name, tokens) ← word tokens
      let (_, tokens) ← expectSym ":" tokens
      let (backend, tokens) ← word tokens
      let agent := { name, backend : Agent }
      match tokens with
      | Token.sym "," :: rest =>
          let (agents, tail) ← parseAgents rest
          pure (#[agent] ++ agents, tail)
      | _ => pure (#[agent], tokens)

private partial def parseParams (tokens : List Token) : Except String (Array Param × List Token) := do
  match tokens with
  | Token.sym ")" :: _ => pure (#[], tokens)
  | _ =>
      let (name, tokens) ← word tokens
      let (_, tokens) ← expectSym ":" tokens
      let (type, tokens) ← parseType tokens
      let param := { name, type : Param }
      match tokens with
      | Token.sym "," :: rest =>
          let (params, tail) ← parseParams rest
          pure (#[param] ++ params, tail)
      | _ => pure (#[param], tokens)

private def kindName : PortKind -> String
  | .input => "input"
  | .output => "output"
  | .action => "action"

private def tokenSource : Token -> String
  | .word value => value
  | .nat value => toString value
  | .sym value => value
  | .text _ => "<prompt>"

private def productionTargets (tokens : List Token) : Array String :=
  -- Keep only words which occur at the start of an atom. Literals always
  -- follow '=' and are skipped.
  let rec collect (expectAtom : Bool) (acc : Array String) : List Token -> Array String
    | [] => acc
    | Token.sym "=" :: rest => collect false acc rest
    | Token.sym "(" :: rest => collect true acc rest
    | Token.sym "|" :: rest => collect true acc rest
    | Token.sym "," :: rest => collect true acc rest
    | Token.sym "?" :: rest => collect false acc rest
    | Token.sym ")" :: rest => collect false acc rest
    | Token.word value :: rest =>
        if expectAtom then collect false (acc.push value) rest
        else collect false acc rest
    | _ :: rest => collect expectAtom acc rest
  collect true #[] tokens

private partial def takeContract (acc : List Token) : List Token -> Except String (List Token × String × List Token)
  | [] => throw "expected prompt string after production contract"
  | Token.text prompt :: rest => pure (acc.reverse, prompt, rest)
  | token :: rest => takeContract (token :: acc) rest

private partial def parseDependencies (acc : Array String) : Parser (Array String)
  | Token.sym ")" :: rest => pure (acc, rest)
  | tokens => do
      let (name, tokens) ← word tokens
      match tokens with
      | Token.sym "," :: rest => parseDependencies (acc.push name) rest
      | Token.sym ")" :: rest => pure (acc.push name, rest)
      | _ => throw "expected ',' or ')' in prompt dependencies"

private def parseActionDelay : Parser (Option Nat)
  | Token.sym "(" :: Token.word "delay" :: Token.sym "=" :: rest => do
      let (delay, rest) ← natural rest
      let (_, rest) ← expectSym ")" rest
      pure (some delay, rest)
  | tokens => pure (none, tokens)

private partial def parseDeclarations
    (reactionIndex : Nat)
    (ports : Array Port)
    (connections : Array Connection)
    (reactions : Array Reaction) :
    List Token -> Except String (Array Port × Array Connection × Array Reaction × List Token)
  | Token.sym "}" :: rest => pure (ports, connections, reactions, rest)
  | Token.word "input" :: rest => do
      let (name, rest) ← word rest
      let (_, rest) ← expectSym ":" rest
      let (type, rest) ← parseType rest
      parseDeclarations reactionIndex (ports.push { name, kind := .input, type }) connections reactions rest
  | Token.word "output" :: rest => do
      let (name, rest) ← word rest
      let (_, rest) ← expectSym ":" rest
      let (type, rest) ← parseType rest
      parseDeclarations reactionIndex (ports.push { name, kind := .output, type }) connections reactions rest
  | Token.word "action" :: rest => do
      let (name, rest) ← word rest
      let (delay, rest) ← parseActionDelay rest
      match rest with
      | Token.sym ":" :: tail =>
          let (type, tail) ← parseType tail
          parseDeclarations reactionIndex (ports.push { name, kind := .action, type, delay }) connections reactions tail
      | _ =>
          parseDeclarations reactionIndex (ports.push { name, kind := .action, type := "signal", delay }) connections reactions rest
  | Token.word source :: Token.sym "->" :: rest => do
      let (target, rest) ← word rest
      let (_, rest) ← expectWord "after" rest
      let (delay, rest) ← natural rest
      parseDeclarations reactionIndex ports (connections.push { source, target, delay }) reactions rest
  | Token.word "prompt" :: rest => do
      let (agent, rest) ← word rest
      let (_, rest) ← expectSym "(" rest
      let (triggers, rest) ← parseDependencies #[] rest
      let (_, rest) ← expectSym "->" rest
      let (contractTokens, prompt, rest) ← takeContract [] rest
      let effects := productionTargets contractTokens
      let contract := String.intercalate " " (contractTokens.map tokenSource)
      let reaction := {
        id := s!"reaction.{reactionIndex}"
        agent, triggers, effects, contract, prompt
      }
      parseDeclarations (reactionIndex + 1) ports connections (reactions.push reaction) rest
  | token :: _ => throw s!"unexpected token in team body: {reprStr token}"
  | [] => throw "unterminated team body"

private def parseTeam : Parser TeamDecl := fun tokens => do
  let (_, tokens) ← expectWord "team" tokens
  let (name, tokens) ← word tokens
  -- Both lists are optional: a team with neither parameters nor agents is
  -- just `team Name { ... }`.
  let (params, tokens) ← match tokens with
    | Token.sym "(" :: rest => do
        let (params, rest) ← parseParams rest
        let (_, rest) ← expectSym ")" rest
        pure (params, rest)
    | _ => pure (#[], tokens)
  let (agents, tokens) ← match tokens with
    | Token.sym "[" :: rest => do
        let (agents, rest) ← parseAgents rest
        let (_, rest) ← expectSym "]" rest
        pure (agents, rest)
    | _ => pure (#[], tokens)
  let (_, tokens) ← expectSym "{" tokens
  let (ports, connections, reactions, tokens) ← parseDeclarations 0 #[] #[] #[] tokens
  pure ({ name, params, agents, ports, connections, reactions }, tokens)

private def literal : Parser Literal
  | Token.nat value :: rest => pure (Literal.int value, rest)
  | Token.text value :: rest => pure (Literal.str value, rest)
  | tokens => throw s!"expected an int or string argument, found {reprStr tokens.head?}"

private partial def parseArgs (acc : Array Literal) : Parser (Array Literal)
  | Token.sym ")" :: rest => pure (acc, rest)
  | tokens => do
      let (value, tokens) ← literal tokens
      match tokens with
      | Token.sym "," :: rest => parseArgs (acc.push value) rest
      | Token.sym ")" :: rest => pure (acc.push value, rest)
      | _ => throw "expected ',' or ')' in team arguments"

private partial def parseMainBody
    (instances : Array Instance)
    (connections : Array InstanceConnection) :
    List Token -> Except String (Array Instance × Array InstanceConnection × List Token)
  | Token.sym "}" :: rest => pure (instances, connections, rest)
  | Token.word name :: Token.sym "=" :: rest => do
      let (team, rest) ← word rest
      let (_, rest) ← expectSym "(" rest
      let (args, rest) ← parseArgs #[] rest
      parseMainBody (instances.push { name, team, args }) connections rest
  | Token.word sourceInstance :: Token.sym "." :: rest => do
      let (sourcePort, rest) ← word rest
      let (_, rest) ← expectSym "->" rest
      let (targetInstance, rest) ← word rest
      let (_, rest) ← expectSym "." rest
      let (targetPort, rest) ← word rest
      -- `after` is optional here. A connection with no delay still lands on
      -- the next microstep, so a feedback loop through instances progresses.
      let (delay, rest) ← match rest with
        | Token.word "after" :: tail => natural tail
        | _ => pure (0, rest)
      let connection :=
        { sourceInstance, sourcePort, targetInstance, targetPort, delay : InstanceConnection }
      parseMainBody instances (connections.push connection) rest
  | token :: _ => throw s!"unexpected token in main: {reprStr token}"
  | [] => throw "unterminated main block"

private def parseMain : Parser Main := fun tokens => do
  let (_, tokens) ← expectWord "main" tokens
  let (_, tokens) ← expectSym "{" tokens
  let (instances, connections, tokens) ← parseMainBody #[] #[] tokens
  pure ({ instances, connections }, tokens)

private partial def parseTeams (acc : Array TeamDecl) :
    List Token -> Except String (Array TeamDecl × List Token)
  | [] => pure (acc, [])
  | tokens@(Token.word "main" :: _) => pure (acc, tokens)
  | tokens => do
      let (decl, tokens) ← parseTeam tokens
      parseTeams (acc.push decl) tokens

private def containsName (names : Array String) (name : String) : Bool :=
  names.any (· == name)

private def ensureUnique (kind : String) (names : Array String) : Except String Unit :=
  let rec loop (seen : Array String) : List String -> Except String Unit
    | [] => pure ()
    | name :: rest =>
        if containsName seen name then throw s!"duplicate {kind} '{name}'"
        else loop (seen.push name) rest
  loop #[] names.toList

private def validate (program : Program) : Except String Program := do
  let agentNames := program.agents.map (·.name)
  let portNames := program.ports.map (·.name)
  let connectionNames := program.connections.map fun connection =>
    s!"{connection.source}->{connection.target}"
  ensureUnique "agent" agentNames
  ensureUnique "port" portNames
  ensureUnique "connection" connectionNames
  for reaction in program.reactions do
    if !containsName agentNames reaction.agent then
      throw s!"reaction references unknown agent '{reaction.agent}'"
    for trigger in reaction.triggers do
      let valid := program.ports.any fun port =>
        port.name == trigger && port.kind != .output
      if !valid then throw s!"unknown input/action dependency '{trigger}'"
    for effect in reaction.effects do
      let valid := program.ports.any fun port =>
        port.name == effect && port.kind != .input
      if !valid then throw s!"unknown output/action production '{effect}'"
  for connection in program.connections do
    let source ← match program.ports.find? (·.name == connection.source) with
      | some port => pure port
      | none => throw s!"connection names unknown source port '{connection.source}'"
    let target ← match program.ports.find? (·.name == connection.target) with
      | some port => pure port
      | none => throw s!"connection names unknown target port '{connection.target}'"
    if source.type != target.type then
      throw s!"connection type mismatch from '{connection.source}' to '{connection.target}'"
  pure program

private def literalText : Literal -> String
  | .int value => toString value
  | .str value => value

private def literalType : Literal -> String
  | .int _ => "int"
  | .str _ => "string"

/-- `instance.member`. The VM has one flat namespace, so instantiating a team
    is a renaming: everything it declares gains its instance's prefix. -/
private def qualify (instance_ : String) (name : String) : String := s!"{instance_}.{name}"

/-- Parameters are compile-time constants, so they are substituted into the
    prompt here. The runtime resolves `$(…)` against trigger values and has no
    idea a parameter existed. -/
private def substitute (bindings : Array (String × String)) (text : String) : String :=
  bindings.foldl (fun acc binding => acc.replace s!"$({binding.1})" binding.2) text

/-- The runtime matches contract names against the effects a reaction writes,
    and those are qualified, so the contract has to be too. Only declared port
    names are rewritten; `( | ) , ? =` and constants are left alone. -/
private def qualifyContract (inst : String) (ports : Array Port) (contract : String) : String :=
  let portNames := ports.map (·.name)
  String.intercalate " " ((contract.splitOn " ").map fun piece =>
    if portNames.any (· == piece) then qualify inst piece else piece)

/-- Likewise for `$(port)` in a prompt: the runtime looks the name up among the
    trigger values, which are qualified.

    Only the plain spelling is rewritten. `$( port )` and `$(port /* note */)`
    are accepted by the runtime but not matched here, so inside a team that
    `main` instantiates, write `$(port)`. -/
private def qualifyPrompt (inst : String) (ports : Array Port) (text : String) : String :=
  ports.foldl
    (fun acc port => acc.replace s!"$({port.name})" s!"$({qualify inst port.name})")
    text

private def bindArguments (decl : TeamDecl) (inst : Instance) :
    Except String (Array (String × String)) := do
  if decl.params.size != inst.args.size then
    throw s!"team '{decl.name}' takes {decl.params.size} argument(s), \
      but '{inst.name}' supplies {inst.args.size}"
  (decl.params.zip inst.args).foldlM
    (fun acc (param, arg) => do
      if param.type != literalType arg then
        throw s!"'{inst.name}' passes {literalType arg} to parameter \
          '{param.name}' of type {param.type}"
      pure (acc.push (param.name, literalText arg)))
    (#[] : Array (String × String))

private def elaborateInstance (teams : Array TeamDecl) (inst : Instance) :
    Except String (Array Agent × Array Port × Array Connection × Array Reaction) := do
  let decl ← match teams.find? (·.name == inst.team) with
    | some decl => pure decl
    | none => throw s!"instance '{inst.name}' names unknown team '{inst.team}'"
  let bindings ← bindArguments decl inst
  let agents := decl.agents.map fun agent =>
    { agent with name := qualify inst.name agent.name }
  let ports := decl.ports.map fun port =>
    { port with name := qualify inst.name port.name }
  let connections := decl.connections.map fun connection =>
    { connection with
        source := qualify inst.name connection.source
        target := qualify inst.name connection.target }
  let reactions := decl.reactions.map fun reaction =>
    { reaction with
        id := qualify inst.name reaction.id
        agent := qualify inst.name reaction.agent
        triggers := reaction.triggers.map (qualify inst.name)
        effects := reaction.effects.map (qualify inst.name)
        contract := qualifyContract inst.name decl.ports reaction.contract
        prompt := substitute bindings (qualifyPrompt inst.name decl.ports reaction.prompt) }
  pure (agents, ports, connections, reactions)

private def elaborate (teams : Array TeamDecl) (main : Main) : Except String Program := do
  ensureUnique "instance" (main.instances.map (·.name))
  let instanceNames := main.instances.map (·.name)
  for connection in main.connections do
    for side in #[connection.sourceInstance, connection.targetInstance] do
      if !containsName instanceNames side then
        throw s!"main connection names unknown instance '{side}'"
  let parts ← main.instances.mapM (elaborateInstance teams)
  let wired := main.connections.map fun connection =>
    { source := qualify connection.sourceInstance connection.sourcePort
      target := qualify connection.targetInstance connection.targetPort
      delay := connection.delay : Connection }
  pure {
    -- The main block is the entry point, and unlike a team it has no name of
    -- its own to take.
    team := "main"
    agents := parts.foldl (fun acc part => acc ++ part.1) #[]
    ports := parts.foldl (fun acc part => acc ++ part.2.1) #[]
    connections := parts.foldl (fun acc part => acc ++ part.2.2.1) #[] ++ wired
    reactions := parts.foldl (fun acc part => acc ++ part.2.2.2) #[]
  }

def parse (tokens : List Token) : Except String Program := do
  let (teams, tokens) ← parseTeams #[] tokens
  if teams.isEmpty then throw "program declares no team"
  ensureUnique "team" (teams.map (·.name))
  match tokens with
  | [] =>
      -- No main block: the single team is the program, exactly as before.
      let decl ← match teams.toList with
        | [decl] => pure decl
        | _ => throw "a program with several teams needs a main block to instantiate them"
      if !decl.params.isEmpty then
        throw s!"team '{decl.name}' has parameters, which only a main block can supply"
      validate {
        team := decl.name, agents := decl.agents, ports := decl.ports,
        connections := decl.connections, reactions := decl.reactions
      }
  | _ =>
      let (main, tokens) ← parseMain tokens
      if !tokens.isEmpty then throw s!"unexpected tokens after main: {reprStr tokens.head?}"
      validate (← elaborate teams main)

private def jsonStringArray (values : Array String) : Json :=
  Json.arr (values.map toJson)

private def renderField (field : String × Json) : String :=
  s!"{(toJson field.1).compress}: {field.2.compress}"

private def instruction (op : String) (fields : List (String × Json) := []) : String :=
  "{" ++ String.intercalate ", " ((("op", toJson op) :: fields).map renderField) ++ "}"

def compile (program : Program) : String :=
  let begin := instruction "begin_plan" [("team", toJson program.team)]
  let agents := program.agents.map fun agent =>
    instruction "spawn_agent" [
      ("name", toJson agent.name),
      ("backend", toJson agent.backend)
    ]
  let ports := program.ports.map fun port =>
    let fields := [
      ("kind", toJson (kindName port.kind)),
      ("name", toJson port.name),
      ("type", toJson port.type)
    ] ++ match port.delay with
      | some delay => [("delay", toJson delay)]
      | none => []
    instruction "define_port" fields
  let connections := program.connections.map fun connection =>
    instruction "connect_ports" [
      ("source", toJson connection.source),
      ("target", toJson connection.target),
      ("delay", toJson connection.delay)
    ]
  let reactions := program.reactions.map fun reaction =>
    instruction "install_reaction" [
      ("id", toJson reaction.id),
      ("agent", toJson reaction.agent),
      ("triggers", jsonStringArray reaction.triggers),
      ("effects", jsonStringArray reaction.effects),
      ("contract", toJson reaction.contract),
      ("prompt", toJson reaction.prompt)
    ]
  let commit := instruction "commit_plan"
  let instructions := #[begin] ++ agents ++ ports ++ connections ++ reactions ++ #[commit]
  let rendered := String.intercalate ",\n    " instructions.toList
  "{\n  \"version\": 1,\n  \"team\": " ++ (toJson program.team).compress ++
    ",\n  \"instructions\": [\n    " ++ rendered ++ "\n  ]\n}\n"

def compileSource (source : String) : Except String String := do
  pure (compile (← parse (← lex source)))

end Omar
