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
      else if "(),;:{}?|=<>[].".contains c then
        do pure (Token.sym c.toString :: (← lexChars rest))
      else
        throw s!"unexpected character '{c}'"

def lex (source : String) : Except String (List Token) := lexChars source.toList

structure Agent where
  name : String
  backend : String
  /-- The instance it belongs to. Every program instantiates, so this is never
      empty by the time elaboration is done. -/
  instance_ : String := ""
  deriving Repr

inductive PortKind where
  | input | output | action
  deriving Repr, BEq

structure Port where
  name : String
  kind : PortKind
  type : String
  delay : Option Nat := none
  instance_ : String := ""
  deriving Repr

/-- `timer t(offset, period)`.

    A trigger that fires from the runtime's own clock rather than from another
    reaction. `period = 0` fires once, at `offset`; a non-zero period re-arms
    it forever. Both are logical time, the same unit an action's `delay` and a
    connection's `after` are counted in. -/
structure Timer where
  name : String
  offset : Nat
  period : Nat
  instance_ : String := ""
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
  instance_ : String := ""
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

structure Instance where
  name : String
  team : String
  args : Array Literal
  deriving Repr

/-- A team as written: a template, which `main` instantiates. A team is never
    the program itself — that is what `main` is for. -/
structure TeamDecl where
  name : String
  params : Array Param
  agents : Array Agent
  ports : Array Port
  timers : Array Timer
  connections : Array Connection
  reactions : Array Reaction
  /-- Teams this one instantiates. A team is a template, so instantiating one
      inside another nests the template rather than sharing it: `b.a.out` is
      not `c.a.out`. -/
  instances : Array Instance := #[]
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
  /-- `main Name { … }` names the program. Without one it takes its source
      file's name, which a program submitted over the wire does not have. -/
  name : Option String
  instances : Array Instance
  connections : Array InstanceConnection
  deriving Repr

/-- What `main` instantiated: the container the diagram draws, and the team it
    came from. -/
structure InstanceDecl where
  name : String
  team : String
  /-- The instance that declared it, or empty for one `main` declared. -/
  parent : String := ""
  deriving Repr

/-- The elaborated program. Names are flattened into one namespace because that
    is what the VM runs, but which instance each name came from is kept: it is
    structure, and rediscovering it by splitting on '.' downstream would be
    guessing at a convention rather than reading a fact. -/
structure Program where
  team : String
  instances : Array InstanceDecl
  agents : Array Agent
  ports : Array Port
  timers : Array Timer
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

/-- `port`, or `instance.port` written as one dotted name. -/
private def qualifiedTail (head : String) : Parser String
  | Token.sym "." :: rest => do
      let (member, rest) ← word rest
      pure (s!"{head}.{member}", rest)
  | tokens => pure (head, tokens)

private partial def parseDependencies (acc : Array String) : Parser (Array String)
  | Token.sym ")" :: rest => pure (acc, rest)
  | tokens => do
      let (name, tokens) ← word tokens
      -- `refine.out` reads a contained instance's output, which is how a team
      -- observes what it instantiated.
      let (name, tokens) ← qualifiedTail name tokens
      match tokens with
      | Token.sym "," :: rest => parseDependencies (acc.push name) rest
      | Token.sym ")" :: rest => pure (acc.push name, rest)
      | _ => throw "expected ',' or ')' in prompt dependencies"

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

private def parseActionDelay : Parser (Option Nat)
  | Token.sym "(" :: Token.word "delay" :: Token.sym "=" :: rest => do
      let (delay, rest) ← natural rest
      let (_, rest) ← expectSym ")" rest
      pure (some delay, rest)
  | tokens => pure (none, tokens)

