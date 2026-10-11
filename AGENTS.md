# AGENTS.md

## Language

- Write all code, comments, and documentation in English.

## Comments

- Only comment the non-obvious: a hidden constraint, a workaround for a
  specific bug, or an invariant that would surprise a reader.
- Do not describe what the code does — well-named identifiers already do that.
- Keep comments short: one line is almost always enough.

## API

- Public configuration structs are `#[non_exhaustive]`: build them from
  `Default` and set fields, and mark new ones the same way.

## Untrusted input

- Only descend into the container boxes or elements the parser needs, so
  nesting depth stays bounded.
- Use checked or saturating arithmetic on values read from the input.
- Bound every allocation by a limit in the options struct.
- Cover each bound with a test on a hostile input.

## Git

- Never add AI attribution: no `Co-Authored-By` trailer in commits, no
  "Generated with …" line in pull or merge request descriptions.
- Commit messages: a Conventional Commits title (`feat:`, `fix:`,
  `refactor:`, …) in the imperative mood, then a concise one-paragraph
  body explaining the why. Do not describe what the code does — the diff
  shows that.
- Address review feedback by amending the relevant commit rather than
  adding a separate "address review" commit, so the branch keeps a
  clean, logical history. Only add a new commit when amending is not
  possible (e.g. the commit is already merged).
- To amend a commit other than `HEAD` without an interactive editor:
  `git commit --fixup=<sha>`, then
  `GIT_SEQUENCE_EDITOR=: git rebase -i --autosquash <sha>~1`. When `<sha>`
  is the root commit, use `--root` instead of `<sha>~1`.
- Force-pushing a feature branch after amending is allowed, with
  `git push --force-with-lease` only. Never force-push `main`.
