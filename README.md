## `glcim` - Gitlab CI monitor for the terminal

Highlights:

* Explore pipelines and their jobs using an interactive terminal interface.
* Automatically expand job list from child pipelines.
* Tail update of job execution in terminal.
* Show difference in job status between two pipelines.

### Installation

Install after Rust toolchain with `cargo install --path .`

See [example config](example-config.toml) in the main directory.

## Command line

```
glcim 0.1.0

USAGE:
    glcim [FLAGS] [OPTIONS] <SUBCOMMAND>

FLAGS:
    -d               Request debug mode - no TUI
    -S               Disable auto-refresh (reloading server data)
    -h, --help       Prints help information
    -n               Non-interactive mode - print data and exit
    -V, --version    Prints version information

OPTIONS:
    -c <config-file>

SUBCOMMANDS:
    help         Prints this message or the help of the given subcommand(s)
    jobs         Show all jobs related to a given pipeline
    pipe-diff    Show difference between two pipes
    pipelines    Show all pipelines for a project, or specific to current user
```
