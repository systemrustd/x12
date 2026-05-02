# Yserver

## instructions for all agents
* Read AGENTS.md

## specific instructions for claude code
* use codex command for all reviews as you are burning too many tokens

## environment
* you are most likely running in a bwrap sandbox, if you see /home/jos/realhome, you are.
* the project dir is rw mounted
* in /home/jos/Projects/xserver/hw/kdrive/ephyr/ you can find the source to Xephyr for reference
* in /home/jos/Projects/xserver/hw/xnest/ you can find the sources to Xnest for reference
* to test you can run ynest with RUST_LOG=debug, capture its output
* x11trace is available if you want to trace how Xephyr/Xnest does things, x11trace always needs -n flag
* to push, apparently you need `ssh -F`