private partial def parseDeclarations
    (reactionIndex : Nat)
    (ports : Array Port)
    (timers : Array Timer)
    (connections : Array Connection)
    (reactions : Array Reaction)
    (instances : Array Instance) :
    List Token ->
      Except String
        (Array Port × Array Timer × Array Connection × Array Reaction × Array Instance ×
          List Token)
  | Token.sym "}" :: rest => pure (ports, timers, connections, reactions, instances, rest)
  -- `a = A();` ends with an optional semicolon; it separates declarations and
  -- means nothing else.
  | Token.sym ";" :: rest =>
      parseDeclarations reactionIndex ports timers connections reactions instances rest
  | Token.word "input" :: rest => do
      let (name, rest) ← word rest
      let (_, rest) ← expectSym ":" rest
      let (type, rest) ← parseType rest
      parseDeclarations reactionIndex (ports.push { name, kind := .input, type }) timers connections reactions instances rest
  | Token.word "output" :: rest => do
      let (name, rest) ← word rest
      let (_, rest) ← expectSym ":" rest
      let (type, rest) ← parseType rest
      parseDeclarations reactionIndex (ports.push { name, kind := .output, type }) timers connections reactions instances rest
  | Token.word "action" :: rest => do
      let (name, rest) ← word rest
      let (delay, rest) ← parseActionDelay rest
      match rest with
      | Token.sym ":" :: tail =>
          let (type, tail) ← parseType tail
          parseDeclarations reactionIndex (ports.push { name, kind := .action, type, delay }) timers connections reactions instances tail
      | _ =>
          parseDeclarations reactionIndex (ports.push { name, kind := .action, type := "signal", delay }) timers connections reactions instances rest
  | Token.word "timer" :: rest => do
      let (name, rest) ← word rest
      let (_, rest) ← expectSym "(" rest
      let (offset, rest) ← natural rest
      let (_, rest) ← expectSym "," rest
      let (period, rest) ← natural rest
      let (_, rest) ← expectSym ")" rest
      parseDeclarations reactionIndex ports (timers.push { name, offset, period }) connections reactions instances rest
  | Token.word name :: Token.sym "=" :: rest => do
      let (team, rest) ← word rest
      let (_, rest) ← expectSym "(" rest
      let (args, rest) ← parseArgs #[] rest
      parseDeclarations reactionIndex ports timers connections reactions
        (instances.push { name, team, args }) rest
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
      parseDeclarations (reactionIndex + 1) ports timers connections (reactions.push reaction) instances rest
  | Token.word first :: rest => do
      -- An endpoint is either a port of this team or `instance.port` of one it
      -- instantiated. Both are one name once the instance path is prepended,
      -- so the dot is kept rather than resolved here.
      let (source, rest) ← qualifiedTail first rest
      let (_, rest) ← expectSym "->" rest
      let (target, rest) ← word rest
      let (target, rest) ← qualifiedTail target rest
      -- `after` is optional, matching main: a connection with no delay still
      -- lands on the next microstep.
      let (delay, rest) ← match rest with
        | Token.word "after" :: tail => natural tail
        | _ => pure (0, rest)
      parseDeclarations reactionIndex ports timers (connections.push { source, target, delay }) reactions instances rest
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
  let (ports, timers, connections, reactions, instances, tokens) ←
    parseDeclarations 0 #[] #[] #[] #[] #[] tokens
  pure ({ name, params, agents, ports, timers, connections, reactions, instances }, tokens)

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
  let (name, tokens) := match tokens with
    | Token.word name :: rest => (some name, rest)
    | _ => (none, tokens)
  let (_, tokens) ← expectSym "{" tokens
  let (instances, connections, tokens) ← parseMainBody #[] #[] tokens
  pure ({ name, instances, connections }, tokens)

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
  let timerNames := program.timers.map (·.name)
  ensureUnique "agent" agentNames
  ensureUnique "port" portNames
  ensureUnique "timer" timerNames
  ensureUnique "connection" connectionNames
  for timer in program.timers do
    if containsName portNames timer.name then
      throw s!"timer '{timer.name}' is also a port; a trigger has one name"
    if timer.offset == 0 && timer.period == 0 then
      throw s!"timer '{timer.name}' never fires; give it an offset, a period, or both"
  for reaction in program.reactions do
    if !containsName agentNames reaction.agent then
      throw s!"reaction references unknown agent '{reaction.agent}'"
    for trigger in reaction.triggers do
      -- A reaction reads its own team's inputs and actions, and the *outputs*
      -- of teams its team instantiated. Reading its own output would be
      -- reading what it is there to write.
      let valid := program.ports.any (fun port =>
        port.name == trigger &&
          (port.kind != .output || port.instance_ != reaction.instance_)) ||
        containsName timerNames trigger
      if !valid then throw s!"unknown input/action dependency '{trigger}'"
    for effect in reaction.effects do
      if containsName timerNames effect then
        throw s!"timer '{effect}' cannot be written to; a timer is a trigger"
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
private def qualifyPrompt
    (inst : String) (ports : Array Port) (timers : Array Timer) (reads : Array String)
    (text : String) : String :=
  let named := ports.map (·.name) ++ timers.map (·.name) ++ reads
  named.foldl
    (fun acc name => acc.replace s!"$({name})" s!"$({qualify inst name})")
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

