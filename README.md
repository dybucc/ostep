# `ostep`

This repo contains solutions to labs/homework in the book
_Operating Systems: Three Easy Pieces_.

Right now, it only contains the first of the basic UNIX utilities proposed in
the first lab, and an implementation in Rust of the test harness used in the
OSTEP repo.

Work is now focused on making the harness async, as it relies heavily on I/O
bound filesystem operations and command invocation. Beyond that, finishing the
labs is the priority.
