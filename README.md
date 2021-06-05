## `glcim` - Gitlab CI monitor for the terminal

Highlights:

* Explore pipelines and their jobs using an interactive terminal interface.
* Automatically expand job list from child pipelines.
* Tail update of job execution in terminal.
* Show difference in job status between two pipelines.


### Installation

Install after Rust toolchain with `cargo install --path .`

See [example config](example-config.toml) in the main directory.


### Configuration

While `glcim` can work as a standalone program, it can also use the current Git
branch as default input to commands.

Following configuration of the Git aliases:

```
git config --local alias.pipelines '!glcim from-alias pipelines'
git config --local alias.job '!glcim from-alias job'
```


This can be used as such:

* `git pipelines` - Pipelines for the current branch.
* `git pipelines (-a/--all)` - Your pipelines on all your branches.
* `git pipelines (-e/--everyone)` - All users' pipelines (implicit `--all`).
* `git jobs` - Jump to jobs for the most recent pipeline on the current branch.


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