/-- Everything one instantiation contributes, its nested instantiations
    included. -/
structure Elaborated where
  agents : Array Agent := #[]
  ports : Array Port := #[]
  timers : Array Timer := #[]
  connections : Array Connection := #[]
  reactions : Array Reaction := #[]
  instances : Array InstanceDecl := #[]

private def Elaborated.append (a b : Elaborated) : Elaborated :=
  { agents := a.agents ++ b.agents
    ports := a.ports ++ b.ports
    timers := a.timers ++ b.timers
    connections := a.connections ++ b.connections
    reactions := a.reactions ++ b.reactions
    instances := a.instances ++ b.instances }

/-- How deep teams may nest.

    A team that instantiates itself, directly or through another, describes an
    infinite program. Nothing else in the language recurses, so rather than
    tracking the path this stops at a depth no honest program reaches and says
    what it suspects. -/
private def maxNesting : Nat := 32

private partial def elaborateInstance
    (teams : Array TeamDecl) (depth : Nat) (parent : String) (inst : Instance) :
    Except String Elaborated := do
  if depth > maxNesting then
    throw s!"team instantiation nests more than {maxNesting} deep at '{inst.name}'; \
      is a team instantiating itself?"
  let decl ← match teams.find? (·.name == inst.team) with
    | some decl => pure decl
    | none => throw s!"instance '{inst.name}' names unknown team '{inst.team}'"
  let bindings ← bindArguments decl inst
  -- The path from `main` down to here. Qualifying by it rather than by the
  -- instance's own name is the whole of nesting: `b.a.out` and `c.a.out` are
  -- different ports of different copies of the same team.
  let path := if parent.isEmpty then inst.name else s!"{parent}.{inst.name}"
  ensureUnique "instance" (decl.instances.map (·.name))
  let agents := decl.agents.map fun agent =>
    { agent with name := qualify path agent.name, instance_ := path }
  let ports := decl.ports.map fun port =>
    { port with name := qualify path port.name, instance_ := path }
  let timers := decl.timers.map fun timer =>
    { timer with name := qualify path timer.name, instance_ := path }
  -- An endpoint written `a.out` is already the nested instance's local name,
  -- so prefixing the path is all it takes to reach `b.a.out`.
  let connections := decl.connections.map fun connection =>
    { connection with
        source := qualify path connection.source
        target := qualify path connection.target }
  let reactions := decl.reactions.map fun reaction =>
    { reaction with
        id := qualify path reaction.id
        agent := qualify path reaction.agent
        triggers := reaction.triggers.map (qualify path)
        effects := reaction.effects.map (qualify path)
        instance_ := path
        contract := qualifyContract path decl.ports reaction.contract
        prompt :=
          substitute bindings
            (qualifyPrompt path decl.ports decl.timers reaction.triggers reaction.prompt) }
  let own : Elaborated :=
    { agents, ports, timers, connections, reactions
      instances := #[{ name := path, team := inst.team, parent }] }
  decl.instances.foldlM
    (fun acc nested => do
      pure (acc.append (← elaborateInstance teams (depth + 1) path nested)))
    own

private def elaborate (programName : String) (teams : Array TeamDecl) (main : Main) :
    Except String Program := do
  ensureUnique "instance" (main.instances.map (·.name))
  let instanceNames := main.instances.map (·.name)
  for connection in main.connections do
    for side in #[connection.sourceInstance, connection.targetInstance] do
      if !containsName instanceNames side then
        throw s!"main connection names unknown instance '{side}'"
  let parts ← main.instances.mapM (elaborateInstance teams 0 "")
  let wired := main.connections.map fun connection =>
    { source := qualify connection.sourceInstance connection.sourcePort
      target := qualify connection.targetInstance connection.targetPort
      delay := connection.delay : Connection }
  let whole := parts.foldl Elaborated.append {}
  pure {
    team := programName
    instances := whole.instances
    agents := whole.agents
    ports := whole.ports
    timers := whole.timers
    connections := whole.connections ++ wired
    reactions := whole.reactions
  }

/-- `programName` is the fallback when `main` is not named: the source file,
    the way a C binary is named for its file rather than for `main`. A program
    that arrives over the wire has no file, which is why `main` can name
    itself. -/
def parse (programName : String) (tokens : List Token) : Except String Program := do
  let (teams, tokens) ← parseTeams #[] tokens
  if teams.isEmpty then throw "program declares no team"
  ensureUnique "team" (teams.map (·.name))
  if tokens.isEmpty then
    throw "program has no main block; a program runs instantiated teams, so \
      every program needs 'main { … }'"
  let (main, tokens) ← parseMain tokens
  if !tokens.isEmpty then throw s!"unexpected tokens after main: {reprStr tokens.head?}"
  if main.instances.isEmpty then
    throw "main instantiates no team; a program with no instance has nothing to run"
  validate (← elaborate (main.name.getD programName) teams main)

private def jsonStringArray (values : Array String) : Json :=
  Json.arr (values.map toJson)

private def renderField (field : String × Json) : String :=
  s!"{(toJson field.1).compress}: {field.2.compress}"

private def instruction (op : String) (fields : List (String × Json) := []) : String :=
  "{" ++ String.intercalate ", " ((("op", toJson op) :: fields).map renderField) ++ "}"

def compile (program : Program) : String :=
  let begin := instruction "begin_plan" [("team", toJson program.team)]
  let instances := program.instances.map fun inst =>
    instruction "declare_instance" [
      ("name", toJson inst.name),
      ("parent", toJson inst.parent),
      ("team", toJson inst.team)
    ]
  let agents := program.agents.map fun agent =>
    instruction "spawn_agent" [
      ("instance", toJson agent.instance_),
      ("name", toJson agent.name),
      ("backend", toJson agent.backend)
    ]
  let ports := program.ports.map fun port =>
    let fields := [
      ("instance", toJson port.instance_),
      ("kind", toJson (kindName port.kind)),
      ("name", toJson port.name),
      ("type", toJson port.type)
    ] ++ match port.delay with
      | some delay => [("delay", toJson delay)]
      | none => []
    instruction "define_port" fields
  let timers := program.timers.map fun timer =>
    instruction "declare_timer" [
      ("instance", toJson timer.instance_),
      ("name", toJson timer.name),
      ("offset", toJson timer.offset),
      ("period", toJson timer.period)
    ]
  let connections := program.connections.map fun connection =>
    instruction "connect_ports" [
      ("source", toJson connection.source),
      ("target", toJson connection.target),
      ("delay", toJson connection.delay)
    ]
  let reactions := program.reactions.map fun reaction =>
    instruction "install_reaction" [
      ("instance", toJson reaction.instance_),
      ("id", toJson reaction.id),
      ("agent", toJson reaction.agent),
      ("triggers", jsonStringArray reaction.triggers),
      ("effects", jsonStringArray reaction.effects),
      ("contract", toJson reaction.contract),
      ("prompt", toJson reaction.prompt)
    ]
  let commit := instruction "commit_plan"
  let instructions :=
    #[begin] ++ instances ++ agents ++ ports ++ timers ++ connections ++ reactions ++ #[commit]
  let rendered := String.intercalate ",\n    " instructions.toList
  "{\n  \"version\": 1,\n  \"team\": " ++ (toJson program.team).compress ++
    ",\n  \"instructions\": [\n    " ++ rendered ++ "\n  ]\n}\n"

def compileSource (programName : String) (source : String) : Except String String := do
  pure (compile (← parse programName (← lex source)))

end Omar
