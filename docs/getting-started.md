## Getting started

To start a ghciwatch session, you'll need a command to start a GHCi session
(like `cabal repl`) and a set of paths and directories to watch for changes.
For example:

    ghciwatch --command "cabal repl lib:test-dev" \
              --watch src --watch test

Reloads and restarts respond to changed file contents, additions, and removals—not
timestamps. Saving identical bytes, atomically replacing a file with identical
contents, or running `touch` does not trigger work. This also applies to
non-Haskell reload/restart inputs and retries of a failed `--setup-shell` command.
An initial content baseline is captured before launch.

Check out the [examples](cli.md#examples) and [command-line
arguments](cli.md#options) for more information.

Ghciwatch can [run test suites](cli.md#--test-ghci) after reloads, [evaluate
code in comments](cli.md#--enable-eval), [log compiler errors to a
file](cli.md#--error-file), run [startup hooks](cli.md#--before-startup-shell)
like [`hpack`][hpack] to generate `.cabal` files, and more!

[hpack]: https://github.com/sol/hpack
