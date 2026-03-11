set quiet := true

alias b := build
alias r := run

cargo := require("cargo")
rm := require("rm")
cd := require("cd")
cp := require("cp")

[default]
[private]
default:
    {{ just_executable() }} --list --unsorted --justfile {{ justfile() }}

# builds a given project in the workspace in either release mode or not
build project release="0":
    {{ cargo }} build -p {{ project }} {{ if release != "0" { "-r" } else { "" } }}

# builds the passed project and runs it in the invocation directory
run project release="0":
    {{ cargo }} build -p {{ project }} {{ if release != "0" { "-r" } else { "" } }} --target-dir={{ invocation_directory() / project }}
    {{ cp }} {{ if release != "0" { invocation_directory_native() / project / "release" / project } else { invocation_directory() / project / "debug" / project } }} {{ invocation_directory_native() }}
    -{{ cd }} {{ invocation_directory_native() }} && {{ project }}
    # -{{ rm }} -rf {{ invocation_directory_native() / project }}
