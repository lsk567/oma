import Omar.Compiler

open Omar

def usage : String := "usage: omarc <input.omar> <output.json>"

def main (args : List String) : IO UInt32 := do
  match args with
  | [input, output] =>
      try
        let source ← IO.FS.readFile input
        match compileSource source with
        | .ok bytecode =>
            IO.FS.writeFile output bytecode
            IO.println s!"compiled {input} -> {output}"
            pure 0
        | .error message =>
            IO.eprintln s!"{input}: {message}"
            pure 1
      catch error =>
        IO.eprintln error.toString
        pure 1
  | _ =>
      IO.eprintln usage
      pure 2
