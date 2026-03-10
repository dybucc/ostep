set quiet := true

alias b := build

cargo := require("cargo")
cd := require("cd")

[default]
[private]
default:
    {{ just_executable() }} --list --unsorted --justfile {{ justfile() }}

build project release="false":
    {{ cd }} {{ justfile_directory() / project }}
    {{ cargo }} build {{ if release != "false" { "-r" } else { "" } }}
