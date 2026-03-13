set quiet := true
set ignore-comments := true

alias b := build
alias r := run
alias c := clean

cargo := require("cargo")

[default]
[private]
default:
    {{ just_executable() }} --list --unsorted --justfile {{ justfile() }}

# builds a given project in the workspace in either release mode or not
[arg("project", pattern='[[:alpha:]]+')]
[arg("release", pattern='0|1')]
build project release="0":
    {{ cargo }} build -p {{ project }} {{ if release != "0" { "-r" } else { "" } }} --target-dir={{ invocation_directory() / project }}

# builds the passed project and runs it in the invocation directory
[arg("binary", pattern='[[:alpha:]]+')]
[arg("project", pattern='[[:alpha:]]+')]
[arg("release", pattern='0|1')]
[no-cd]
run project binary release="0": (build project release)
    #!/usr/bin/env sh
    set -uo pipefail
    cp {{ if release != "0" { invocation_directory() / project / "release" / project } else { invocation_directory() / project / "debug" / project } }} {{ invocation_directory() / "harness" }}
    ./{{ binary }}

[arg("binary", pattern='[[:alpha:]]+')]
[arg("project", pattern='[[:alpha:]]+')]
[no-cd]
clean project binary:
    -rm -rf {{ invocation_directory() / project }} {{ invocation_directory() / binary }}
