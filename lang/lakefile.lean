import Lake
open Lake DSL

package omarLang

lean_lib Omar

@[default_target]
lean_exe omarc where
  root := `Main

@[test_driver]
lean_exe omarLangTests where
  root := `Tests
