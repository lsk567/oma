import Lean

open Lean

namespace Omar

inductive Token where
  | word : String -> Token
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
      else if "(),:{}?|=<>".contains c then
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
  deriving Repr

structure Reaction where
  id : String
  agent : String
  triggers : Array String
  effects : Array String
  contract : String
  prompt : String
  deriving Repr

structure Program where
  team : String
  agents : Array Agent
  ports : Array Port
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
  | Token.sym ")" :: _ => pure (#[], tokens)
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

private def kindName : PortKind -> String
  | .input => "input"
  | .output => "output"
  | .action => "action"

private def tokenSource : Token -> String
  | .word value => value
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

private partial def parseDeclarations
    (reactionIndex : Nat)
    (ports : Array Port)
    (reactions : Array Reaction) :
    List Token -> Except String (Array Port × Array Reaction × List Token)
  | Token.sym "}" :: rest => pure (ports, reactions, rest)
  | Token.word "input" :: rest => do
      let (name, rest) ← word rest
      let (_, rest) ← expectSym ":" rest
      let (type, rest) ← parseType rest
      parseDeclarations reactionIndex (ports.push { name, kind := .input, type }) reactions rest
  | Token.word "output" :: rest => do
      let (name, rest) ← word rest
      let (_, rest) ← expectSym ":" rest
      let (type, rest) ← parseType rest
      parseDeclarations reactionIndex (ports.push { name, kind := .output, type }) reactions rest
  | Token.word "action" :: rest => do
      let (name, rest) ← word rest
      match rest with
      | Token.sym ":" :: tail =>
          let (type, tail) ← parseType tail
          parseDeclarations reactionIndex (ports.push { name, kind := .action, type }) reactions tail
      | _ =>
          parseDeclarations reactionIndex (ports.push { name, kind := .action, type := "signal" }) reactions rest
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
      parseDeclarations (reactionIndex + 1) ports (reactions.push reaction) rest
  | token :: _ => throw s!"unexpected token in team body: {reprStr token}"
  | [] => throw "unterminated team body"

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
  ensureUnique "agent" agentNames
  ensureUnique "port" portNames
  for reaction in program.reactions do
    if !containsName agentNames reaction.agent then
      throw s!"reaction names unknown agent '{reaction.agent}'"
    for trigger in reaction.triggers do
      let valid := program.ports.any fun port =>
        port.name == trigger && port.kind != .output
      if !valid then throw s!"unknown input/action dependency '{trigger}'"
    for effect in reaction.effects do
      let valid := program.ports.any fun port =>
        port.name == effect && port.kind != .input
      if !valid then throw s!"unknown output/action production '{effect}'"
  pure program

def parse (tokens : List Token) : Except String Program := do
  let (_, tokens) ← expectWord "team" tokens
  let (team, tokens) ← word tokens
  let (_, tokens) ← expectSym "(" tokens
  let (agents, tokens) ← parseAgents tokens
  let (_, tokens) ← expectSym ")" tokens
  let (_, tokens) ← expectSym "{" tokens
  let (ports, reactions, tokens) ← parseDeclarations 0 #[] #[] tokens
  if !tokens.isEmpty then throw s!"unexpected tokens after team: {reprStr tokens.head?}"
  validate { team, agents, ports, reactions }

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
    instruction "define_port" [
      ("kind", toJson (kindName port.kind)),
      ("name", toJson port.name),
      ("type", toJson port.type)
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
  let instructions := #[begin] ++ agents ++ ports ++ reactions ++ #[commit]
  let rendered := String.intercalate ",\n    " instructions.toList
  "{\n  \"version\": 1,\n  \"team\": " ++ (toJson program.team).compress ++
    ",\n  \"instructions\": [\n    " ++ rendered ++ "\n  ]\n}\n"

def compileSource (source : String) : Except String String := do
  pure (compile (← parse (← lex source)))

end Omar
