# Bundled syntax definitions

These two MIT-licensed grammars supplement syntect 5.3's built-in definitions.
They are embedded in the executable; no runtime downloads or grammar directory
are required. All other source viewer languages use syntect's existing bundle.

- `TOML.sublime-syntax`: [jasonwilliams/sublime_toml_highlighting](https://github.com/jasonwilliams/sublime_toml_highlighting),
  revision `f3a8d6eb3c5fb9587ed992e384131b84c9619d6a`. See `TOML-LICENSE`.
- `PowerShell.sublime-syntax`: [bat's converted EditorSyntax grammar](https://github.com/sharkdp/bat/blob/62b439dae330a8c4d053ff7d8526a6e3697000b9/assets/syntaxes/02_Extra/PowerShell.sublime-syntax),
  bat revision `62b439dae330a8c4d053ff7d8526a6e3697000b9`;
  [PowerShell/EditorSyntax](https://github.com/PowerShell/EditorSyntax) revision
  `a08b55bf1146c210f58e844be53c2aa78fd5e610`. See `PowerShell-LICENSE`.
  One compatibility change replaces the regex subroutine `\g<1>` with its
  equivalent `[0-9a-fA-F]` character class in the Unicode escape rule. This
  permits the pure-Rust fancy-regex backend without adding Oniguruma.

Keep the corresponding licenses when distributing these definitions.
